//! Command line arguments.
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use iroh::EndpointAddr;
use jiff::{tz::TimeZone, Timestamp, Zoned};
use regex::Regex;
use tokio::time::timeout;
use yappr::{
    choices::{self, Choice, Question},
    client::{Denied, Event, Follower, Unreachable},
    control::ControlClient,
    cursor::Cursor,
    disk,
    entry::{self, Draft, Entry, Invalid},
    ingest::{self, Rejected},
    net::{self, NetOptions, Network},
    outbox, pairing, peers,
    proto::Request,
    render::{self, ColorChoice, Output, Renderer},
    server::Server,
    store::Store,
    EndpointTicket,
};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Private feeds, questions, and choices between your devices.
///
/// One machine writes entries. Your other devices follow it over iroh. There
/// is no account or hosted feed service. When a follower
/// connects, it receives the entries that it missed, and then the new entries.
///
/// The identity of this machine is in <feed dir>/secret.key. Set IROH_SECRET to
/// use a different secret key.
#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
    #[clap(flatten)]
    common: CommonArgs,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(clap::Args, Debug)]
struct CommonArgs {
    /// The name of the local feed.
    #[clap(long, global = true, env = "YAPPR_FEED", default_value = "default")]
    feed: String,

    /// The feed directory. This replaces the directory that --feed selects.
    #[clap(long, global = true, env = "YAPPR_DIR", value_name = "PATH")]
    dir: Option<PathBuf>,

    /// Do not find or announce endpoints on the local network (mDNS).
    #[clap(long, global = true)]
    no_mdns: bool,
    /// Use these relay URLs instead of n0 defaults. Repeat for more relays.
    #[clap(long = "relay", global = true, conflicts_with = "no_relay")]
    relays: Vec<String>,
    /// Use direct connections only.
    #[clap(long, global = true, conflicts_with = "relays")]
    no_relay: bool,

    /// "local" uses loopback only, with no relays. For tests.
    #[clap(
        long,
        global = true,
        hide = true,
        env = "YAPPR_NETWORK",
        value_enum,
        default_value_t
    )]
    network: Network,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Add one entry to the local feed and print its id.
    ///
    /// The body comes from stdin, or from --body. The serve command does not
    /// have to run.
    #[clap(after_help = "Examples:
  df -h / | yappr post --source disk --title \"Disk usage\"
  yappr follow laptop --output json | jq -c 'select(.kind == \"alert\")' | yappr post --json")]
    Post(PostArgs),

    /// Serve the feed, pair one device, and keep serving.
    ///
    /// Use it on the writer the first time. Later, start the writer with
    /// serve, and pair more devices with pair.
    #[clap(after_help = "Examples:
  yappr init
  yappr init --name \"Work laptop\"")]
    Init(InitArgs),

    /// Serve the local feed to the allowed followers.
    ///
    /// Prints the ticket on stderr.
    Serve(ServeArgs),

    /// Print the entries of a feed, then wait for new entries.
    ///
    /// Without --since or --tail, the follower starts after the last entry
    /// that it printed before, and it saves its position after each entry.
    #[clap(after_help = "Examples:
  yappr follow laptop
  yappr follow laptop server --output json
  yappr follow laptop --since 2h --no-follow")]
    Follow(FollowArgs),

    /// Let a follower read this feed.
    Allow {
        /// The alias or endpoint ID of the follower.
        id: String,
    },

    /// Stop a follower from reading this feed.
    Deny {
        /// The alias or endpoint ID of the follower.
        id: String,
    },

    /// Print the endpoint ids that can read this feed. `devices` replaces it.
    #[clap(hide = true)]
    Allowed,

    /// Print the endpoint id of this machine.
    Id,

    /// Print the ticket of this feed without serving it.
    Ticket,

    /// Ask a question and wait for one choice.
    ///
    /// Prints the value of the selected choice.
    #[clap(after_help = "Examples:
  yappr ask laptop \"Backup failed. What next?\" --choice retry=\"Retry now\" --choice skip
  yappr ask laptop \"Continue?\" --choice yes --choice no --no-wait")]
    Ask(AskArgs),
    /// Ask for approval. Approval exits with 0, rejection with code 7.
    #[clap(after_help = "Example:
  yappr approve laptop \"Deploy to production?\" && ./deploy")]
    Approve(ApprovalArgs),
    /// Read local questions and their current states.
    Questions {
        /// The output format. The default is text for a terminal and one JSON
        /// state per line for a pipe or a file.
        #[clap(long, value_enum)]
        output: Option<Format>,
    },
    /// Wait for a local question's final decision.
    Wait { id: String },
    /// Cancel a local question.
    Cancel { id: String },
    /// Answer a question from another writer.
    ///
    /// The answer waits in a local outbox until the writer saves it.
    #[clap(after_help = "Example:\n  yappr answer laptop 3f9a... --choice retry")]
    Answer {
        /// The alias, ID, or ticket of the writer that asked.
        #[clap(value_name = "WRITER")]
        ticket: String,
        /// The question ID.
        id: String,
        /// The value of the selected choice.
        #[clap(long = "choice", value_name = "VALUE")]
        value: String,
    },
    /// Print this machine's identity, feed, peers, questions, and outbox.
    ///
    /// "reads this feed" means the peer is in this machine's allow list. It
    /// says nothing about access to that peer's feed.
    Status {
        /// The output format. The default is text for a terminal and JSON for
        /// a pipe or a file.
        #[clap(long, value_enum)]
        output: Option<Format>,
    },
    /// Print aliases, IDs, and access to this feed as tab-separated lines.
    /// `status` replaces it.
    #[clap(hide = true)]
    Devices,
    /// Send queued choices again.
    Retry,
    /// Name devices locally. An alias does not grant access. `devices` lists them.
    Alias {
        #[clap(subcommand)]
        command: AliasCommand,
    },
    /// Read or change persistent relay settings.
    Network {
        #[clap(subcommand)]
        command: NetworkCommand,
    },
    /// Probe a writer and print its connection health.
    Health {
        /// The alias, ID, or ticket of the writer.
        target: String,
        /// The output format. The default is text for a terminal and JSON for
        /// a pipe or a file.
        #[clap(long, value_enum)]
        output: Option<Format>,
    },
    /// Pair a device with an expiring invitation while serve runs.
    #[clap(after_help = "Examples:
  yappr pair
  yappr pair --accept <ID>    (confirm from another terminal)")]
    Pair(PairArgs),
    /// Join a pairing invitation from another writer.
    Join {
        invitation: String,
        #[clap(long, default_value = "Device")]
        name: String,
        #[clap(long)]
        alias: Option<String>,
    },
}

