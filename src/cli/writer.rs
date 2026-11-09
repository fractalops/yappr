//! Posting, serving, and delivering the local feed.

use std::{
    io::{self, IsTerminal, Read},
    time::Duration,
};

use anyhow::{Context, Result};
use iroh::EndpointAddr;
use tokio::time::timeout;
use yappr::{
    choices,
    client::Unreachable,
    control::ControlClient,
    deliver::{self, Hello},
    disk,
    entry::{self, Draft, Invalid},
    ingest,
    net::{self, Network},
    outbox, peers,
    proto::Stored,
    server::{Feed, Server},
    store::Store,
    EndpointTicket,
};

use super::{
    args::{CommonArgs, PostArgs, StartArgs},
    context::{hostname, short_ticket},
    pairing::{pairing_session, PairOutcome},
    shown, ONLINE_TIMEOUT,
};

pub(super) async fn post(common: &CommonArgs, mut args: PostArgs) -> Result<()> {
    // Before levels, the kind was the level.
    if entry::LEVELS[1..].contains(&args.kind.as_str()) && args.details.level.is_none() {
        eprintln!(
            "Use --level {0} in place of --kind {0}. This entry gets level {0}.",
            args.kind
        );
        args.details.level = Some(std::mem::replace(
            &mut args.kind,
            entry::DEFAULT_KIND.into(),
        ));
    }
    let details = args.details.into_details();
    if args.json {
        // Install the handler before anything else, so that Ctrl-C stops
        // between two entries and not in the middle of one.
        let stop = ctrl_c_now()?;
        let store = common.store()?;
        let defaults = Draft {
            source: args.source.unwrap_or_else(hostname),
            kind: args.kind,
            level: details.level,
            subject: details.subject,
            title: args.title,
            body: String::new(),
            format: details.format,
            data: details.data,
            extra: Default::default(),
        };
        let ingested = ingest::run(
            &store,
            &defaults,
            io::stdin(),
            &mut io::stdout(),
            &mut io::stderr(),
            stop,
        )
        .await;
        let delivered = deliver_now(common, &store, store.last_id()?).await;
        return ingested.and(delivered);
    }
    let store = common.store()?;
    let body = match args.body {
        Some(body) => body,
        // a person at a terminal who gives only --title
        None if io::stdin().is_terminal() => String::new(),
        None => read_body(io::stdin().lock())?,
    };
    let source = args.source.unwrap_or_else(hostname);
    let draft = Draft {
        source,
        kind: args.kind,
        level: details.level,
        subject: details.subject,
        title: args.title,
        body,
        format: details.format,
        data: details.data,
        extra: Default::default(),
    };
    let entry = store.append(draft)?;
    println!("{}", entry.id);
    deliver_now(common, &store, entry.id).await
}

/// The allowed devices that receive this feed, when this process must
/// deliver to them. A running writer delivers by itself, so then the list is
/// empty.
pub(super) fn own_receivers(store: &Store) -> Result<Vec<(String, EndpointAddr)>> {
    if disk::try_lock(store.dir(), SERVE_LOCK)?.is_none() {
        return Ok(Vec::new());
    }
    let allowed = store.allow_list().load()?;
    Ok(peers::receivers(store.dir())?
        .into_iter()
        .filter(|(_, addr)| allowed.contains(&addr.id))
        .collect())
}

/// How long a command waits for the devices to save a new entry.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// Deliver the feed up to entry `through` to each device that receives it,
/// all at the same time.
pub(super) async fn deliver_now(common: &CommonArgs, store: &Store, through: u64) -> Result<()> {
    let receivers = own_receivers(store)?;
    if receivers.is_empty() || through == 0 {
        return Ok(());
    }
    let secret = common.secret()?;
    let endpoint = net::bind(iroh::SecretKey::generate(), common.net(None)?, vec![], None).await?;
    let feed = Feed::open(store.clone()).await?;
    let hello = Hello::granted(&secret, endpoint.id());
    let mut tasks = tokio::task::JoinSet::new();
    for (alias, addr) in receivers {
        let (endpoint, feed, hello) = (endpoint.clone(), feed.clone(), hello.clone());
        tasks.spawn(async move {
            let progress = tokio::sync::watch::channel(Stored::default()).0;
            let delivered = timeout(
                DELIVERY_TIMEOUT,
                deliver::deliver(&endpoint, &feed, addr, &hello, Some(through), &progress),
            )
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "no confirmation in {}s",
                    DELIVERY_TIMEOUT.as_secs()
                ))
            });
            (alias, delivered)
        });
    }
    let mut missed = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (alias, delivered) = joined?;
        if let Err(error) = delivered {
            eprintln!("{alias} did not confirm entry {through}: {}", shown(&error));
            missed.push(alias);
        }
    }
    endpoint.close().await;
    if missed.is_empty() {
        return Ok(());
    }
    Err(Undelivered { devices: missed }.into())
}

/// A device that receives the feed did not confirm a new entry.
#[derive(Debug)]
pub(super) struct Undelivered {
    devices: Vec<String>,
}

impl std::fmt::Display for Undelivered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the entry is saved, but {} did not confirm it. \"yappr start\" or \"yappr retry\" delivers it later",
            self.devices.join(", ")
        )
    }
}

impl std::error::Error for Undelivered {}

