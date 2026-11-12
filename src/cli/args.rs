//! Command definitions, help text, and argument validation.

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use jiff::{Timestamp, Zoned};
use regex::Regex;
use yappr::{
    choices::{self, Choice},
    entry,
    net::Network,
    render::{ColorChoice, Output},
};

use super::Usage;

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
pub(super) struct Args {
    #[clap(flatten)]
    pub(super) common: CommonArgs,

    #[clap(subcommand)]
    pub(super) command: Commands,
}

#[derive(clap::Args, Debug)]
#[clap(next_help_heading = "Global options")]
pub(super) struct CommonArgs {
    /// The name of the local feed.
    #[clap(long, global = true, env = "YAPPR_FEED", default_value = "default")]
    pub(super) feed: String,

    /// The feed directory. This replaces the directory that --feed selects.
    #[clap(long, global = true, env = "YAPPR_DIR", value_name = "PATH")]
    pub(super) dir: Option<PathBuf>,

    /// Do not find or announce endpoints on the local network (mDNS).
    #[clap(long, global = true)]
    pub(super) no_mdns: bool,
    /// Use these relay URLs instead of n0 defaults. Repeat for more relays.
    #[clap(long = "relay", global = true, conflicts_with = "no_relay")]
    pub(super) relays: Vec<String>,
    /// Use direct connections only.
    #[clap(long, global = true, conflicts_with = "relays")]
    pub(super) no_relay: bool,

    /// "local" uses loopback only, with no relays. For tests.
    #[clap(
        long,
        global = true,
        hide = true,
        env = "YAPPR_NETWORK",
        value_enum,
        default_value_t
    )]
    pub(super) network: Network,
}

#[derive(Subcommand, Debug)]
pub(super) enum Commands {
    /// Add one entry to the local feed and print its id.
    ///
    /// The body comes from stdin, or from --body. The start command does not
    /// have to run. Without it, post delivers the entry to the devices that
    /// receive the feed.
    ///
    /// Exit code 5 means a device did not confirm the saved entry. It can
    /// still receive a push notification.
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

