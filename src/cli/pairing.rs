//! Pairing invitations, device confirmation, and joining a writer.

use std::{
    io::{self, IsTerminal, Write},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use jiff::Timestamp;
use yappr::{
    choices, control::ControlClient, disk, net, pairing, peers, store::Store, EndpointTicket,
};

use super::{
    args::{CommonArgs, PairArgs},
    context::hostname,
    qr::print_qr,
};

pub(super) async fn pair(common: &CommonArgs, args: PairArgs) -> Result<()> {
    let store = common.store()?;
    if let Some(alias) = &args.alias {
        peers::validate_alias(alias)?;
    }
    if let Some(target) = args.accept {
        let id = peers::resolve(store.dir(), &target)?.id;
        if let Some(alias) = &args.alias {
            peers::set(store.dir(), alias, &id.to_string())?;
        }
        return pairing::confirm(&store, id, true);
    }
    if let Some(target) = args.reject {
        return pairing::confirm(&store, peers::resolve(store.dir(), &target)?.id, false);
    }
    let ticket: Option<String> = disk::read(&store.dir().join("endpoint.json"))?;
    let ticket = ticket.context("run \"yappr start\" before you create an invitation")?;
    let name = args.name.unwrap_or_else(hostname);
    match pairing_session(common, &store, ticket, name, args.expires, args.alias).await? {
        PairOutcome::Expired => bail!("the pairing invitation expired"),
        _ => Ok(()),
    }
}

/// How a pairing session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PairOutcome {
    Accepted,
    Rejected,
    Expired,
    /// Ctrl-C. The invitation is revoked.
    Interrupted,
}

/// Create an invitation, show it as text and as a QR code, and wait for one
/// device. A terminal asks to accept the device. Without a terminal, the
/// writer accepts with "pair --accept".
pub(super) async fn pairing_session(
    common: &CommonArgs,
    store: &Store,
    ticket: String,
    name: String,
    lifetime: Duration,
    alias: Option<String>,
) -> Result<PairOutcome> {
    let expires_at = Timestamp::now().checked_add(jiff::SignedDuration::try_from(lifetime)?)?;
    let invitation = pairing::create(store, ticket, name, expires_at)?;
    let text = invitation.encode()?;
    println!("{text}");
    io::stdout().flush()?;
    print_qr(&text)?;
    eprintln!("Scan the code with the Yappr app, or on another computer run:");
    eprintln!("yappr join {text}");
    eprintln!("The invitation expires at {expires_at}.");
    let interrupted = || -> Result<PairOutcome> {
        pairing::revoke(store.dir(), &invitation.token)?;
        Ok(PairOutcome::Interrupted)
    };
    let mut asked = false;
    loop {
        let offer = pairing::get(store.dir(), &invitation.token)?;
        match offer.status.as_str() {
            "accepted" => {
                let candidate = offer.candidate.context("the accepted device is missing")?;
                let alias = match &alias {
                    Some(alias) => alias.clone(),
                    None => peers::alias_for(store.dir(), &candidate.name, candidate.id)?,
                };
                match &candidate.ticket {
                    Some(ticket) => {
                        // Keep the device's id and the relays of this machine
                        // only, so that the device cannot choose hosts for
                        // this machine to contact.
                        let ticket: EndpointTicket = ticket.parse()?;
                        let known = common.net(None)?.relays.relay_urls()?;
                        let addr = net::known_relays_only(ticket.endpoint_addr(), &known);
                        peers::set_receiver(store.dir(), &alias, &addr.into())?
                    }
                    None => peers::set(store.dir(), &alias, &candidate.id.to_string())?,
                }
                eprintln!("Paired. The device is \"{alias}\" on this machine.");
                return Ok(PairOutcome::Accepted);
            }
            "rejected" => {
                eprintln!("Pairing rejected.");
                return Ok(PairOutcome::Rejected);
            }
            _ => {}
        }
        if Timestamp::now() >= expires_at {
            pairing::revoke(store.dir(), &invitation.token)?;
            return Ok(PairOutcome::Expired);
        }
        if let (Some(candidate), false) = (offer.candidate, asked) {
            asked = true;
            eprintln!(
                "Device: {}\nID: {}",
                yappr::proto::printable(&candidate.name),
                candidate.id
            );
            if io::stdin().is_terminal() {
                eprintln!("Allow this device? [y/N]");
                let answer = tokio::select! {
                    line = read_stdin_line() => line?,
                    _ = tokio::signal::ctrl_c() => return interrupted(),
                };
                let accept = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
                // "pair --accept" or "pair --reject" in another terminal can
                // answer first. The next check reports that answer.
                if let Err(error) = pairing::confirm(store, candidate.id, accept) {
                    if pairing::get(store.dir(), &invitation.token)?.status == "pending" {
                        return Err(error);
                    }
                }
                continue;
            }
            eprintln!(
                "Confirm this device in another terminal: yappr pair --accept {}",
                candidate.id
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(300)) => {},
            _ = tokio::signal::ctrl_c() => return interrupted(),
        }
    }
}

/// One line from stdin, read on a blocking thread so Ctrl-C still works.
async fn read_stdin_line() -> Result<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        anyhow::Ok(line)
    })
    .await?
}

pub(super) async fn join(
    common: &CommonArgs,
    text: &str,
    name: String,
    alias: Option<String>,
) -> Result<()> {
    if let Some(alias) = &alias {
        peers::validate_alias(alias)?;
    }
    let invitation = pairing::Invitation::decode(text)?;
    let ticket: EndpointTicket = invitation.ticket.parse()?;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let client = ControlClient::new(endpoint.clone());
    let result = async {
        let mut waiting = None;
        loop {
            // The writer answers a claimed invitation for a short time after
            // its deadline, so keep asking until that window ends.
            let last_ask = invitation
                .expires_at
                .checked_add(pairing::RESULT_WINDOW)
                .unwrap_or(invitation.expires_at);
            if last_ask <= Timestamp::now() {
                bail!("the invitation expired. Create a new invitation on the writer");
            }
            let reply: pairing::PairReply = client
                .exchange(
                    ticket.endpoint_addr().clone(),
                    pairing::ALPN,
                    &pairing::Join {
                        token: invitation.token.clone(),
                        name: name.clone(),
                        ticket: None,
                    },
                )
                .await
                .map_err(|e| match e.downcast_ref::<choices::ChoiceError>() {
                    // The writer refuses unknown devices when no invitation is open.
                    Some(c) if c.code == "denied" => anyhow::anyhow!(
                        "the writer has no open invitation. Create a new invitation on the writer"
                    ),
                    _ => e,
                })?;
            match reply.status.as_str() {
                "accepted" => {
                    if let Some(alias) = &alias {
                        peers::set(&common.dir()?, alias, &invitation.ticket)?;
                    }
                    println!("{}", invitation.ticket);
                    return Ok(());
                }
                "rejected" => bail!("{}", yappr::proto::printable(&reply.message)),
                _ => {
                    // Print the message once, not on each ask.
                    if waiting.as_ref() != Some(&reply.message) {
                        eprintln!("{}", yappr::proto::printable(&reply.message));
                        waiting = Some(reply.message);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    .await;
    client.close().await;
    endpoint.close().await;
    result
}
