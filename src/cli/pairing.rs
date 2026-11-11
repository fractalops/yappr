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
    eprintln!("Scan the QR code with the Yappr app.");
    eprintln!("On another computer, pass the YAPPR: code to yappr join.");
    eprintln!("The invitation expires at {expires_at}.");
    let interrupted = || -> Result<PairOutcome> {
        pairing::revoke(store.dir(), &invitation.token)?;
        Ok(PairOutcome::Interrupted)
    };
    let writer = invitation
        .ticket
        .parse::<EndpointTicket>()?
        .endpoint_addr()
        .id;
    let mut asked = false;
    let mut input = None;
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
                        peers::set_receiver(store.dir(), &alias, &addr.into())?;
                        if let Some(push) = candidate.push {
                            yappr::push::set(store, candidate.id, Some(push))?;
                        }
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
                "\nPair with {}\nCheck code: {}",
                yappr::proto::printable(&candidate.name),
                pairing::check_code(writer, candidate.id, &invitation.token)
            );
            if io::stdin().is_terminal() {
                eprintln!("Accept if the codes match [y/N]");
                let (answer, receiver) = tokio::sync::oneshot::channel();
                // A terminal read can outlive the invitation. Keep it outside
                // the runtime so it cannot block process shutdown.
                std::thread::Builder::new().spawn(move || {
                    let mut line = String::new();
                    let result = io::stdin().read_line(&mut line).map(|_| line);
                    let _ = answer.send(result);
                })?;
                input = Some((candidate.id, receiver));
            } else {
                eprintln!("Compare the check code with the device.");
                eprintln!("yappr pair --accept {}", candidate.id);
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(300)) => {},
            answer = async {
                let (id, receiver) = input.as_mut().expect("a terminal prompt is open");
                (*id, receiver.await)
            }, if input.is_some() => {
                input = None;
                let (id, line) = answer;
                let line = line??;
                let accept = matches!(line.trim().to_lowercase().as_str(), "y" | "yes");
                if let Err(error) = pairing::confirm(store, id, accept) {
                    if Timestamp::now() < expires_at
                        && pairing::get(store.dir(), &invitation.token)?.status == "pending"
                    {
                        return Err(error);
                    }
                }
            },
            _ = tokio::signal::ctrl_c() => return interrupted(),
        }
    }
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
            let reply = client
                .exchange(
                    ticket.endpoint_addr().clone(),
                    pairing::ALPN,
                    &pairing::Join {
                        token: invitation.token.clone(),
                        name: name.clone(),
                        ticket: None,
                        push: None,
                    },
                )
                .await;
            let reply: pairing::PairReply = match reply {
                Ok(reply) => reply,
                Err(e)
                    if e.downcast_ref::<choices::ChoiceError>()
                        .is_some_and(|e| e.code == "denied") =>
                {
                    bail!(
                        "the writer has no open invitation. Create a new invitation on the writer"
                    );
                }
                // The writer can restart after it saves the decision. Wait
                // for the result even if the old connection fails.
                Err(e) if waiting.is_some() => {
                    tracing::debug!(%e, "pairing request failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                Err(e) => return Err(e),
            };
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
                    // Print the message once, not on each ask. The check code
                    // shows only after this device's own claim.
                    if waiting.as_ref() != Some(&reply.message) {
                        eprintln!("{}", yappr::proto::printable(&reply.message));
                        eprintln!(
                            "Check code: {}",
                            pairing::check_code(
                                ticket.endpoint_addr().id,
                                endpoint.id(),
                                &invitation.token
                            )
                        );
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