/// The output format of a command that prints states, not entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    /// For people. The layout can change between versions.
    Text,
    /// One JSON object per line.
    Json,
}

impl Format {
    /// The chosen format, or text for a terminal and JSON for a pipe.
    fn is_text(choice: Option<Format>) -> bool {
        choice.unwrap_or(if io::stdout().is_terminal() {
            Format::Text
        } else {
            Format::Json
        }) == Format::Text
    }
}

#[derive(Subcommand, Debug)]
enum AliasCommand {
    Set { name: String, target: String },
    Remove { name: String },
}
#[derive(Subcommand, Debug)]
enum NetworkCommand {
    Show,
    Set {
        #[clap(long)]
        defaults: bool,
    },
}

#[derive(clap::Args, Debug)]
struct QuestionArgs {
    /// The alias or endpoint ID of the device that may answer. --to can
    /// replace it.
    #[clap(value_name = "DEVICE")]
    device: Option<String>,
    /// The question shown on the other device. --title can replace it.
    #[clap(value_name = "QUESTION")]
    question: Option<String>,
    /// The alias or endpoint ID of the device that may answer.
    #[clap(long)]
    to: Option<String>,
    /// The question shown on the other device.
    #[clap(long)]
    title: Option<String>,
    /// Details shown with the question.
    #[clap(long, default_value = "")]
    body: String,
    #[clap(long, default_value = "yappr")]
    source: String,
    /// The time allowed for an answer, such as 10m or 2h.
    #[clap(long, value_parser = parse_interval, default_value = "10m")]
    timeout: Duration,
    /// Print the question ID and return without waiting.
    #[clap(long)]
    no_wait: bool,
}
#[derive(clap::Args, Debug)]
struct AskArgs {
    #[clap(flatten)]
    question: QuestionArgs,
    /// A value and an optional label, such as retry="Retry now". Repeat for
    /// each choice.
    #[clap(long = "choice", value_name = "VALUE[=LABEL]", value_parser = choices::parse_option, num_args = 1)]
    choices: Vec<Choice>,
}

impl QuestionArgs {
    /// The device and the question, from the words or from the flags.
    fn target(&mut self) -> Result<(String, String)> {
        // With --to, the first word is the question.
        let mut words = self.device.take().into_iter().chain(self.question.take());
        let usage = |message: &str| anyhow::Error::from(Usage(message.into()));
        let to = match self.to.take() {
            Some(to) => to,
            None => words.next().ok_or_else(|| {
                usage("name the device that may answer, such as: yappr ask laptop \"Continue?\"")
            })?,
        };
        let title = match self.title.take() {
            Some(title) => title,
            None => words.next().ok_or_else(|| {
                usage("give the question, such as: yappr ask laptop \"Continue?\"")
            })?,
        };
        if let Some(extra) = words.next() {
            return Err(usage(&format!(
                "unexpected argument {extra:?}. Put the question in quotes"
            )));
        }
        Ok((to, title))
    }
}
#[derive(clap::Args, Debug)]
struct ApprovalArgs {
    #[clap(flatten)]
    question: QuestionArgs,
}
#[derive(clap::Args, Debug)]
struct InitArgs {
    /// The writer name shown on the joining device. The default is the host
    /// name.
    #[clap(long)]
    name: Option<String>,
    /// Save the accepted device under this alias. The default comes from the
    /// device's name.
    #[clap(long)]
    alias: Option<String>,
    /// The invitation lifetime.
    #[clap(long, value_parser = parse_interval, default_value = "5m")]
    expires: Duration,
    /// The UDP port for IPv4. See serve --port.
    #[clap(long)]
    port: Option<u16>,
}