/// A future that completes on Ctrl-C. The handler is active when this
/// returns, not only when the future is first polled.
fn ctrl_c_now() -> Result<impl std::future::Future<Output = ()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt())?;
        Ok(async move {
            sigint.recv().await;
        })
    }
    #[cfg(not(unix))]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
        Ok(async move {
            ctrl_c.recv().await;
        })
    }
}

/// Read at most one byte more than the limit, so a large input fails fast.
fn read_body(input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .take(entry::MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read stdin")?;
    if bytes.len() > entry::MAX_BODY {
        return Err(Invalid::TooLong {
            field: "body",
            len: bytes.len(),
            max: entry::MAX_BODY,
        })
        .context("stdin is too large");
    }
    String::from_utf8(bytes).map_err(|e| {
        Invalid::NotUtf8 {
            field: "body",
            offset: e.utf8_error().valid_up_to(),
        }
        .into()
    })
}

/// The lock that a running writer holds. Other commands then leave delivery
/// to the writer.
const SERVE_LOCK: &str = "serve.lock";

/// Start the server, save its ticket, and print how to read it. With
/// `pair_hint`, a feed that no device can read gets a hint to run pair. The
/// returned lock shows other commands that the writer runs.
async fn start_serving(
    common: &CommonArgs,
    port: Option<u16>,
    pair_hint: bool,
) -> Result<(Store, Server, EndpointTicket, std::fs::File)> {
    let store = common.store()?;
    let lock = disk::try_lock(store.dir(), SERVE_LOCK)?
        .with_context(|| format!("yappr start already runs for {}", store.dir().display()))?;
    // A failed write can leave a pairing acceptance unfinished.
    if let Err(e) = yappr::pairing::recover(&store) {
        eprintln!("Warning: cannot finish an earlier pairing: {e:#}");
    }
    let opts = common.net(port)?;
    let wait_for_relay = opts.network == Network::Default && !opts.relays.disabled;
    let server = Server::spawn(store.clone(), common.secret()?, opts).await?;
    if wait_for_relay
        && timeout(ONLINE_TIMEOUT, server.endpoint().online())
            .await
            .is_err()
    {
        eprintln!("Warning: cannot connect to the home relay");
    }
    let addr = server.addr().await;
    disk::write(
        &store.dir().join("endpoint.json"),
        &Some(EndpointTicket::new(addr.clone()).to_string()),
    )?;
    let short = short_ticket(&addr);
    let ticket = EndpointTicket::new(addr);
    let allowed = store.allow_list().load()?.len();

    // Status goes to stderr. The tests read the first "yappr read" line.
    eprintln!(
        "Serving {} ({} entries, {allowed} allowed devices).",
        store.dir().display(),
        store.last_id()?
    );
    if allowed == 0 && pair_hint {
        eprintln!(
            "No device can read this feed yet. To pair a device, run \"yappr pair\" \
             in another terminal."
        );
    }
    eprintln!("To read the feed on another computer, use:\nyappr read {ticket}");
    eprintln!("or, with the endpoint id only:\nyappr read {short}");
    Ok((store, server, ticket, lock))
}

/// When `start` pairs a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pairing {
    /// When no device can read the feed.
    First,
    Always,
    Never,
}

pub(super) async fn start(common: &CommonArgs, args: StartArgs, pairing: Pairing) -> Result<()> {
    if let Some(alias) = &args.alias {
        peers::validate_alias(alias)?;
    }
    let pairing = match pairing {
        Pairing::First if args.pair => Pairing::Always,
        pairing => pairing,
    };
    let (store, server, ticket, _lock) =
        start_serving(common, args.port, pairing == Pairing::Never).await?;
    let first = store.allow_list().load()?.is_empty();
    if pairing == Pairing::Always || (pairing == Pairing::First && first) {
        let name = args.name.unwrap_or_else(hostname);
        let outcome = pairing_session(
            common,
            &store,
            ticket.to_string(),
            name,
            args.expires,
            args.alias,
        )
        .await?;
        match outcome {
            PairOutcome::Interrupted => {
                server.shutdown().await;
                return Ok(());
            }
            PairOutcome::Expired => {
                eprintln!("The invitation expired. To pair a device, run \"yappr pair\".")
            }
            PairOutcome::Accepted | PairOutcome::Rejected => {}
        }
        eprintln!("Serving. Press Ctrl-C to stop.");
    }
    tokio::signal::ctrl_c().await?;
    eprintln!("got ctrl-c, exiting");
    server.shutdown().await;
    Ok(())
}

pub(super) async fn retry(common: &CommonArgs) -> Result<()> {
    let dir = common.dir()?;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let client = ControlClient::new(endpoint.clone());
    let mut failure = None;
    for delivery in outbox::list(&dir)?
        .into_iter()
        .filter(|d| d.status == "queued")
    {
        match outbox::deliver_with(&dir, &client, &delivery).await {
            Ok(decision) => println!("{}", serde_json::to_string(&decision)?),
            Err(error) => {
                eprintln!("{}: {}", delivery.submission.question_id, shown(&error));
                // A writer that answered keeps its own code. Any other
                // failure means that the writer did not confirm the choice.
                failure = Some(if error.downcast_ref::<choices::ChoiceError>().is_some() {
                    error
                } else {
                    Unreachable {
                        writer: delivery.writer.parse()?,
                        reason: "the writer did not confirm the choice".into(),
                    }
                    .into()
                });
            }
        }
    }
    client.close().await;
    endpoint.close().await;
    let store = common.store()?;
    let delivered = deliver_now(common, &store, store.last_id()?).await;
    failure.map_or(delivered, Err)
}
