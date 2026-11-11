//! Device access, keys, status, and connection health.

use std::io::{self, IsTerminal};

use anyhow::{bail, Context, Result};
use yappr::{
    choices,
    client::{Denied, Event, Follower},
    net, pairing, peers,
    proto::Request,
};

use super::{
    args::{CommonArgs, KeyArgs, KeyOutput},
    qr::print_qr,
    Usage,
};

/// Connection health for people. A failure gives the state only, because the
/// error goes to stderr.
fn health_line(target: &str, health: &yappr::health::Health) -> String {
    let mut line = format!("{target}: {}", health.state);
    if let Some(route) = &health.route {
        line.push_str(&format!(" via {route}"));
    }
    if let Some(address) = &health.address {
        let address = address.strip_prefix("relay:").unwrap_or(address);
        line.push_str(&format!(" ({address})"));
    }
    if let Some(rtt) = health.rtt_ms {
        line.push_str(&format!(", round trip {rtt:.0} ms"));
    }
    line
}

pub(super) async fn probe(common: &CommonArgs, target: &str, text: bool) -> Result<()> {
    let addr = peers::resolve(&common.dir()?, target)?;
    let writer = addr.id;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let mut follower = Follower::new(
        endpoint.clone(),
        addr,
        Request {
            since: u64::MAX,
            follow: false,
            ..Default::default()
        },
    );
    let result = async {
        while let Some(event) = follower.next().await? {
            if matches!(event, Event::CaughtUp { .. }) {
                let health = yappr::health::snapshot(
                    &follower
                        .connection()
                        .context("the writer closed its connection")?,
                );
                if text {
                    println!("{}", health_line(target, &health));
                } else {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &serde_json::json!({"writer": writer, "alias": target, "health": health})
                        )?
                    );
                }
                return Ok(());
            }
        }
        bail!("the writer closed without reporting its feed state")
    }
    .await;
    if let Err(error) = &result {
        let health = yappr::health::Health::failure(
            if error.downcast_ref::<Denied>().is_some() {
                "denied"
            } else {
                "unreachable"
            },
            format!("{error:#}"),
            None,
        );
        if text {
            println!("{}", health_line(target, &health));
        } else {
            println!(
                "{}",
                serde_json::to_string(
                    &serde_json::json!({"writer": writer, "alias": target, "health": health})
                )?
            );
        }
    }
    endpoint.close().await;
    result
}

pub(super) fn allow(
    common: &CommonArgs,
    target: String,
    alias: Option<String>,
    receives: bool,
) -> Result<()> {
    let dir = common.dir()?;
    if let Some(alias) = &alias {
        peers::set(&dir, alias, &target)?;
    }
    if receives {
        let alias = alias
            .or_else(|| {
                peers::validate_alias(&target)
                    .is_ok()
                    .then(|| target.clone())
            })
            .ok_or_else(|| Usage("give --alias NAME with --receives".into()))?;
        let ticket = peers::list(&dir)?
            .remove(&alias)
            .and_then(|peer| peer.ticket)
            .with_context(|| format!("{alias} has no address. Give its address (ticket)"))?;
        peers::set_receiver(&dir, &alias, &ticket.parse()?)?;
    }
    let id = peers::resolve(&dir, &target)?.id;
    let list = common.store()?.allow_list();
    if list.allow(id)? {
        eprintln!("allowed {id}");
    } else {
        eprintln!("{id} is already allowed");
    }
    Ok(())
}

/// Create a key pair for a machine such as a CI job, and print what the
/// machine and the app need.
pub(super) fn key(common: &CommonArgs, args: KeyArgs) -> Result<()> {
    let secret = iroh::SecretKey::generate();
    let code = pairing::MachineCode {
        id: secret.public(),
        name: args.name.clone(),
    }
    .encode()
    .map_err(|e| Usage(format!("{e:#}")))?;
    let private = data_encoding::HEXLOWER.encode(&secret.to_bytes());
    let device = match &args.device {
        Some(alias) => {
            let peer = peers::list(&common.dir()?)?
                .remove(alias)
                .with_context(|| format!("unknown device alias {alias:?}. Pair it first"))?;
            let ticket = peer.ticket.with_context(|| {
                format!("this machine has no address for {alias}. Pair it first")
            })?;
            Some((alias.clone(), ticket))
        }
        None => None,
    };
    // The variable for the address of the device: phone gives YAPPR_PHONE.
    let var = |alias: &str| format!("YAPPR_{}", alias.to_ascii_uppercase().replace('-', "_"));
    match args.output {
        KeyOutput::Text => {
            println!(
                "Private key. Keep it secret, and store it in your CI as YAPPR_KEY:\n{private}\n"
            );
            println!("Public key:\n{}\n", secret.public());
            if let Some((alias, ticket)) = &device {
                println!(
                    "Address of {alias}. Store it in your CI as {}:\n{ticket}\n",
                    var(alias)
                );
            }
            println!("Machine code:\n{code}");
        }
        KeyOutput::Env => {
            println!("YAPPR_KEY={private}");
            if let Some((alias, ticket)) = &device {
                println!("{}={ticket}", var(alias));
            }
        }
        KeyOutput::Json => {
            let device = device
                .as_ref()
                .map(|(alias, ticket)| serde_json::json!({"alias": alias, "address": ticket}));
            println!(
                "{}",
                serde_json::json!({
                    "private_key": private,
                    "public_key": secret.public(),
                    "name": args.name,
                    "machine_code": code,
                    "device": device,
                })
            );
        }
    }
    eprintln!(
        "To allow {:?}, scan this code in the Yappr app: Setup, then Scan QR.",
        yappr::proto::printable(&args.name)
    );
    print_qr(&code)
}