#[derive(clap::Args, Debug)]
struct PairArgs {
    /// The writer name shown on the joining device. The default is the host
    /// name.
    #[clap(long)]
    name: Option<String>,
    /// The invitation lifetime.
    #[clap(long, value_parser = parse_interval, default_value = "5m")]
    expires: Duration,
    /// Save the accepted device under this alias. The default comes from the
    /// device's name.
    #[clap(long)]
    alias: Option<String>,
    /// Confirm a pending device by its full endpoint ID.
    #[clap(long, conflicts_with = "reject")]
    accept: Option<String>,
    /// Reject a pending device by its full endpoint ID.
    #[clap(long, conflicts_with = "accept")]
    reject: Option<String>,
}

#[derive(clap::Args, Debug)]
struct ServeArgs {
    /// The UDP port for IPv4. With a fixed port, the ticket stays valid after
    /// a restart, also when no relay is available. The default is a random
    /// port.
    #[clap(long)]
    port: Option<u16>,
}

#[derive(clap::Args, Debug)]
struct PostArgs {
    /// The program or the topic that the entry comes from. The default is the
    /// host name.
    #[clap(long)]
    source: Option<String>,

    /// A one-line summary.
    #[clap(long)]
    title: Option<String>,

    /// The kind of entry. Followers color "warn" and "alert".
    #[clap(long, default_value = "info")]
    kind: String,

    /// The body. If you do not give it, the body comes from stdin.
    #[clap(long)]
    body: Option<String>,

    /// Post each line of stdin as one entry in JSON, the format of "follow
    /// --output json". A field that a line does not have comes from the
    /// options.
    #[clap(long, conflicts_with = "body")]
    json: bool,
}

fn parse_interval(s: &str) -> Result<Duration, String> {
    let d: jiff::SignedDuration = s
        .parse()
        .map_err(|_| "use a duration such as 500ms, 2s or 1m".to_string())?;
    Duration::try_from(d)
        .ok()
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "the duration must be more than zero".into())
}

#[derive(clap::Args, Debug)]
struct FollowArgs {
    /// Writer aliases, endpoint IDs, or feed tickets.
    #[clap(num_args = 1.., required = true, value_name = "WRITER")]
    ticket: Vec<String>,

    /// Start after this entry id, or after this time. The time can be a
    /// duration such as 1h or 2d, or a time such as 2026-10-27T09:30:00Z.
    #[clap(long, value_parser = parse_since, value_name = "ID|DURATION|TIME")]
    since: Option<Since>,

    /// Start with the last N entries.
    #[clap(long, value_name = "N")]
    tail: Option<u64>,

    /// Print only the entries with a source that matches this regex.
    #[clap(long, value_name = "REGEX")]
    source: Option<Regex>,

    /// Print only the entries of this kind. Use the option again for more kinds.
    #[clap(long)]
    kind: Vec<String>,

    /// Do not print the entries of this kind, for example "log". Use the
    /// option again for more kinds.
    #[clap(long, value_name = "KIND")]
    not_kind: Vec<String>,

    /// The output format. The default is text for a terminal and JSON for a
    /// pipe or a file.
    #[clap(long, value_enum)]
    output: Option<Output>,

    /// When to use color.
    #[clap(long, value_enum, default_value_t)]
    color: ColorChoice,

    /// Stop after the last entry. Do not wait for new entries.
    #[clap(long)]
    no_follow: bool,

    /// The name of the saved position. Different names have independent
    /// positions.
    #[clap(long, default_value = "default", value_name = "NAME")]
    cursor: String,
}

#[derive(Debug, Clone, Copy)]
enum Since {
    Id(u64),
    Time(Timestamp),
}

fn parse_since(s: &str) -> Result<Since, String> {
    if let Ok(id) = s.parse() {
        return Ok(Since::Id(id));
    }
    if let Ok(time) = s.parse() {
        return Ok(Since::Time(time));
    }
    if let Ok(span) = s.parse::<jiff::Span>() {
        if let Ok(time) = Zoned::now().checked_sub(span.abs()) {
            return Ok(Since::Time(time.timestamp()));
        }
    }
    Err("use an entry id (42), a duration (1h, 2d) or a time (2026-10-27T09:30:00Z)".into())
}

