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
    choices::{self, Choice, Question, QuestionStatus},
    client::{Denied, Event, Follower, Unreachable},
    control::ControlClient,
    cursor::Cursor,
    deliver::{self, Hello},
    disk,
    entry::{self, Draft, Entry, Invalid},
    ingest::{self, Rejected},
    net::{self, NetOptions, Network},
    outbox, pairing, peers,
    proto::{Request, Stored},
    render::{self, ColorChoice, Output, Renderer},
    server::{Feed, Server},
    store::Store,
    EndpointTicket,
};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Private feeds, questions, and choices between your devices.
///
/// One machine writes entries. Your other devices read them over iroh. There
/// is no account or hosted feed service. A device that reads the feed gets
/// the entries that it missed, and with --follow, the new entries.
///
/// The private key of this machine is in <feed dir>/secret.key. Set YAPPR_KEY
/// to use a different private key, such as the key of a CI job.
#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
    #[clap(flatten)]
    common: CommonArgs,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(clap::Args, Debug)]
#[clap(next_help_heading = "Global options")]
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
    /// The body comes from stdin, or from --body. The start command does not
    /// have to run. Without it, post delivers the entry to the devices that
    /// receive the feed.
    #[clap(after_help = "Examples:
  df -h / | yappr post --source disk --title \"Disk usage\"
  yappr post --kind deploy.finished --subject deploy/web --data '{\"version\":\"1.4.2\"}' --body \"v1.4.2 is live\"
  yappr read laptop -f --output json | jq -c 'select(.level == \"alert\")' | yappr post --json")]
    Post(PostArgs),

    /// Serve the feed, and deliver it to the devices that receive it.
    ///
    /// The first start pairs one device. With --pair, start pairs a device
    /// also when other devices can read the feed. To pair more devices while
    /// start runs, use pair in a second terminal. Prints the ticket on stderr.
    #[clap(after_help = "Examples:
  yappr start
  yappr start --pair --name \"Work laptop\"")]
    Start(StartArgs),

    /// The same as "start --pair".
    #[clap(hide = true)]
    Init(StartArgs),

    /// The same as "start", but it does not pair the first device.
    #[clap(hide = true)]
    Serve(StartArgs),

    /// Print the new entries of a feed. With --follow, wait for more.
    ///
    /// Without --since or --tail, read starts after the last entry that it
    /// printed before, and it saves its position after each entry. Without a
    /// writer, read prints the feed of this machine.
    #[clap(after_help = "Examples:
  yappr read laptop
  yappr read laptop -f
  yappr read laptop server --output json
  yappr read laptop --since 2h
  yappr read laptop --open")]
    Read(ReadArgs),

    /// The same as "read --follow".
    #[clap(hide = true)]
    Follow(ReadArgs),

    /// Run a command for each new entry that matches the filters.
    ///
    /// The command gets the entry as JSON on stdin, and YAPPR_ID, YAPPR_KIND,
    /// YAPPR_LEVEL, YAPPR_SUBJECT and YAPPR_SOURCE in its environment. The
    /// saved position moves past an entry after the command exits with 0.
    /// When the command fails, on stops, so the entry runs again at the next
    /// start. The first start begins after the last entry of the feed.
    #[clap(after_help = "Examples:
  yappr on server --kind deploy.requested -- ./deploy.sh
  yappr on server --level alert -- sh -c 'jq -r .title | say'")]
    On(OnArgs),

    /// Let a device read this feed.
    ///
    /// Pairing allows a device too. With an address and --receives, allow
    /// also delivers this feed to the device, as pairing does. A CI job uses
    /// it to reach a phone without pairing.
    #[clap(after_help = "Examples:
  yappr allow phone
  yappr allow \"$YAPPR_PHONE\" --alias phone --receives")]
    Allow {
        /// The alias, endpoint ID, or address (ticket) of the device.
        #[clap(value_name = "DEVICE")]
        id: String,
        /// Save the device under this alias.
        #[clap(long)]
        alias: Option<String>,
        /// Deliver this feed to the device. The device needs an address.
        #[clap(long)]
        receives: bool,
    },

    /// Connect a coding agent to your devices with hooks.
    ///
    /// The agent's permission requests become approvals on a device, and
    /// the agent posts when it needs input or finishes. Without an answer
    /// in time, the agent asks in the terminal as before.
    #[clap(after_help = "Examples:
  yappr hooks install claude
  yappr hooks install codex --to phone
  yappr hooks status
  yappr hooks remove claude")]
    Hooks {
        #[clap(subcommand)]
        command: HooksCommand,
    },

    /// Handle one hook event of a coding agent. The agent runs this command
    /// and gives the event as JSON on stdin.
    #[clap(hide = true)]
    Hook(HookArgs),

    /// Create a key for a machine that a device allows without pairing,
    /// such as a CI job.
    ///
    /// Prints the private key once, for the machine's secret store as
    /// YAPPR_KEY, and shows a code that the app scans to allow the public
    /// key. With --for, it also prints the address of a paired device, so
    /// that the machine can deliver to it. This computer does not keep the
    /// key.
    #[clap(after_help = "Examples:
  yappr key --name \"CI deploys\" --for phone
  yappr key --name ci --for phone --output env")]
    Key(KeyArgs),

    /// Stop a device from reading this feed.
    Deny {
        /// The alias or endpoint ID of the device.
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
    /// Print local questions and their states. "read --open" prints the open
    /// questions.
    #[clap(hide = true)]
    Questions {
        /// The output format. The default is text for a terminal and one JSON
        /// state per line for a pipe or a file.
        #[clap(long, value_enum)]
        output: Option<Format>,
    },
    /// Wait for a local question's final decision.
    Wait {
        /// The entry ID of the question, such as 42, or its question ID.
        id: String,
    },
    /// Cancel a local question.
    Cancel {
        /// The entry ID of the question, such as 42, or its question ID.
        id: String,
    },
    /// Answer a question from another writer.
    ///
    /// The answer waits in a local outbox until the writer saves it.
    #[clap(after_help = "Examples:\n  yappr read laptop --open\n  yappr answer laptop 42 retry")]
    Answer {
        /// The alias, ID, or ticket of the writer that asked.
        #[clap(value_name = "WRITER")]
        ticket: String,
        /// The entry ID of the question, such as 42, or its question ID.
        id: String,
        /// The value of the selected choice, such as "approve".
        #[clap(value_name = "VALUE", required_unless_present = "choice")]
        value: Option<String>,
        /// The same as VALUE.
        #[clap(long, hide = true, conflicts_with = "value")]
        choice: Option<String>,
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
    /// Send queued answers again, and deliver this feed to the devices
    /// that receive it.
    Retry,
    /// Name devices locally. An alias does not grant access. `yappr status` lists them.
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
    /// Pair a device with an expiring invitation while start runs.
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
    /// The program or the topic that the question comes from. The default is
    /// the host name.
    #[clap(long)]
    source: Option<String>,
    #[clap(flatten)]
    details: DetailArgs,
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
#[derive(Subcommand, Debug)]
enum HooksCommand {
    /// Add the yappr hooks to the settings of an agent. Other hooks stay.
    Install {
        agent: yappr::hooks::Agent,
        /// The device that answers. The default is the one device that
        /// receives this feed.
        #[clap(long, value_name = "DEVICE")]
        to: Option<String>,
        /// The events, such as permission,notification,stop or
        /// pretooluse:Bash. The default is permission and stop. Claude
        /// also has notification.
        #[clap(long)]
        events: Option<String>,
        /// Which settings file to change.
        #[clap(long, value_enum, default_value_t = HookScope::User)]
        scope: HookScope,
        /// How long a hook waits for an answer, such as 2m.
        #[clap(long, value_parser = parse_interval, default_value = "2m")]
        timeout: Duration,
    },
    /// Show the yappr hooks in the settings of each agent.
    Status { agent: Option<yappr::hooks::Agent> },
    /// Remove the yappr hooks from the settings of an agent.
    Remove {
        agent: yappr::hooks::Agent,
        #[clap(long, value_enum, default_value_t = HookScope::User)]
        scope: HookScope,
    },
}

/// The settings file of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum HookScope {
    /// Your settings, for all projects.
    User,
    /// The settings of this project, in its repository.
    Project,
    /// The settings of this project on this machine. Claude Code only.
    Local,
}

#[derive(clap::Args, Debug)]
struct HookArgs {
    #[clap(long, value_enum)]
    agent: yappr::hooks::Agent,
    /// The device that answers.
    #[clap(long)]
    to: Option<String>,
    /// How long to wait for an answer.
    #[clap(long, value_parser = parse_interval, default_value = "2m")]
    timeout: Duration,
    /// Deliver the feed through this entry, and read no event.
    #[clap(long, hide = true)]
    deliver: Option<u64>,
}

#[derive(clap::Args, Debug)]
struct KeyArgs {
    /// The name that the app shows for the machine.
    #[clap(long, default_value = "CI")]
    name: String,
    /// A paired device, such as phone, whose address the machine needs.
    #[clap(long = "for", value_name = "DEVICE")]
    device: Option<String>,
    /// text for people, env for KEY=value lines, json for programs.
    #[clap(long, value_enum, default_value_t = KeyOutput::Text)]
    output: KeyOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum KeyOutput {
    Text,
    Env,
    Json,
}

#[derive(clap::Args, Debug)]
struct StartArgs {
    /// Pair a device, also when other devices can read the feed.
    #[clap(long)]
    pair: bool,
    /// The writer name shown on the joining device. The default is the host
    /// name.
    #[clap(long)]
    name: Option<String>,
    /// Save the accepted device under this alias. The default comes from the
    /// device's name.
    #[clap(long)]
    alias: Option<String>,
    /// How long the invitation stays open, such as 5m or 1h.
    #[clap(long = "timeout", alias = "expires", value_parser = parse_interval, default_value = "5m")]
    expires: Duration,
    /// The UDP port for IPv4. With a fixed port, the ticket stays valid after
    /// a restart, also when no relay is available. The default is a random
    /// port.
    #[clap(long)]
    port: Option<u16>,
}

#[derive(clap::Args, Debug)]
struct PairArgs {
    /// The writer name shown on the joining device. The default is the host
    /// name.
    #[clap(long)]
    name: Option<String>,
    /// How long the invitation stays open, such as 5m or 1h.
    #[clap(long = "timeout", alias = "expires", value_parser = parse_interval, default_value = "5m")]
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
struct PostArgs {
    /// The program or the topic that the entry comes from. The default is the
    /// host name.
    #[clap(long)]
    source: Option<String>,

    /// A one-line summary.
    #[clap(long)]
    title: Option<String>,

    /// What the entry is, such as "deploy.finished". "state" means the
    /// current state of the subject.
    #[clap(long, default_value = entry::DEFAULT_KIND)]
    kind: String,

    #[clap(flatten)]
    details: DetailArgs,

    /// The body. If you do not give it, the body comes from stdin.
    #[clap(long)]
    body: Option<String>,

    /// Post each line of stdin as one entry in JSON, the format of "read
    /// --output json". A field that a line does not have comes from the
    /// options.
    #[clap(long, conflicts_with = "body")]
    json: bool,
}

/// The optional fields that post, ask and approve share.
#[derive(clap::Args, Debug)]
struct DetailArgs {
    /// How urgent it is. Readers color "warn" and "alert".
    #[clap(long, value_parser = clap::builder::PossibleValuesParser::new(entry::LEVELS))]
    level: Option<String>,

    /// What it is about, such as "backup/nas". Entries with the same subject
    /// belong together.
    #[clap(long)]
    subject: Option<String>,

    /// A JSON object for programs, such as '{"version":"1.4.2"}'.
    #[clap(long, value_name = "JSON", value_parser = parse_data)]
    data: Option<serde_json::Map<String, serde_json::Value>>,

    /// Show the body as Markdown in the app.
    #[clap(long)]
    markdown: bool,
}

impl DetailArgs {
    fn into_details(self) -> entry::Details {
        entry::Details {
            level: self.level,
            subject: self.subject,
            format: self.markdown.then(|| "markdown".into()),
            data: self.data,
        }
    }
}

fn parse_data(s: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match serde_json::from_str(s) {
        Ok(serde_json::Value::Object(data)) => Ok(data),
        Ok(_) => Err("give a JSON object, such as '{\"version\":\"1.4.2\"}'".into()),
        Err(e) => Err(format!("not valid JSON: {e}")),
    }
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
struct ReadArgs {
    /// Writer aliases, endpoint IDs, or feed tickets. Without a writer, read
    /// prints the feed of this machine.
    #[clap(value_name = "WRITER")]
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

    /// Print only the entries of this kind. "*" matches any text, as in
    /// "deploy.*". Use the option again for more kinds.
    #[clap(long)]
    kind: Vec<String>,

    /// Do not print the entries of this kind, for example "log". Use the
    /// option again for more kinds.
    #[clap(long, value_name = "KIND")]
    not_kind: Vec<String>,

    /// Print only the entries with this level or a higher level.
    #[clap(long, value_parser = clap::builder::PossibleValuesParser::new(entry::LEVELS))]
    level: Option<String>,

    /// Print only the entries with a subject that matches this pattern, such
    /// as "backup/*".
    #[clap(long)]
    subject: Option<String>,

    /// Of the entries that are already in the feed, print only the newest
    /// entry for each subject. Entries without a subject all print.
    #[clap(long)]
    latest: bool,

    /// Print only the entries that still need attention: questions with no
    /// final decision, and subjects whose newest "state" entry is "warn" or
    /// "alert". From a writer, only the questions that this device can answer.
    /// The saved position does not move.
    #[clap(long, conflicts_with_all = ["since", "tail", "latest"])]
    open: bool,

    /// The output format. The default is text for a terminal and JSON for a
    /// pipe or a file.
    #[clap(long, value_enum)]
    output: Option<Output>,

    /// When to use color.
    #[clap(long, value_enum, default_value_t)]
    color: ColorChoice,

    /// After the last entry, wait for new entries.
    #[clap(short, long)]
    follow: bool,

    /// Stop after the last entry. This is the default of read.
    #[clap(long, hide = true, overrides_with = "follow")]
    no_follow: bool,

    /// The name of the saved position. Different names have independent
    /// positions.
    #[clap(long, default_value = "default", value_name = "NAME")]
    cursor: String,
}

#[derive(clap::Args, Debug)]
struct OnArgs {
    /// The alias, endpoint ID, or ticket of the writer.
    #[clap(value_name = "WRITER")]
    ticket: String,

    /// Run only for entries of this kind. "*" matches any text. Use the
    /// option again for more kinds.
    #[clap(long)]
    kind: Vec<String>,

    /// Do not run for entries of this kind.
    #[clap(long, value_name = "KIND")]
    not_kind: Vec<String>,

    /// Run only for entries with this level or a higher level.
    #[clap(long, value_parser = clap::builder::PossibleValuesParser::new(entry::LEVELS))]
    level: Option<String>,

    /// Run only for entries with a subject that matches this pattern.
    #[clap(long)]
    subject: Option<String>,

    /// Run only for entries with a source that matches this regex.
    #[clap(long, value_name = "REGEX")]
    source: Option<Regex>,

    /// When the command fails, print the error and continue with the next
    /// entry, so the failed entry does not run again.
    #[clap(long)]
    skip_failed: bool,

    /// The name of the saved position.
    #[clap(long, default_value = "on", value_name = "NAME")]
    cursor: String,

    /// The command and its arguments. No shell reads them.
    #[clap(last = true, required = true, value_name = "COMMAND")]
    command: Vec<String>,
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

    /// The feed of this machine. It signs new entries with this machine's key.
    fn store(&self) -> Result<Store> {
        Ok(Store::open(self.dir()?)?.signed_by(self.secret()?))
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

async fn post(common: &CommonArgs, mut args: PostArgs) -> Result<()> {
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
fn own_receivers(store: &Store) -> Result<Vec<(String, EndpointAddr)>> {
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
async fn deliver_now(common: &CommonArgs, store: &Store, through: u64) -> Result<()> {
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
struct Undelivered {
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
enum Pairing {
    /// When no device can read the feed.
    First,
    Always,
    Never,
}

async fn start(common: &CommonArgs, args: StartArgs, pairing: Pairing) -> Result<()> {
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

/// Prints the entries that pass the filters, and moves the cursor.
async fn on(common: &CommonArgs, args: OnArgs) -> Result<()> {
    let dir = common.dir()?;
    let addr = peers::resolve(&dir, &args.ticket)?;
    let cursor = Cursor::new(&dir, addr.id, &args.cursor)?;
    let mut req = Request {
        follow: true,
        kind_patterns: args.kind,
        not_kind_patterns: args.not_kind,
        min_level: args.level,
        subject: args.subject,
        source: args.source.as_ref().map(|re| re.as_str().to_string()),
        ..Default::default()
    };
    if cursor.exists() {
        let position = cursor.load()?;
        req.since = position.id;
        req.log = position.log;
    } else {
        // A new handler runs for new entries only, not for the history.
        req.tail = Some(0);
    }
    let filter = yappr::proto::Filter::new(&req)?;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let mut follower = Follower::new(endpoint.clone(), addr, req);
    let result = async {
        while let Some(event) = follower.next().await? {
            match event {
                Event::Entry(entry) => {
                    if filter.matches(&entry) {
                        if let Err(error) = run_handler(&args.command, &entry).await {
                            if !args.skip_failed {
                                return Err(
                                    error.context(format!("entry #{} is not done", entry.id))
                                );
                            }
                            eprintln!("entry #{}: {}. Skipped.", entry.id, shown(&error));
                        }
                    }
                    cursor.save(entry.id, follower.log())?;
                }
                Event::CaughtUp { head } => {
                    // Also save 0, so that the next start does not run the
                    // entries that arrive in between as history.
                    let saved = cursor.load()?;
                    if !cursor.exists() || head > saved.id || saved.log.as_deref() != follower.log()
                    {
                        cursor.save(head, follower.log())?;
                    }
                }
                Event::Retrying { delay, error } => {
                    eprintln!(
                        "cannot read the feed: {}. Retry in {}s",
                        yappr::proto::printable(&error),
                        delay.as_secs()
                    );
                }
            }
        }
        anyhow::Ok(())
    }
    .await;
    endpoint.close().await;
    result
}

/// Run the handler for one entry and wait for it.
async fn run_handler(command: &[String], entry: &Entry) -> Result<()> {
    let (program, args) = command.split_first().context("give a command")?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .env("YAPPR_ID", entry.id.to_string())
        .env("YAPPR_KIND", entry.kind_name())
        .env("YAPPR_LEVEL", entry.level())
        .env(
            "YAPPR_SUBJECT",
            entry.subject.as_deref().unwrap_or_default(),
        )
        .env("YAPPR_SOURCE", &entry.source)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // A handler that does not read stdin closes it early. That is not
        // an error.
        if let Err(e) = stdin.write_all(&line).await {
            if e.kind() != io::ErrorKind::BrokenPipe {
                return Err(e.into());
            }
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

/// The entries without a subject, and the newest entry for each subject, in
/// feed order. Only the entries that the filter wants count.
fn newest_for_each_subject(entries: Vec<Entry>) -> Vec<Entry> {
    let mut newest = std::collections::HashMap::new();
    for (at, entry) in entries.iter().enumerate() {
        if let Some(subject) = &entry.subject {
            newest.insert(subject.clone(), at);
        }
    }
    entries
        .into_iter()
        .enumerate()
        .filter(|(at, entry)| entry.subject.as_ref().is_none_or(|s| newest[s] == *at))
        .map(|(_, entry)| entry)
        .collect()
}

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

    /// Move the cursor to `head` of the log `log`. The writer checked every
    /// entry up to there, also the entries that the filter left out. In one
    /// log, the cursor never moves back.
    fn passed(&self, head: u64, log: Option<&str>) -> Result<()> {
        if let Some(cursor) = &self.cursor {
            let saved = cursor.load()?;
            if head > saved.id || saved.log.as_deref() != log {
                cursor.save(head, log)?;
            }
        }
        Ok(())
    }

    /// Print one entry of the log `log`, then move the cursor past it.
    fn print(&self, entry: &Entry, log: Option<&str>) -> Result<()> {
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
            cursor.save(entry.id, log)?;
        }
        Ok(())
    }
}

/// Before levels, the kind was the level, and every other entry had the kind
/// "info". These kinds now mean a level, or no filter at all.
fn legacy_kinds(args: &mut ReadArgs) {
    let mut levels = Vec::new();
    args.kind.retain(|kind| match kind.as_str() {
        "warn" | "alert" => {
            levels.push(kind.clone());
            false
        }
        "info" => {
            eprintln!("Entries have the kind \"event\" now. Use --kind event.");
            false
        }
        _ => true,
    });
    if let Some(level) = levels.into_iter().min_by_key(|l| entry::level_rank(l)) {
        eprintln!("Use --level {level} in place of --kind {level}.");
        args.level.get_or_insert(level);
    }
}

/// Where `read` gets entries: a writer over iroh, or this machine's feed.
enum Source {
    Remote(Follower),
    Local(LocalFeed),
}

impl Source {
    async fn next(&mut self) -> Result<Option<Event>> {
        match self {
            Self::Remote(follower) => follower.next().await,
            Self::Local(feed) => feed.next().await,
        }
    }

    fn connection(&self) -> Option<iroh::endpoint::Connection> {
        match self {
            Self::Remote(follower) => follower.connection(),
            Self::Local(_) => None,
        }
    }

    /// The name of the feed's log, after the source knows it.
    fn log(&self) -> Option<&str> {
        match self {
            Self::Remote(follower) => follower.log(),
            Self::Local(feed) => feed.log.as_deref(),
        }
    }
}

/// Reads the local feed file with the same request and events as a writer.
struct LocalFeed {
    reader: yappr::store::LogReader,
    /// The identity that signs this feed.
    writer: iroh::EndpointId,
    /// The name of the log: the time of the first entry.
    log: Option<String>,
    req: Request,
    filter: yappr::proto::Filter,
    replay: Option<std::collections::VecDeque<Entry>>,
    head: u64,
    caught_up: bool,
}

impl LocalFeed {
    fn new(store: &Store, writer: iroh::EndpointId, mut req: Request) -> Result<Self> {
        let log = store.log()?;
        // A position in another log does not apply to this log.
        if req.log.is_some() && req.log != log {
            req.since = 0;
        }
        Ok(Self {
            reader: store.reader()?,
            writer,
            log,
            filter: yappr::proto::Filter::new(&req)?,
            req,
            replay: None,
            head: 0,
            caught_up: false,
        })
    }

    /// The next entry of the log after the rules and signature checks that a
    /// remote reader also makes.
    fn next_checked(&mut self) -> Result<Option<Entry>> {
        let Some(entry) = self.reader.next_entry()? else {
            return Ok(None);
        };
        let log = self.log.get_or_insert_with(|| entry.time.to_string());
        entry
            .validate()
            .with_context(|| format!("entry {} of this feed is not valid", entry.id))?;
        entry.verify(&self.writer, log)?;
        Ok(Some(entry))
    }

    fn wanted(&self, entry: &Entry) -> bool {
        entry.id > self.req.since
            && self.req.since_time.is_none_or(|t| entry.time >= t)
            && self.filter.matches(entry)
    }

    async fn next(&mut self) -> Result<Option<Event>> {
        if self.replay.is_none() {
            let mut replay = std::collections::VecDeque::new();
            while let Some(entry) = self.next_checked()? {
                self.head = entry.id;
                if self.wanted(&entry) {
                    replay.push_back(entry);
                    if self.req.tail.is_some_and(|n| replay.len() as u64 > n) {
                        replay.pop_front();
                    }
                }
            }
            self.replay = Some(replay);
        }
        if let Some(entry) = self.replay.as_mut().and_then(|r| r.pop_front()) {
            return Ok(Some(Event::Entry(entry)));
        }
        if !self.caught_up {
            self.caught_up = true;
            return Ok(Some(Event::CaughtUp { head: self.head }));
        }
        if !self.req.follow {
            return Ok(None);
        }
        loop {
            while let Some(entry) = self.next_checked()? {
                if self.wanted(&entry) {
                    return Ok(Some(Event::Entry(entry)));
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn read(common: &CommonArgs, mut args: ReadArgs) -> Result<()> {
    legacy_kinds(&mut args);
    let dir = common.dir()?;
    let secret = common.secret()?;
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
        targets.push((label, Some(addr)));
    }
    if targets.is_empty() {
        targets.push(("local".into(), None));
    }
    let endpoint = match targets[0].1 {
        Some(_) => Some(net::bind(secret.clone(), common.net(None)?, vec![], None).await?),
        None => None,
    };
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
    let follow = args.follow && !args.no_follow;
    let mut tasks = tokio::task::JoinSet::new();
    for (label, addr) in targets {
        let mut req = Request {
            follow,
            tail: args.tail,
            kind_patterns: args.kind.clone(),
            not_kind_patterns: args.not_kind.clone(),
            min_level: args.level.clone(),
            subject: args.subject.clone(),
            source: args.source.as_ref().map(|re| re.as_str().to_string()),
            ..Default::default()
        };
        let filter = yappr::proto::Filter::new(&req)?;
        let feed = addr.as_ref().map_or(secret.public(), |addr| addr.id);
        let cursor = match args.since {
            _ if args.open => {
                // The writer sends every entry that can open or close an
                // entry. The filters apply to the open entries.
                req = Request {
                    follow,
                    kind_patterns: yappr::open::KINDS.map(String::from).to_vec(),
                    ..Default::default()
                };
                None
            }
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
                let cursor = Cursor::new(&dir, feed, &args.cursor)?;
                let position = cursor.load()?;
                req.since = position.id;
                req.log = position.log;
                Some(cursor)
            }
        };
        let printer = Printer {
            cursor,
            filter,
            writer: multiple.then(|| label.clone()),
            renderer: renderer.clone(),
        };
        let status = follow && addr.is_some() && io::stderr().is_terminal();
        let latest = args.latest;
        // On the local feed, the open questions are the questions that this
        // machine asked. On a writer, they are the questions for this device.
        let mut open = args
            .open
            .then(|| yappr::open::Open::new(addr.as_ref().map(|_| secret.public())));
        let mut source = match (&endpoint, addr) {
            (Some(endpoint), Some(addr)) => {
                Source::Remote(Follower::new(endpoint.clone(), addr, req))
            }
            _ => Source::Local(LocalFeed::new(&common.store()?, secret.public(), req)?),
        };
        tasks.spawn(async move {
            let mut caught_up = false;
            let mut replayed = false;
            // With --latest, the replay waits here until it is complete.
            let mut replay: Option<Vec<Entry>> = latest.then(Vec::new);
            let result = async {
                while let Some(event) = source.next().await? {
                    match event {
                        Event::Entry(entry) => match (&mut open, &mut replay) {
                            (Some(open), _) => {
                                if open.push(&entry) && replayed {
                                    printer.print(&entry, source.log())?;
                                }
                            }
                            (None, Some(replay)) if printer.wants(&entry) => replay.push(entry),
                            (None, Some(_)) => {}
                            (None, None) => printer.print(&entry, source.log())?,
                        },
                        Event::CaughtUp { head } => {
                            if let Some(replay) = replay.take() {
                                for entry in newest_for_each_subject(replay) {
                                    printer.print(&entry, source.log())?;
                                }
                            }
                            if let Some(open) = open.as_ref().filter(|_| !replayed) {
                                for entry in open.entries() {
                                    printer.print(entry, source.log())?;
                                }
                            }
                            replayed = true;
                            printer.passed(head, source.log())?;
                            if status && !caught_up {
                                let health =
                                    source.connection().map(|c| yappr::health::snapshot(&c));
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
                                "[{label}] cannot read the feed: {}. Retry in {}s",
                                yappr::proto::printable(&error),
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
                if let Some(endpoint) = &endpoint {
                    endpoint.close().await;
                }
                return Err(error);
            }
            if multiple {
                eprintln!("[{label}] stopped: {}", shown(&error));
            }
            failure = Some(error);
        }
    }
    if let Some(endpoint) = &endpoint {
        endpoint.close().await;
    }
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

fn allow(common: &CommonArgs, target: String, alias: Option<String>, receives: bool) -> Result<()> {
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

/// The settings file of `agent` for `scope`. CLAUDE_CONFIG_DIR and
/// CODEX_HOME move the user settings, as they do for the agents.
fn hook_settings(agent: yappr::hooks::Agent, scope: HookScope) -> Result<PathBuf> {
    use yappr::hooks::Agent;
    let home = || dirs::home_dir().context("cannot find the home directory");
    let env_dir = |var: &str| std::env::var_os(var).map(PathBuf::from);
    Ok(match (agent, scope) {
        (Agent::Claude, HookScope::User) => env_dir("CLAUDE_CONFIG_DIR")
            .map_or_else(|| home().map(|h| h.join(".claude")), Ok)?
            .join("settings.json"),
        (Agent::Claude, HookScope::Project) => PathBuf::from(".claude/settings.json"),
        (Agent::Claude, HookScope::Local) => PathBuf::from(".claude/settings.local.json"),
        (Agent::Codex, HookScope::User) => env_dir("CODEX_HOME")
            .map_or_else(|| home().map(|h| h.join(".codex")), Ok)?
            .join("hooks.json"),
        (Agent::Codex, HookScope::Project) => PathBuf::from(".codex/hooks.json"),
        (Agent::Codex, HookScope::Local) => {
            return Err(
                Usage("Codex has no local settings. Use --scope user or project".into()).into(),
            )
        }
    })
}

/// The settings at `path`, or an empty object if the file does not exist.
fn read_settings(path: &std::path::Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON. Fix it first", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_settings(path: &std::path::Path, settings: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// The device that answers when an install names none: the one allowed
/// device that receives this feed, or else the one allowed device.
fn default_device(store: &Store) -> Result<String> {
    let allowed = store.allow_list().load()?;
    let peers: Vec<(String, peers::Peer)> = peers::list(store.dir())?
        .into_iter()
        .filter(|(_, peer)| allowed.contains(&peer.id))
        .collect();
    let receivers: Vec<&String> = peers
        .iter()
        .filter(|(_, peer)| peer.receives)
        .map(|(alias, _)| alias)
        .collect();
    match (receivers.as_slice(), peers.as_slice()) {
        ([one], _) => Ok((*one).clone()),
        ([], [(one, _)]) => Ok(one.clone()),
        ([], []) => Err(Usage(
            "no device can answer. Pair one first with: yappr start --pair".into(),
        )
        .into()),
        _ => {
            let names: Vec<&str> = peers.iter().map(|(alias, _)| alias.as_str()).collect();
            Err(Usage(format!(
                "several devices can answer: {}. Choose one with --to",
                names.join(", ")
            ))
            .into())
        }
    }
}

fn hooks_command(common: &CommonArgs, command: HooksCommand) -> Result<()> {
    use yappr::hooks::{self, Agent};
    match command {
        HooksCommand::Install {
            agent,
            to,
            events,
            scope,
            timeout,
        } => {
            let path = hook_settings(agent, scope)?;
            let events = match events {
                Some(text) => {
                    hooks::parse_events(agent, &text).map_err(|e| Usage(format!("{e:#}")))?
                }
                None => hooks::default_events(agent),
            };
            let store = common.store()?;
            let device = match to {
                Some(device) => device,
                None => default_device(&store)?,
            };
            let id = peers::resolve(store.dir(), &device)?.id;
            if !store.allow_list().contains(&id) {
                bail!("{device} cannot answer yet. Allow it first: yappr allow {device}");
            }
            let program = std::env::current_exe()?.to_string_lossy().into_owned();
            let dir = std::fs::canonicalize(store.dir())?
                .to_string_lossy()
                .into_owned();
            let secs = timeout.as_secs().max(1);
            let mut settings = read_settings(&path)?;
            if path.exists() {
                let backup = path.with_file_name(format!(
                    "{}.yappr-backup",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                std::fs::copy(&path, &backup)?;
            }
            hooks::install(
                &mut settings,
                &events,
                |_| hooks::command(&program, &dir, agent, Some(&device), secs),
                secs,
            )?;
            write_settings(&path, &settings)?;
            eprintln!("Added yappr hooks to {}:", path.display());
            for selected in &events {
                let what = if selected.event.asks() {
                    format!("ask {device}, then allow or deny")
                } else {
                    "post an entry".into()
                };
                let matcher = selected
                    .matcher
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                eprintln!("  {}{matcher}: {what}", selected.event.key());
            }
            if agent == Agent::Codex {
                eprintln!("In Codex, run /hooks once and trust the yappr hooks.");
            }
            eprintln!("To remove them: yappr hooks remove {agent}");
            Ok(())
        }
        HooksCommand::Status { agent } => {
            let agents = agent.map_or(vec![Agent::Claude, Agent::Codex], |a| vec![a]);
            let mut found = false;
            for agent in agents {
                for scope in [HookScope::User, HookScope::Project, HookScope::Local] {
                    let Ok(path) = hook_settings(agent, scope) else {
                        continue;
                    };
                    let Ok(settings) = read_settings(&path) else {
                        continue;
                    };
                    let installed = hooks::installed(&settings);
                    if installed.is_empty() {
                        continue;
                    }
                    found = true;
                    println!("{agent} ({}):", path.display());
                    for (event, command) in installed {
                        println!("  {event}: {command}");
                    }
                }
            }
            if !found {
                eprintln!("No yappr hooks are installed. Run \"yappr hooks install claude\" or \"yappr hooks install codex\".");
            }
            Ok(())
        }
        HooksCommand::Remove { agent, scope } => {
            let path = hook_settings(agent, scope)?;
            let mut settings = read_settings(&path)?;
            if hooks::remove(&mut settings) {
                write_settings(&path, &settings)?;
                eprintln!("Removed the yappr hooks from {}.", path.display());
            } else {
                eprintln!("{} has no yappr hooks.", path.display());
            }
            Ok(())
        }
    }
}

/// Handle one hook event. A failure never blocks the agent: yappr reports
/// it on stderr and gives no decision, so the agent asks as before.
async fn hook(common: &CommonArgs, args: HookArgs) {
    if let Some(through) = args.deliver {
        if let Ok(store) = common.store() {
            deliver_now(common, &store, through).await.ok();
        }
        return;
    }
    match run_hook(common, args).await {
        Ok(Some(decision)) => println!("{decision}"),
        Ok(None) => {}
        Err(error) => eprintln!("yappr hook: {}", shown(&error)),
    }
}

/// Start a process that delivers the feed through `through`, and do not wait
/// for it. The saved entry stays in the feed if the delivery fails.
fn deliver_later(store: &Store, agent: yappr::hooks::Agent, through: u64) -> Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--dir")
        .arg(store.dir())
        .args(["hook", "--agent", agent.name(), "--deliver"])
        .arg(through.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A process group of its own, so that the agent does not stop it with
    // the hook.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("start the delivery")?;
    Ok(())
}

async fn run_hook(common: &CommonArgs, args: HookArgs) -> Result<Option<serde_json::Value>> {
    use yappr::hooks::{self, Action};
    let mut text = String::new();
    io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .context("read the hook event")?;
    let input: hooks::Input =
        serde_json::from_str(&text).context("the hook event is not valid JSON")?;
    let store = common.store()?;
    match hooks::plan(args.agent, &input) {
        Action::Nothing => Ok(None),
        Action::Post {
            kind,
            title,
            body,
            subject,
        } => {
            let entry = store.append(Draft {
                source: args.agent.name().into(),
                kind,
                level: None,
                subject,
                title: Some(title),
                body,
                format: None,
                data: None,
                extra: Default::default(),
            })?;
            // The agent waits for the hook. A device that is not reachable
            // must not stop the agent, so deliver in another process.
            deliver_later(&store, args.agent, entry.id)?;
            Ok(None)
        }
        Action::Ask {
            event,
            title,
            body,
            subject,
        } => {
            let to = args
                .to
                .context("no device to ask. Run: yappr hooks install")?;
            let responder = peers::resolve(store.dir(), &to)?.id;
            if !store.allow_list().contains(&responder) {
                bail!("{to} cannot answer. Allow it first: yappr allow {to}");
            }
            let expires_at =
                Timestamp::now().checked_add(jiff::SignedDuration::try_from(args.timeout)?)?;
            let question = choices::new_question(
                title,
                body,
                args.agent.name().into(),
                choices::approval_options(),
                responder,
                Some(expires_at),
                true,
            );
            let details = entry::Details {
                subject,
                ..Default::default()
            };
            let question = choices::create_with(&store, question, details)?;
            // The question expires at the deadline, and a decision cannot
            // arrive after it. Read the state once more, in case a decision
            // reached disk at the deadline.
            let state = match timeout(args.timeout, final_state(common, &question.id)).await {
                Ok(state) => state,
                Err(_) => store
                    .entries()
                    .and_then(|entries| choices::get(&entries, &question.id)),
            };
            match state {
                Ok(state) if state.status == "answered" => {
                    let approved = state.decision.is_some_and(|d| d.value == "approve");
                    Ok(Some(hooks::decision(args.agent, event, approved)))
                }
                Ok(_) => Ok(None),
                Err(error) => {
                    choices::cancel(&store, &question.id).ok();
                    Err(error)
                }
            }
        }
    }
}

/// Create a key pair for a machine such as a CI job, and print what the
/// machine and the app need.
fn key(common: &CommonArgs, args: KeyArgs) -> Result<()> {
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

fn deny(common: &CommonArgs, target: String) -> Result<()> {
    let id = peers::resolve(&common.dir()?, &target)?.id;
    let list = common.store()?.allow_list();
    if list.deny(&id)? {
        eprintln!("denied {id}. A running start closes its feed within 5 seconds");
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
        args.source.unwrap_or_else(hostname),
        options,
        responder,
        Some(expires_at),
        approval,
    );
    let question: Question = choices::create_with(&store, question, args.details.into_details())?;
    if args.no_wait {
        println!("{}", question.id);
        return deliver_now(common, &store, store.last_id()?).await;
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
            match state.status {
                QuestionStatus::Pending => cancelled?,
                QuestionStatus::Cancelled => {}
                _ => if let Some(result) = finish(state) { return result; },
            }
            Err(QuestionOutcome { code: 8, message: "the wait stopped. Use yappr wait to read the final state" }.into())
        }
    }
}

async fn wait(common: &CommonArgs, id: &str) -> Result<()> {
    let state = final_state(common, id).await?;
    finish(state).expect("the question is in a final state")
}

/// Wait until the question `id` is answered, expired, or cancelled, and
/// deliver it to its device in the meantime. Returns its final state.
async fn final_state(common: &CommonArgs, id: &str) -> Result<choices::QuestionState> {
    let store = common.store()?;
    // Read only the entries that arrive after each check.
    let mut reader = store.reader()?;
    let mut tracker = choices::Tracker::one(id);
    let mut delivery = None;
    loop {
        while let Some(entry) = reader.next_entry()? {
            tracker.push(&entry);
        }
        let state = tracker
            .states()
            .pop()
            .ok_or_else(|| choices::ChoiceError::new("not_found", "the question does not exist"))?;
        let pending = state.status == QuestionStatus::Pending;
        if pending && delivery.is_none() {
            delivery = Some(
                QuestionDelivery::start(common, &store, state.question.responder)
                    .await
                    .unwrap_or_else(|error| {
                        eprintln!("cannot deliver the question: {}", shown(&error));
                        None
                    }),
            );
        }
        if !pending {
            if let Some(delivery) = delivery.flatten() {
                delivery.finish(store.last_id()?).await;
            }
            return Ok(state);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Delivers the feed to the device that answers a question, while the
/// question waits and no running writer delivers it.
struct QuestionDelivery {
    endpoint: iroh::Endpoint,
    task: n0_future::task::AbortOnDropHandle<()>,
    progress: tokio::sync::watch::Receiver<Stored>,
}

impl QuestionDelivery {
    async fn start(
        common: &CommonArgs,
        store: &Store,
        device: iroh::EndpointId,
    ) -> Result<Option<Self>> {
        let Some((_, addr)) = own_receivers(store)?
            .into_iter()
            .find(|(_, addr)| addr.id == device)
        else {
            return Ok(None);
        };
        let secret = common.secret()?;
        let endpoint =
            net::bind(iroh::SecretKey::generate(), common.net(None)?, vec![], None).await?;
        let feed = Feed::open(store.clone()).await?;
        let (progress, seen) = tokio::sync::watch::channel(Stored::default());
        let transport = endpoint.id();
        let task = tokio::spawn(deliver::deliver_until_stopped(
            endpoint.clone(),
            feed,
            addr,
            move || Hello::granted(&secret, transport),
            progress,
        ));
        Ok(Some(Self {
            endpoint,
            task: n0_future::task::AbortOnDropHandle::new(task),
            progress: seen,
        }))
    }

    /// Give the device a few seconds to save the entries up to `through`,
    /// such as the decision, then stop.
    async fn finish(mut self, through: u64) {
        timeout(
            Duration::from_secs(5),
            self.progress.wait_for(|stored| stored.through >= through),
        )
        .await
        .ok();
        drop(self.task);
        self.endpoint.close().await;
    }
}

/// The result of a question in a final state, or `None` while it is pending.
fn finish(state: choices::QuestionState) -> Option<Result<()>> {
    let outcome = |code, message| Some(Err(QuestionOutcome { code, message }.into()));
    match state.status {
        QuestionStatus::Answered => {
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
        QuestionStatus::Expired => outcome(6, "the question expired"),
        QuestionStatus::Cancelled => outcome(8, "the question was cancelled"),
        QuestionStatus::Pending => None,
    }
}

/// The question ID of a local question, given as an entry ID or a question ID.
fn local_question(common: &CommonArgs, id: String) -> Result<String> {
    match id.parse() {
        Ok(entry) => choices::id_of_entry(&common.store()?.entries()?, entry),
        Err(_) => Ok(id),
    }
}

/// The question ID of a writer's question, given as an entry ID or a question
/// ID. An entry ID needs the writer's question entry.
async fn remote_question(
    common: &CommonArgs,
    ticket: &EndpointTicket,
    id: String,
) -> Result<String> {
    let Ok(entry_id) = id.parse::<u64>() else {
        return Ok(id);
    };
    let req = Request {
        since: entry_id.saturating_sub(1),
        limit: Some(1),
        kind_patterns: vec!["question".into()],
        ..Default::default()
    };
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let mut follower = Follower::new(endpoint.clone(), ticket.endpoint_addr().clone(), req);
    let result = async {
        while let Some(event) = follower.next().await? {
            if let Event::Entry(entry) = event {
                if entry.id == entry_id {
                    if let Some(question) = choices::question(&entry) {
                        return Ok(question.id);
                    }
                }
                break;
            }
            if let Event::CaughtUp { .. } = event {
                break;
            }
        }
        Err(
            choices::ChoiceError::new("not_found", &format!("entry #{entry_id} is not a question"))
                .into(),
        )
    }
    .await;
    endpoint.close().await;
    result
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
            eprintln!(
                "The choice remains queued. Run yappr retry. {}",
                shown(&error)
            );
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
    let ticket = ticket.context("run \"yappr start\" before you create an invitation")?;
    let name = args.name.unwrap_or_else(hostname);
    match pairing_session(common, &store, ticket, name, args.expires, args.alias).await? {
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

/// How a QR code is drawn in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QrStyle {
    /// Two spaces with a background color for each module, and one module
    /// row on each line. A terminal fills the background of the whole cell,
    /// also the space between lines, so the code scans at any line height.
    Cells,
    /// Two module rows on each line, in half blocks. The code is half as
    /// wide and half as tall, but extra line height shows as stripes. With
    /// `color` false, the blocks draw the light modules, for a dark terminal.
    HalfBlocks { color: bool },
}

/// A quiet zone of two modules on each side.
const QR_QUIET: isize = 2;

/// Draw `text` as a QR code on stderr when stderr is a terminal. Black
/// modules on a white field scan best, so the colors are set and do not
/// follow the terminal theme. A terminal that is too narrow for cells gets
/// half blocks.
fn print_qr(text: &str) -> Result<()> {
    if !io::stderr().is_terminal() {
        return Ok(());
    }
    let color = render::use_color(
        ColorChoice::Auto,
        true,
        std::env::var("NO_COLOR").ok().as_deref(),
    );
    let size = qrcode::QrCode::new(text.as_bytes())?.width() as isize;
    let needed = (2 * (size + 2 * QR_QUIET)) as usize;
    let style = match terminal_width() {
        Some(width) if width < needed => QrStyle::HalfBlocks { color },
        _ if color => QrStyle::Cells,
        _ => QrStyle::HalfBlocks { color },
    };
    eprint!("{}", qr_text(text, style)?);
    Ok(())
}

/// The number of columns of the terminal on stderr, if it is known.
fn terminal_width() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes a winsize into `size`, which is valid.
        let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
        (ok && size.ws_col > 0).then_some(size.ws_col as usize)
    }
    #[cfg(not(unix))]
    None
}

/// The text of a QR code of `text` in `style`, with a line break after each
/// line.
fn qr_text(text: &str, style: QrStyle) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    let width = code.width() as isize;
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| {
        (0..width).contains(&x)
            && (0..width).contains(&y)
            && colors[(y * width + x) as usize] == qrcode::Color::Dark
    };
    let range = -QR_QUIET..width + QR_QUIET;
    let mut out = String::new();
    match style {
        QrStyle::Cells => {
            for y in range.clone() {
                let mut current = None;
                for x in range.clone() {
                    let module = dark(x, y);
                    // Write a color only when it changes.
                    if current != Some(module) {
                        out.push_str(if module { "\x1b[40m" } else { "\x1b[107m" });
                        current = Some(module);
                    }
                    out.push_str("  ");
                }
                out.push_str("\x1b[0m\n");
            }
        }
        QrStyle::HalfBlocks { color } => {
            for y in range.clone().step_by(2) {
                if color {
                    out.push_str("\x1b[30;107m");
                }
                for x in range.clone() {
                    let (top, bottom) = (dark(x, y), dark(x, y + 1));
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
            }
        }
    }
    Ok(out)
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

/// An error as text that is safe for a terminal. An error can carry text
/// from another endpoint, such as the reason why it closed a connection.
fn shown(error: &anyhow::Error) -> String {
    yappr::proto::printable(&format!("{error:#}"))
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
        Commands::Start(args) => start(common, args, Pairing::First).await,
        Commands::Init(args) => start(common, args, Pairing::Always).await,
        Commands::Serve(args) => start(common, args, Pairing::Never).await,
        Commands::On(args) => {
            tokio::select! {
                res = on(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Read(args) => {
            tokio::select! {
                res = read(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Follow(args) => {
            let args = ReadArgs {
                follow: true,
                ..args
            };
            tokio::select! {
                res = read(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Allow {
            id,
            alias,
            receives,
        } => allow(common, id, alias, receives),
        Commands::Key(args) => key(common, args),
        Commands::Hook(args) => {
            hook(common, args).await;
            Ok(())
        }
        Commands::Hooks { command } => hooks_command(common, command),
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
        Commands::Wait { id } => wait(common, &local_question(common, id)?).await,
        Commands::Cancel { id } => choices::cancel(&common.store()?, &local_question(common, id)?),
        Commands::Devices => devices(common),
        Commands::Status { output } => status(common, Format::is_text(output)),
        Commands::Answer {
            ticket,
            id,
            value,
            choice,
        } => {
            let ticket = EndpointTicket::new(peers::resolve(&common.dir()?, &ticket)?);
            let id = remote_question(common, &ticket, id).await?;
            let value = value.or(choice).context("give the value of a choice")?;
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

/// The exit code for an error. docs/reference.md documents these codes.
fn exit_code(err: &anyhow::Error) -> i32 {
    if is_broken_pipe(err) {
        // the reader of stdout left, for example "yappr read ... | head"
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
        || err.downcast_ref::<entry::BadSignature>().is_some()
        || err.downcast_ref::<Rejected>().is_some()
        || err.downcast_ref::<yappr::client::OutOfOrder>().is_some()
    {
        4
    } else if err.downcast_ref::<Unreachable>().is_some()
        || err.downcast_ref::<Undelivered>().is_some()
    {
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
                eprintln!("error: {}", shown(&e));
            }
            std::process::exit(code)
        }
    }
}

#[cfg(test)]
mod qr_tests {
    use super::*;

    /// `text` without escape sequences.
    fn visible(text: &str) -> String {
        regex::Regex::new("\x1b\\[[0-9;]*m")
            .unwrap()
            .replace_all(text, "")
            .into_owned()
    }

    #[test]
    fn cells_draw_one_module_row_on_each_line_with_two_columns_for_each_module() {
        let size = qrcode::QrCode::new(b"YAPPR:TEST").unwrap().width();
        let side = size + 2 * QR_QUIET as usize;
        let cells = qr_text("YAPPR:TEST", QrStyle::Cells).unwrap();
        let lines: Vec<String> = cells.lines().map(visible).collect();
        assert_eq!(lines.len(), side);
        assert!(lines
            .iter()
            .all(|l| l.len() == 2 * side && l.trim().is_empty()));
        assert!(cells.contains("\x1b[40m") && cells.contains("\x1b[107m"));

        let half = qr_text("YAPPR:TEST", QrStyle::HalfBlocks { color: true }).unwrap();
        let lines: Vec<String> = half.lines().map(visible).collect();
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(lines.iter().all(|l| l.chars().count() == side));
    }
}