pub(super) fn deny(common: &CommonArgs, target: String) -> Result<()> {
    let id = peers::resolve(&common.dir()?, &target)?.id;
    let store = common.store()?;
    let denied = store.allow_list().deny(&id)?;
    yappr::push::clear(&store, id)?;
    if denied {
        eprintln!("denied {id}. A running start closes its feed within 5 seconds");
    } else {
        eprintln!("{id} was not allowed");
    }
    Ok(())
}

/// This machine at a glance: identity, feed, peers, questions, and outbox.
pub(super) fn status(common: &CommonArgs, text: bool) -> Result<()> {
    let store = common.store()?;
    let id = common.secret()?.public();
    let allowed = store.allow_list().load()?;
    let mut peers = Vec::new();
    let mut named = std::collections::BTreeSet::new();
    for (alias, peer) in peers::list(store.dir())? {
        let reads = allowed.contains(&peer.id);
        peers.push((Some(alias), peer.id, reads, reads && peer.receives));
        named.insert(peer.id);
    }
    peers.extend(
        allowed
            .iter()
            .filter(|id| !named.contains(*id))
            .map(|id| (None, *id, true, false)),
    );
    let mut questions = std::collections::BTreeMap::<&str, u64>::new();
    for state in choices::list(&store.entries()?) {
        *questions.entry(state.status.as_str()).or_default() += 1;
    }
    let queued = yappr::outbox::list(store.dir())?
        .iter()
        .filter(|d| d.status == "queued")
        .count();
    if !text {
        let peers: Vec<_> = peers
            .iter()
            .map(|(alias, id, reads, receives)| {
                serde_json::json!({"alias": alias, "id": id, "reads": reads, "receives": receives})
            })
            .collect();
        let json = serde_json::json!({
            "endpoint": id,
            "feed": {"dir": store.dir(), "entries": store.last_id()?},
            "peers": peers,
            "questions": questions,
            "outbox": {"queued": queued},
        });
        println!("{json}");
        return Ok(());
    }
    println!("endpoint   {id}");
    println!(
        "feed       {} ({} entries)",
        store.dir().display(),
        store.last_id()?
    );
    println!();
    if peers.is_empty() {
        println!("peers      none. Run \"yappr start\" to pair a device.");
    } else {
        println!("peers                            reads this feed  receives entries");
        let yes = |b: bool| if b { "yes" } else { "no" };
        for (alias, id, reads, receives) in &peers {
            let alias = alias.as_deref().unwrap_or("-");
            println!(
                "  {alias:<16} {:<14} {:<16} {}",
                id.fmt_short(),
                yes(*reads),
                yes(*receives)
            );
        }
    }
    println!();
    let counts: Vec<String> = questions
        .iter()
        .map(|(status, n)| format!("{n} {status}"))
        .collect();
    match counts.is_empty() {
        true => println!("questions  none"),
        false => println!("questions  {}", counts.join(", ")),
    }
    match queued {
        0 => println!("outbox     no answers wait"),
        1 => println!("outbox     1 answer waits. Run \"yappr retry\"."),
        n => println!("outbox     {n} answers wait. Run \"yappr retry\"."),
    }
    Ok(())
}

/// Aliases and allowed IDs in one list. A terminal gets a header and short
/// IDs. A pipe gets full IDs, one tab-separated device per line.
pub(super) fn devices(common: &CommonArgs) -> Result<()> {
    let allowed = common.store()?.allow_list().load()?;
    let mut rows = Vec::new();
    let mut named = std::collections::BTreeSet::new();
    for (alias, peer) in peers::list(&common.dir()?)? {
        // Several aliases can name one device. Each row reports its access.
        rows.push((alias, peer.id, allowed.contains(&peer.id)));
        named.insert(peer.id);
    }
    let unnamed = allowed.iter().filter(|id| !named.contains(*id));
    rows.extend(unnamed.map(|id| ("-".to_string(), *id, true)));
    let terminal = io::stdout().is_terminal();
    if terminal && !rows.is_empty() {
        println!("{:<16} {:<10} READS THIS FEED", "ALIAS", "ID");
    }
    for (alias, id, reads) in rows {
        let reads = if reads { "yes" } else { "no" };
        if terminal {
            println!("{alias:<16} {:<10} {reads}", id.fmt_short());
        } else {
            println!("{alias}\t{id}\t{reads}");
        }
    }
    Ok(())
}