impl CommonArgs {
    /// The feed directory.
    fn dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        if !entry::is_plain_file_name(&self.feed) || self.feed.starts_with('.') {
            bail!("feed name {:?} is not a plain name", self.feed);
        }
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::data_dir)
            .context("cannot find a data directory. Use --dir")?;
        Ok(base.join("yappr").join(&self.feed))
    }

    fn store(&self) -> Result<Store> {
        Store::open(self.dir()?)
    }

    fn net(&self, port: Option<u16>) -> Result<NetOptions> {
        let relays = if self.no_relay || !self.relays.is_empty() {
            net::RelaySettings {
                disabled: self.no_relay,
                urls: self.relays.clone(),
            }
        } else {
            net::RelaySettings::load(&self.dir()?)?
        };
        relays.validate()?;
        Ok(NetOptions {
            port,
            mdns: !self.no_mdns && self.network == Network::Default,
            relays,
            ..self.network.into()
        })
    }

    fn secret(&self) -> Result<iroh::SecretKey> {
        net::load_or_create_secret(&self.dir()?)
    }
}

async fn post(common: &CommonArgs, args: PostArgs) -> Result<()> {
    if args.json {
        // Install the handler before anything else, so that Ctrl-C stops
        // between two entries and not in the middle of one.
        let stop = ctrl_c_now()?;
        let store = common.store()?;
        let defaults = Draft {
            source: args.source.unwrap_or_else(hostname),
            kind: args.kind,
            title: args.title,
            body: String::new(),
            extra: Default::default(),
        };
        return ingest::run(
            &store,
            &defaults,
            io::stdin(),
            &mut io::stdout(),
            &mut io::stderr(),
            stop,
        )
        .await;
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
        title: args.title,
        body,
        extra: Default::default(),
    };
    let entry = store.append(draft)?;
    println!("{}", entry.id);
    Ok(())
}

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

fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let short = name.split('.').next().unwrap_or_default();
    if short.is_empty() || short.len() > entry::MAX_SOURCE {
        "localhost".into()
    } else {
        short.into()
    }
}

/// A ticket with only the endpoint id and the relay urls. The follower finds
/// the direct addresses with address lookup.
fn short_ticket(addr: &EndpointAddr) -> EndpointTicket {
    let mut short = EndpointAddr::new(addr.id);
    for relay_url in addr.relay_urls() {
        short = short.with_relay_url(relay_url.clone());
    }
    short.into()
}

async fn serve(common: &CommonArgs, args: ServeArgs) -> Result<()> {
    let (_store, server, _ticket) = start_serving(common, args.port, true).await?;
    tokio::signal::ctrl_c().await?;
    eprintln!("got ctrl-c, exiting");
    server.shutdown().await;
    Ok(())
}

/// Start the server, save its ticket, and print how to follow it. With
/// `pair_hint`, a feed that no device can read gets a hint to run pair.
async fn start_serving(
    common: &CommonArgs,
    port: Option<u16>,
    pair_hint: bool,
) -> Result<(Store, Server, EndpointTicket)> {
    let store = common.store()?;
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

    // Status goes to stderr. The tests read the first "yappr follow" line.
    eprintln!(
        "Serving {} ({} entries, {allowed} allowed followers).",
        store.dir().display(),
        store.last_id()?
    );
    if allowed == 0 && pair_hint {
        eprintln!(
            "No follower can read this feed yet. To pair a device, run \"yappr pair\" \
             in another terminal."
        );
    }
    eprintln!("To follow, use:\nyappr follow {ticket}");
    eprintln!("or, with the endpoint id only:\nyappr follow {short}");
    Ok((store, server, ticket))
}

async fn init(common: &CommonArgs, args: InitArgs) -> Result<()> {
    if let Some(alias) = &args.alias {
        peers::validate_alias(alias)?;
    }
    let (store, server, ticket) = start_serving(common, args.port, false).await?;
    let name = args.name.unwrap_or_else(hostname);
    let outcome =
        pairing_session(&store, ticket.to_string(), name, args.expires, args.alias).await?;
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
    eprintln!("Serving. Press Ctrl-C to stop. Next time, start the writer with \"yappr serve\".");
    tokio::signal::ctrl_c().await?;
    eprintln!("got ctrl-c, exiting");
    server.shutdown().await;
    Ok(())
}

/// Prints the entries that pass the filters, and moves the cursor.
struct Printer {
    cursor: Option<Cursor>,
    /// The writer applies the same filter. A writer that does not know the
    /// filter fields sends all entries, so the follower checks them again.
    filter: yappr::proto::Filter,
    renderer: Renderer,
    writer: Option<String>,
}