    /// Handle one hook event of a coding agent. The yappr plugin of the agent
    /// runs this command and gives the event as JSON on stdin.
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
    /// Print this machine's identity, writer, feed, peers, questions, and outbox.
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
    /// Show or save the settings for this feed.
    #[clap(
        after_help = "Relay and mDNS options are saved here and apply to one run on other commands."
    )]
    Config {
        /// Set the writer name. Pass an empty name to use the computer name.
        #[clap(long)]
        name: Option<String>,
        /// Use the default relays.
        #[clap(long)]
        default_relays: bool,
        /// Find and announce endpoints on the local network (mDNS).
        #[clap(long)]
        mdns: bool,
        #[clap(flatten)]
        hooks: HookConfigArgs,
        /// The output format.
        #[clap(long, value_enum, default_value = "text")]
        output: Format,
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
pub(super) enum Format {
    /// For people. The layout can change between versions.
    Text,
    /// One JSON object per line.
    Json,
}

impl Format {
    /// The chosen format, or text for a terminal and JSON for a pipe.
    pub(super) fn is_text(choice: Option<Format>) -> bool {
        choice.unwrap_or(if io::stdout().is_terminal() {
            Format::Text
        } else {
            Format::Json
        }) == Format::Text
    }
}

#[derive(Subcommand, Debug)]
pub(super) enum AliasCommand {
    /// Save a local name for a device.
    Set { name: String, target: String },
    /// Remove a local name for a device.
    Remove { name: String },
}

#[derive(clap::Args, Debug)]
pub(super) struct QuestionArgs {
    /// The alias or endpoint ID of the device that may answer. --to can
    /// replace it.
    #[clap(value_name = "DEVICE")]
    pub(super) device: Option<String>,
    /// The question shown on the other device. --title can replace it.
    #[clap(value_name = "QUESTION")]
    pub(super) question: Option<String>,
    /// The alias or endpoint ID of the device that may answer.
    #[clap(long)]
    pub(super) to: Option<String>,
    /// The question shown on the other device.
    #[clap(long)]
    pub(super) title: Option<String>,
    /// Details shown with the question.
    #[clap(long, default_value = "")]
    pub(super) body: String,
    /// The program or the topic that the question comes from. The default is
    /// the host name.
    #[clap(long)]
    pub(super) source: Option<String>,
    #[clap(flatten)]
    pub(super) details: DetailArgs,
    /// The time allowed for an answer, such as 10m or 2h.
    #[clap(long, value_parser = parse_interval, default_value = "10m")]
    pub(super) timeout: Duration,
    /// Print the question ID and return without waiting.
    #[clap(long)]
    pub(super) no_wait: bool,
}
#[derive(clap::Args, Debug)]
pub(super) struct AskArgs {
    #[clap(flatten)]
    pub(super) question: QuestionArgs,
    /// A value and an optional label, such as retry="Retry now". Repeat for
    /// each choice.
    #[clap(long = "choice", value_name = "VALUE[=LABEL]", value_parser = choices::parse_option, num_args = 1)]
    pub(super) choices: Vec<Choice>,
}

impl QuestionArgs {
    /// The device and the question, from the words or from the flags.
    pub(super) fn target(&mut self) -> Result<(String, String)> {
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
pub(super) struct ApprovalArgs {
    #[clap(flatten)]
    pub(super) question: QuestionArgs,
    /// Approve only in the app. The notification can still show Reject.
    #[clap(long)]
    pub(super) unlock: bool,
}
/// The agent hook settings of `yappr config`. An empty value restores the
/// default.
#[derive(clap::Args, Debug, Default)]
pub(super) struct HookConfigArgs {
    /// The device that answers agent approvals. The default is the one device
    /// that receives this feed.
    #[clap(long, value_name = "DEVICE")]
    pub(super) hook_to: Option<String>,
    /// How long an agent approval waits, such as 5m. At most 59m. Default: 2m.
    #[clap(long, value_name = "DURATION")]
    pub(super) hook_timeout: Option<String>,
    /// The agent events, as event[:matcher[:command-pattern]] entries
    /// separated by commas. Default: permission,stop.
    #[clap(long, value_name = "EVENTS")]
    pub(super) hook_events: Option<String>,
    /// When agent hooks act: away or always. away: only when the screen is
    /// locked or the computer was idle for 2 minutes. Default: away.
    #[clap(long, value_name = "WHEN")]
    pub(super) hook_when: Option<String>,
}

impl HookConfigArgs {
    pub(super) fn any(&self) -> bool {
        self.hook_to.is_some()
            || self.hook_timeout.is_some()
            || self.hook_events.is_some()
            || self.hook_when.is_some()
    }
}

#[derive(clap::Args, Debug)]
pub(super) struct HookArgs {
    #[clap(long, value_enum)]
    pub(super) agent: yappr::hooks::Agent,
    /// The device that answers. The default comes from config.
    #[clap(long)]
    pub(super) to: Option<String>,
    /// The selected events. The default comes from config.
    #[clap(long)]
    pub(super) events: Option<String>,
    /// How long to wait for an answer. The default comes from config.
    #[clap(long, value_parser = parse_interval)]
    pub(super) timeout: Option<Duration>,
    /// Deliver the feed through this entry, and read no event.
    #[clap(long, hide = true)]
    pub(super) deliver: Option<u64>,
}

#[derive(clap::Args, Debug)]
pub(super) struct KeyArgs {
    /// The name that the app shows for the machine.
    #[clap(long, default_value = "CI")]
    pub(super) name: String,
    /// A paired device, such as phone, whose address the machine needs.
    #[clap(long = "for", value_name = "DEVICE")]
    pub(super) device: Option<String>,
    /// text for people, env for KEY=value lines, json for programs.
    #[clap(long, value_enum, default_value_t = KeyOutput::Text)]
    pub(super) output: KeyOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum KeyOutput {
    Text,
    Env,
    Json,
}

#[derive(clap::Args, Debug)]
pub(super) struct StartArgs {
    /// Pair a device, also when other devices can read the feed.
    #[clap(long)]
    pub(super) pair: bool,
    /// The writer name shown on the joining device. The default comes from config.
    #[clap(long)]
    pub(super) name: Option<String>,
    /// Save the accepted device under this alias. The default comes from the
    /// device's name.
    #[clap(long)]
    pub(super) alias: Option<String>,
    /// How long the invitation stays open, such as 5m or 1h.
    #[clap(long = "timeout", alias = "expires", value_parser = parse_interval, default_value = "5m")]
    pub(super) expires: Duration,
    /// The UDP port for IPv4. With a fixed port, the ticket stays valid after
    /// a restart, also when no relay is available. The default is a random
    /// port.
    #[clap(long)]
    pub(super) port: Option<u16>,
}

#[derive(clap::Args, Debug)]
pub(super) struct PairArgs {
    /// The writer name shown on the joining device. The default comes from config.
    #[clap(long)]
    pub(super) name: Option<String>,
    /// How long the invitation stays open, such as 5m or 1h.
    #[clap(long = "timeout", alias = "expires", value_parser = parse_interval, default_value = "5m")]
    pub(super) expires: Duration,
    /// Save the accepted device under this alias. The default comes from the
    /// device's name.
    #[clap(long)]
    pub(super) alias: Option<String>,
    /// Confirm a pending device by its full endpoint ID.
    #[clap(long, conflicts_with = "reject")]
    pub(super) accept: Option<String>,
    /// Reject a pending device by its full endpoint ID.
    #[clap(long, conflicts_with = "accept")]
    pub(super) reject: Option<String>,
}

#[derive(clap::Args, Debug)]
pub(super) struct PostArgs {
    /// The program or the topic that the entry comes from. The default is the
    /// host name.
    #[clap(long)]
    pub(super) source: Option<String>,

    /// A one-line summary.
    #[clap(long)]
    pub(super) title: Option<String>,

    /// What the entry is, such as "deploy.finished". "state" means the
    /// current state of the subject.
    #[clap(long, default_value = entry::DEFAULT_KIND)]
    pub(super) kind: String,

    #[clap(flatten)]
    pub(super) details: DetailArgs,

    /// The body. If you do not give it, the body comes from stdin.
    #[clap(long)]
    pub(super) body: Option<String>,

    /// Post each line of stdin as one entry in JSON, the format of "read
    /// --output json". A field that a line does not have comes from the
    /// options.
    #[clap(long, conflicts_with = "body")]
    pub(super) json: bool,
}

/// The optional fields that post, ask and approve share.
#[derive(clap::Args, Debug)]
pub(super) struct DetailArgs {
    /// How urgent it is. Readers color "warn" and "alert".
    #[clap(long, value_parser = clap::builder::PossibleValuesParser::new(entry::LEVELS))]
    pub(super) level: Option<String>,

    /// What it is about, such as "backup/nas". Entries with the same subject
    /// belong together.
    #[clap(long)]
    pub(super) subject: Option<String>,

    /// A JSON object for programs, such as '{"version":"1.4.2"}'.
    #[clap(long, value_name = "JSON", value_parser = parse_data)]
    pub(super) data: Option<serde_json::Map<String, serde_json::Value>>,

    /// Show the body as Markdown in the app.
    #[clap(long)]
    pub(super) markdown: bool,
}

impl DetailArgs {
    pub(super) fn into_details(self) -> entry::Details {
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

pub(super) fn parse_interval(s: &str) -> Result<Duration, String> {
    let d: jiff::SignedDuration = s
        .parse()
        .map_err(|_| "use a duration such as 500ms, 2s or 1m".to_string())?;
    Duration::try_from(d)
        .ok()
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "the duration must be more than zero".into())
}

#[derive(clap::Args, Debug)]
pub(super) struct ReadArgs {
    /// Writer aliases, endpoint IDs, or feed tickets. Without a writer, read
    /// prints the feed of this machine.
    #[clap(value_name = "WRITER")]
    pub(super) ticket: Vec<String>,

    /// Start after this entry id, or after this time. The time can be a
    /// duration such as 1h or 2d, or a time such as 2026-10-27T09:30:00Z.
    #[clap(long, value_parser = parse_since, value_name = "ID|DURATION|TIME")]
    pub(super) since: Option<Since>,

    /// Start with the last N entries.
    #[clap(long, value_name = "N")]
    pub(super) tail: Option<u64>,

    /// Print only the entries with a source that matches this regex.
    #[clap(long, value_name = "REGEX")]
    pub(super) source: Option<Regex>,

    /// Print only the entries of this kind. "*" matches any text, as in
    /// "deploy.*". Use the option again for more kinds.
    #[clap(long)]
    pub(super) kind: Vec<String>,

    /// Do not print the entries of this kind, for example "log". Use the
    /// option again for more kinds.
    #[clap(long, value_name = "KIND")]
    pub(super) not_kind: Vec<String>,

    /// Print only the entries with this level or a higher level.
    #[clap(long, value_parser = clap::builder::PossibleValuesParser::new(entry::LEVELS))]
    pub(super) level: Option<String>,

    /// Print only the entries with a subject that matches this pattern, such
    /// as "backup/*".
    #[clap(long)]
    pub(super) subject: Option<String>,

    /// Of the entries that are already in the feed, print only the newest
    /// entry for each subject. Entries without a subject all print.
    #[clap(long)]
    pub(super) latest: bool,

    /// Print only the entries that still need attention: questions with no
    /// final decision, and subjects whose newest "state" entry is "warn" or
    /// "alert". From a writer, only the questions that this device can answer.
    /// The saved position does not move.
    #[clap(long, conflicts_with_all = ["since", "tail", "latest"])]
    pub(super) open: bool,

    /// The output format. The default is text for a terminal and JSON for a
    /// pipe or a file.
    #[clap(long, value_enum)]
    pub(super) output: Option<Output>,

    /// When to use color.
    #[clap(long, value_enum, default_value_t)]
    pub(super) color: ColorChoice,

    /// After the last entry, wait for new entries.
    #[clap(short, long)]
    pub(super) follow: bool,

    /// Stop after the last entry. This is the default of read.
    #[clap(long, hide = true, overrides_with = "follow")]
    pub(super) no_follow: bool,

    /// The name of the saved position. Different names have independent
    /// positions.
    #[clap(long, default_value = "default", value_name = "NAME")]
    pub(super) cursor: String,
}

#[derive(clap::Args, Debug)]
pub(super) struct OnArgs {
    /// The alias, endpoint ID, or ticket of the writer.
    #[clap(value_name = "WRITER")]
    pub(super) ticket: String,

    /// Run only for entries of this kind. "*" matches any text. Use the
    /// option again for more kinds.
    #[clap(long)]
    pub(super) kind: Vec<String>,

    /// Do not run for entries of this kind.
    #[clap(long, value_name = "KIND")]
    pub(super) not_kind: Vec<String>,

    /// Run only for entries with this level or a higher level.
    #[clap(long, value_parser = clap::builder::PossibleValuesParser::new(entry::LEVELS))]
    pub(super) level: Option<String>,

    /// Run only for entries with a subject that matches this pattern.
    #[clap(long)]
    pub(super) subject: Option<String>,

    /// Run only for entries with a source that matches this regex.
    #[clap(long, value_name = "REGEX")]
    pub(super) source: Option<Regex>,

    /// When the command fails, print the error and continue with the next
    /// entry, so the failed entry does not run again.
    #[clap(long)]
    pub(super) skip_failed: bool,

    /// The name of the saved position.
    #[clap(long, default_value = "on", value_name = "NAME")]
    pub(super) cursor: String,

    /// The command and its arguments. No shell reads them.
    #[clap(last = true, required = true, value_name = "COMMAND")]
    pub(super) command: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Since {
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
