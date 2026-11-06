//! Reading feeds, rendering entries, and acknowledging successful handlers.

use std::{
    io::{self, IsTerminal, Write},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use jiff::tz::TimeZone;
use yappr::{
    client::{Event, Follower},
    cursor::Cursor,
    entry::{self, Entry},
    net, peers,
    proto::Request,
    render::{self, Output, Renderer},
    store::Store,
};

use super::{
    args::{CommonArgs, OnArgs, ReadArgs, Since},
    is_broken_pipe, shown,
};

/// Prints the entries that pass the filters, and moves the cursor.
pub(super) async fn on(common: &CommonArgs, args: OnArgs) -> Result<()> {
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

pub(super) async fn read(common: &CommonArgs, mut args: ReadArgs) -> Result<()> {
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