impl Printer {
    fn wants(&self, entry: &Entry) -> bool {
        self.filter.matches(entry)
    }

    /// Move the cursor to `head`. The writer checked every entry up to there,
    /// also the entries that the filter left out. The cursor never moves back.
    fn passed(&self, head: u64) -> Result<()> {
        if let Some(cursor) = &self.cursor {
            if head > cursor.load()? {
                cursor.save(head)?;
            }
        }
        Ok(())
    }

    /// Print one entry, then move the cursor past it.
    fn print(&self, entry: &Entry) -> Result<()> {
        if self.wants(entry) {
            let mut buf = Vec::new();
            if let Some(writer) = &self.writer {
                if self.renderer.output == Output::Json {
                    serde_json::to_writer(
                        &mut buf,
                        &serde_json::json!({"writer": writer, "entry": entry}),
                    )?;
                    buf.push(b'\n');
                } else {
                    write!(&mut buf, "[{writer}] ")?;
                    self.renderer.render(entry, &mut buf)?;
                }
            } else {
                self.renderer.render(entry, &mut buf)?;
            }
            let mut stdout = io::stdout().lock();
            stdout.write_all(&buf)?;
            stdout.flush()?;
        }
        if let Some(cursor) = &self.cursor {
            cursor.save(entry.id)?;
        }
        Ok(())
    }
}

async fn follow(common: &CommonArgs, args: FollowArgs) -> Result<()> {
    let dir = common.dir()?;
    let mut targets = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for target in &args.ticket {
        let addr = peers::resolve(&dir, target)?;
        if !seen.insert(addr.id) {
            bail!("the writer {} occurs more than once", addr.id);
        }
        let label = if peers::validate_alias(target).is_ok() {
            target.clone()
        } else {
            addr.id.to_string()
        };
        targets.push((label, addr));
    }
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let multiple = targets.len() > 1;
    let output = Output::resolve(args.output, io::stdout().is_terminal());
    let renderer = Renderer::new(
        output,
        output == Output::Text
            && render::use_color(
                args.color,
                io::stdout().is_terminal(),
                std::env::var("NO_COLOR").ok().as_deref(),
            ),
        TimeZone::system(),
    );
    let mut tasks = tokio::task::JoinSet::new();
    for (label, addr) in targets {
        let mut req = Request {
            follow: !args.no_follow,
            tail: args.tail,
            kinds: args.kind.clone(),
            not_kinds: args.not_kind.clone(),
            source: args.source.as_ref().map(|re| re.as_str().to_string()),
            ..Default::default()
        };
        let cursor = match args.since {
            Some(Since::Id(id)) => {
                req.since = id;
                None
            }
            Some(Since::Time(time)) => {
                req.since_time = Some(time);
                None
            }
            None if args.tail.is_some() => None,
            None => {
                let cursor = Cursor::new(&dir, addr.id, &args.cursor)?;
                req.since = cursor.load()?;
                Some(cursor)
            }
        };
        let printer = Printer {
            cursor,
            filter: yappr::proto::Filter::new(&req)?,
            writer: multiple.then(|| label.clone()),
            renderer: renderer.clone(),
        };
        let status = !args.no_follow && io::stderr().is_terminal();
        let mut follower = Follower::new(endpoint.clone(), addr, req);
        tasks.spawn(async move {
            let mut caught_up = false;
            let result = async {
                while let Some(event) = follower.next().await? {
                    match event {
                        Event::Entry(entry) => printer.print(&entry)?,
                        Event::CaughtUp { head } => {
                            printer.passed(head)?;
                            if status && !caught_up {
                                let health =
                                    follower.connection().map(|c| yappr::health::snapshot(&c));
                                let route = health
                                    .as_ref()
                                    .and_then(|h| h.route.as_deref())
                                    .unwrap_or("unknown path");
                                eprintln!("[{label}] caught up at #{head}, connected via {route}");
                            }
                            caught_up = true;
                        }
                        Event::Retrying { delay, error } => {
                            eprintln!(
                                "[{label}] cannot read the feed: {error}. Retry in {}s",
                                delay.as_secs()
                            );
                            caught_up = false;
                        }
                    }
                }
                anyhow::Ok(())
            }
            .await;
            (label, result)
        });
    }
    let mut failure = None;
    while let Some(result) = tasks.join_next().await {
        let (label, result) = result?;
        if let Err(error) = result {
            if is_broken_pipe(&error) {
                tasks.abort_all();
                endpoint.close().await;
                return Err(error);
            }
            if multiple {
                eprintln!("[{label}] stopped: {error:#}");
            }
            failure = Some(error);
        }
    }
    endpoint.close().await;
    failure.map_or(Ok(()), Err)
}

/// One question for people: status, title, the choices or the answer, then
/// the full ID for `wait`, `cancel`, and `answer`.
fn question_line(state: &choices::QuestionState) -> String {
    let detail = match &state.decision {
        Some(decision) => format!("answer: {}", decision.value),
        None => {
            let values: Vec<&str> = state
                .question
                .options
                .iter()
                .map(|o| o.value.as_str())
                .collect();
            format!("choices: {}", values.join(", "))
        }
    };
    format!(
        "{:<9} {}  ({detail})  {}",
        state.status, state.question.title, state.question.id
    )
}

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

async fn probe(common: &CommonArgs, target: &str, text: bool) -> Result<()> {
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

fn allow(common: &CommonArgs, target: String) -> Result<()> {
    let id = peers::resolve(&common.dir()?, &target)?.id;
    let list = common.store()?.allow_list();
    if list.allow(id)? {
        eprintln!("allowed {id}");
    } else {
        eprintln!("{id} is already allowed");
    }
    Ok(())
}

fn deny(common: &CommonArgs, target: String) -> Result<()> {
    let id = peers::resolve(&common.dir()?, &target)?.id;
    let list = common.store()?.allow_list();
    if list.deny(&id)? {
        eprintln!("denied {id}. A running serve closes its feed within 5 seconds");
    } else {
        eprintln!("{id} was not allowed");
    }
    Ok(())
}

/// This machine at a glance: identity, feed, peers, questions, and outbox.
fn status(common: &CommonArgs, text: bool) -> Result<()> {
    let store = common.store()?;
    let id = common.secret()?.public();
    let allowed = store.allow_list().load()?;
    let mut peers = Vec::new();
    let mut named = std::collections::BTreeSet::new();
    for (alias, peer) in peers::list(store.dir())? {
        peers.push((Some(alias), peer.id, allowed.contains(&peer.id)));
        named.insert(peer.id);
    }
    peers.extend(
        allowed
            .iter()
            .filter(|id| !named.contains(*id))
            .map(|id| (None, *id, true)),
    );
    let mut questions = std::collections::BTreeMap::<String, u64>::new();
    for state in choices::list(&store.entries()?) {
        *questions.entry(state.status).or_default() += 1;
    }
    let queued = yappr::outbox::list(store.dir())?
        .iter()
        .filter(|d| d.status == "queued")
        .count();
    if !text {
        let peers: Vec<_> = peers
            .iter()
            .map(|(alias, id, reads)| serde_json::json!({"alias": alias, "id": id, "reads": reads}))
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
        println!("peers      none. Run \"yappr init\" or \"yappr pair\" to pair a device.");
    } else {
        println!("peers                            reads this feed");
        for (alias, id, reads) in &peers {
            let alias = alias.as_deref().unwrap_or("-");
            let reads = if *reads { "yes" } else { "no" };
            println!("  {alias:<16} {:<14} {reads}", id.fmt_short());
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
fn devices(common: &CommonArgs) -> Result<()> {
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

#[derive(Debug)]
struct QuestionOutcome {
    code: i32,
    message: &'static str,
}

/// A command line that clap accepts but the command cannot use. Exits with 2,
/// like a clap usage error.
#[derive(Debug)]
struct Usage(String);
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Usage {}
impl std::fmt::Display for QuestionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for QuestionOutcome {}

async fn ask(
    common: &CommonArgs,
    mut args: QuestionArgs,
    options: Vec<Choice>,
    approval: bool,
) -> Result<()> {
    let (to, title) = args.target()?;
    let store = common.store()?;
    let responder = peers::resolve(store.dir(), &to)?.id;
    if !store.allow_list().contains(&responder) {
        bail!("allow the responder first: yappr allow {to}");
    }
    let expires_at = Timestamp::now().checked_add(jiff::SignedDuration::try_from(args.timeout)?)?;
    let question = choices::new_question(
        title,
        args.body,
        args.source,
        options,
        responder,
        Some(expires_at),
        approval,
    );
    let question: Question = choices::create(&store, question)?;
    if args.no_wait {
        println!("{}", question.id);
        return Ok(());
    }
    eprintln!(
        "Question {}. Waiting for {}.",
        question.id, question.responder
    );
    tokio::select! {
        result = wait(common, &question.id) => result,
        _ = tokio::signal::ctrl_c() => {
            // A decision can reach disk before the cancellation. It remains
            // final, so report it in place of the cancellation.
            let cancelled = choices::cancel(&store, &question.id);
            let state = choices::get(&store.entries()?, &question.id)?;
            match state.status.as_str() {
                "pending" => cancelled?,
                "cancelled" => {}
                _ => if let Some(result) = finish(state) { return result; },
            }
            Err(QuestionOutcome { code: 8, message: "the wait stopped. Use yappr wait to read the final state" }.into())
        }
    }
}

async fn wait(common: &CommonArgs, id: &str) -> Result<()> {
    let store = common.store()?;
    // Read only the entries that arrive after each check.
    let mut reader = store.reader()?;
    let mut tracker = choices::Tracker::one(id);
    loop {
        while let Some(entry) = reader.next_entry()? {
            tracker.push(&entry);
        }
        let state = tracker
            .states()
            .pop()
            .ok_or_else(|| choices::ChoiceError::new("not_found", "the question does not exist"))?;
        match finish(state) {
            Some(result) => return result,
            None => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// The result of a question in a final state, or `None` while it is pending.
fn finish(state: choices::QuestionState) -> Option<Result<()>> {
    let outcome = |code, message| Some(Err(QuestionOutcome { code, message }.into()));
    match state.status.as_str() {
        "answered" => {
            let Some(decision) = state.decision else {
                return Some(Err(anyhow::anyhow!("the decision is missing")));
            };
            println!("{}", decision.value);
            if state.question.approval && decision.value == "reject" {
                outcome(7, "the responder rejected the request")
            } else {
                Some(Ok(()))
            }
        }
        "expired" => outcome(6, "the question expired"),
        "cancelled" => outcome(8, "the question was cancelled"),
        _ => None,
    }
}

async fn answer(
    common: &CommonArgs,
    ticket: EndpointTicket,
    id: String,
    value: String,
) -> Result<()> {
    let dir = common.dir()?;
    let delivery = outbox::queue(&dir, &ticket, id, value)?;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let client = ControlClient::new(endpoint.clone());
    let result = outbox::deliver_with(&dir, &client, &delivery).await;
    client.close().await;
    endpoint.close().await;
    match result {
        Ok(decision) => {
            println!("{}", serde_json::to_string(&decision)?);
            Ok(())
        }
        Err(error) if error.downcast_ref::<choices::ChoiceError>().is_some() => Err(error),
        Err(error) => {
            eprintln!("The choice remains queued. Run yappr retry. {error:#}");
            Err(Unreachable {
                writer: ticket.endpoint_addr().id,
                reason: "the writer did not confirm the choice".into(),
            }
            .into())
        }
    }
}

async fn retry(common: &CommonArgs) -> Result<()> {
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
                eprintln!("{}: {error:#}", delivery.submission.question_id);
                failure = Some(error);
            }
        }
    }
    client.close().await;
    endpoint.close().await;
    failure.map_or(Ok(()), Err)
}

async fn pair(common: &CommonArgs, args: PairArgs) -> Result<()> {
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
    let ticket = ticket.context("start yappr serve before you create an invitation")?;
    let name = args.name.unwrap_or_else(hostname);
    match pairing_session(&store, ticket, name, args.expires, args.alias).await? {
        PairOutcome::Expired => bail!("the pairing invitation expired"),
        _ => Ok(()),
    }
}

/// How a pairing session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairOutcome {
    Accepted,
    Rejected,
    Expired,
    /// Ctrl-C. The invitation is revoked.
    Interrupted,
}

/// Create an invitation, show it as text and as a QR code, and wait for one
/// device. A terminal asks to accept the device. Without a terminal, the
/// writer accepts with "pair --accept".
async fn pairing_session(
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
                peers::set(store.dir(), &alias, &candidate.id.to_string())?;
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
                pairing::confirm(store, candidate.id, accept)?;
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

/// Draw `text` as a QR code on stderr when stderr is a terminal. Each text
/// row holds two module rows in half blocks. Black modules on a white field
/// scan best, so the colors are set and do not follow the terminal theme.
fn print_qr(text: &str) -> Result<()> {
    if !io::stderr().is_terminal() {
        return Ok(());
    }
    let code = qrcode::QrCode::new(text.as_bytes())?;
    let width = code.width() as isize;
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| {
        (0..width).contains(&x)
            && (0..width).contains(&y)
            && colors[(y * width + x) as usize] == qrcode::Color::Dark
    };
    let color = render::use_color(
        ColorChoice::Auto,
        true,
        std::env::var("NO_COLOR").ok().as_deref(),
    );
    // A quiet zone of two modules on each side.
    const QUIET: isize = 2;
    let mut out = String::new();
    let mut y = -QUIET;
    while y < width + QUIET {
        if color {
            out.push_str("\x1b[30;107m");
        }
        for x in -QUIET..width + QUIET {
            let (top, bottom) = (dark(x, y), dark(x, y + 1));
            // Without color, the blocks draw the light modules: a dark
            // terminal then shows dark modules on a light field.
            let (top, bottom) = if color {
                (top, bottom)
            } else {
                (!top, !bottom)
            };
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        if color {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
        y += 2;
    }
    eprint!("{out}");
    Ok(())
}

async fn join(common: &CommonArgs, text: &str, name: String, alias: Option<String>) -> Result<()> {
    if let Some(alias) = &alias {
        peers::validate_alias(alias)?;
    }
    let invitation = pairing::Invitation::decode(text)?;
    let ticket: EndpointTicket = invitation.ticket.parse()?;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let client = ControlClient::new(endpoint.clone());
    let result = async {
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
                    eprintln!("{}", yappr::proto::printable(&reply.message));
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

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    })
}

async fn run(args: Args) -> Result<()> {
    let common = &args.common;
    match args.command {
        Commands::Post(args) => post(common, args).await,
        Commands::Serve(args) => serve(common, args).await,
        Commands::Init(args) => init(common, args).await,
        Commands::Follow(args) => {
            tokio::select! {
                res = follow(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Allow { id } => allow(common, id),
        Commands::Deny { id } => deny(common, id),
        Commands::Allowed => {
            for id in common.store()?.allow_list().load()? {
                println!("{id}");
            }
            Ok(())
        }
        Commands::Id => {
            println!("{}", common.secret()?.public());
            Ok(())
        }
        Commands::Ask(args) => {
            if args.choices.len() < 2 {
                return Err(Usage(
                    "give two or more choices, such as --choice retry --choice skip. \
                     To ask for approval, use: yappr approve"
                        .into(),
                )
                .into());
            }
            ask(common, args.question, args.choices, false).await
        }
        Commands::Approve(args) => {
            ask(common, args.question, choices::approval_options(), true).await
        }
        Commands::Questions { output } => {
            let text = Format::is_text(output);
            for state in choices::list(&common.store()?.entries()?) {
                if text {
                    println!("{}", question_line(&state));
                } else {
                    println!("{}", serde_json::to_string(&state)?);
                }
            }
            Ok(())
        }
        Commands::Wait { id } => wait(common, &id).await,
        Commands::Cancel { id } => choices::cancel(&common.store()?, &id),
        Commands::Devices => devices(common),
        Commands::Status { output } => status(common, Format::is_text(output)),
        Commands::Answer { ticket, id, value } => {
            let ticket = EndpointTicket::new(peers::resolve(&common.dir()?, &ticket)?);
            answer(common, ticket, id, value).await
        }
        Commands::Retry => retry(common).await,
        Commands::Pair(args) => pair(common, args).await,
        Commands::Join {
            invitation,
            name,
            alias,
        } => join(common, &invitation, name, alias).await,
        Commands::Alias { command } => match command {
            AliasCommand::Set { name, target } => peers::set(&common.dir()?, &name, &target),
            AliasCommand::Remove { name } => peers::remove(&common.dir()?, &name),
        },
        Commands::Network { command } => match command {
            NetworkCommand::Show => {
                println!(
                    "{}",
                    serde_json::to_string(&net::RelaySettings::load(&common.dir()?)?)?
                );
                Ok(())
            }
            NetworkCommand::Set { defaults } => {
                if defaults && (common.no_relay || !common.relays.is_empty()) {
                    bail!("--defaults cannot be combined with --relay or --no-relay");
                }
                if common.relays.is_empty() && !common.no_relay && !defaults {
                    bail!("use --relay URL, --no-relay, or --defaults");
                }
                net::RelaySettings {
                    urls: common.relays.clone(),
                    disabled: common.no_relay,
                }
                .save(&common.dir()?)
            }
        },
        Commands::Health { target, output } => {
            probe(common, &target, Format::is_text(output)).await
        }
        Commands::Ticket => {
            let addr = EndpointAddr::new(common.secret()?.public());
            println!("{}", EndpointTicket::new(addr));
            Ok(())
        }
    }
}

/// The exit code for an error. docs/scripting.md documents these codes.
fn exit_code(err: &anyhow::Error) -> i32 {
    if is_broken_pipe(err) {
        // the reader of stdout left, for example "yappr follow ... | head"
        0
    } else if let Some(outcome) = err.downcast_ref::<QuestionOutcome>() {
        outcome.code
    } else if err.downcast_ref::<Usage>().is_some() {
        2
    } else if let Some(error) = err.downcast_ref::<choices::ChoiceError>() {
        match error.code.as_str() {
            "denied" => 3,
            "expired" => 6,
            "cancelled" => 8,
            _ => 4,
        }
    } else if err.downcast_ref::<Denied>().is_some() {
        3
    } else if err.downcast_ref::<Invalid>().is_some()
        || err.downcast_ref::<Rejected>().is_some()
        || err.downcast_ref::<yappr::client::OutOfOrder>().is_some()
    {
        4
    } else if err.downcast_ref::<Unreachable>().is_some() {
        5
    } else {
        1
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // clap exits with code 2 on a usage error
    let args = Args::parse();
    match run(args).await {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            let code = exit_code(&e);
            if code != 0 {
                eprintln!("error: {e:#}");
            }
            std::process::exit(code)
        }
    }
}
