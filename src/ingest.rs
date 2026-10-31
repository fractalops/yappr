//! Streaming input for `post`: one entry per line, lines grouped by time, or
//! one JSON entry per line.
use std::{
    fmt,
    future::Future,
    io::{self, BufRead, BufReader, Read, Write},
    time::Duration,
};

use anyhow::Result;
use tokio::{sync::mpsc, time::Instant};

use crate::{
    entry::{self, Draft, Invalid},
    store::Store,
};

/// How `post` turns input lines into entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Each line is the body of one entry.
    Lines,
    /// The lines that arrive within the interval are the body of one entry.
    Every(Duration),
    /// Each line is one entry as a JSON object.
    Json,
}

/// Some input lines were not valid, so they are not in the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub lines: usize,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "skipped {} input lines that are not valid", self.lines)
    }
}

impl std::error::Error for Rejected {}

/// One message from the reader thread.
enum Input {
    Line(usize, String),
    Bad(usize, Invalid),
    Failed(io::Error),
}

/// Post entries from `input` until it ends or `stop` completes. Write the id
/// of each new entry to `out`, and each skipped line to `err`.
///
/// Fields that a line does not give come from `defaults`. In [`Mode::Every`],
/// the lines that wait for the end of the interval are posted at the end of
/// the input and when `stop` completes.
pub async fn run(
    store: &Store,
    mode: Mode,
    defaults: &Draft,
    input: impl Read + Send + 'static,
    out: &mut impl Write,
    err: &mut impl Write,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(256);
    // A plain thread, because a blocked read must not keep the runtime alive
    // at exit.
    std::thread::spawn(move || read_lines(input, tx));

    let mut post = Poster {
        store,
        out,
        err,
        rejected: 0,
    };
    let mut batch = Batch::default();
    tokio::pin!(stop);
    loop {
        let deadline = batch.deadline;
        tokio::select! {
            _ = &mut stop => break,
            _ = sleep_until(deadline), if deadline.is_some() => post.batch(&mut batch, defaults)?,
            input = rx.recv() => match input {
                None => break,
                Some(Input::Failed(e)) => return Err(e.into()),
                Some(Input::Bad(n, invalid)) => post.skip(&format!("line {n}"), &invalid, 1)?,
                Some(Input::Line(n, line)) => match mode {
                    Mode::Lines => {
                        let draft = Draft { body: line, ..defaults.clone() };
                        post.one(draft, &format!("line {n}"), 1)?;
                    }
                    Mode::Json => match Draft::from_json(&line, defaults) {
                        Ok(draft) => post.one(draft, &format!("line {n}"), 1)?,
                        Err(invalid) => post.skip(&format!("line {n}"), &invalid, 1)?,
                    },
                    Mode::Every(interval) => {
                        if batch.len + line.len() + 1 > entry::MAX_BODY {
                            post.batch(&mut batch, defaults)?;
                        }
                        batch.push(n, line, interval);
                    }
                },
            },
        }
    }
    post.batch(&mut batch, defaults)?;
    match post.rejected {
        0 => Ok(()),
        lines => Err(Rejected { lines }.into()),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Lines that wait for the end of the interval.
#[derive(Debug, Default)]
struct Batch {
    lines: Vec<String>,
    first: usize,
    last: usize,
    len: usize,
    deadline: Option<Instant>,
}

impl Batch {
    fn push(&mut self, n: usize, line: String, interval: Duration) {
        if self.lines.is_empty() {
            self.first = n;
            self.deadline = Some(Instant::now() + interval);
        }
        self.last = n;
        self.len += line.len() + 1;
        self.lines.push(line);
    }
}

struct Poster<'a, O, E> {
    store: &'a Store,
    out: &'a mut O,
    err: &'a mut E,
    rejected: usize,
}

impl<O: Write, E: Write> Poster<'_, O, E> {
    /// Append one draft. An invalid draft is skipped. Other errors stop.
    fn one(&mut self, draft: Draft, label: &str, lines: usize) -> Result<()> {
        match self.store.append(draft) {
            Ok(entry) => {
                writeln!(self.out, "{}", entry.id)?;
                self.out.flush()?;
                Ok(())
            }
            Err(e) => match e.downcast_ref::<Invalid>() {
                Some(invalid) => self.skip(label, invalid, lines),
                None => Err(e),
            },
        }
    }

    fn skip(&mut self, label: &str, invalid: &Invalid, lines: usize) -> Result<()> {
        writeln!(self.err, "skipped {label}: {invalid}")?;
        self.rejected += lines;
        Ok(())
    }

    /// Post the waiting lines as one entry, if there are any.
    fn batch(&mut self, batch: &mut Batch, defaults: &Draft) -> Result<()> {
        let batch = std::mem::take(batch);
        if batch.lines.is_empty() {
            return Ok(());
        }
        let label = match batch.first == batch.last {
            true => format!("line {}", batch.first),
            false => format!("lines {}-{}", batch.first, batch.last),
        };
        let draft = Draft {
            body: batch.lines.join("\n"),
            ..defaults.clone()
        };
        self.one(draft, &label, batch.lines.len())
    }
}

/// Read lines on a blocking thread. A line is at most [`entry::MAX_BODY`]
/// bytes, so a line with no end cannot use all the memory. Blank lines are
/// skipped.
fn read_lines(input: impl Read, tx: mpsc::Sender<Input>) {
    let mut reader = BufReader::new(input);
    let limit = entry::MAX_BODY as u64 + 2; // the line, "\r" and "\n"
    let mut n = 0;
    loop {
        n += 1;
        let mut buf = Vec::new();
        let read = (&mut reader).take(limit).read_until(b'\n', &mut buf);
        let msg = match read {
            Ok(0) => return,
            Err(e) => Input::Failed(e),
            Ok(_) if !buf.ends_with(b"\n") && buf.len() as u64 == limit => {
                // Skip the rest of a line that is too long.
                if let Err(e) = reader.skip_until(b'\n') {
                    let _ = tx.blocking_send(Input::Failed(e));
                    return;
                }
                Input::Bad(
                    n,
                    Invalid::TooLong {
                        field: "line",
                        len: buf.len(),
                        max: entry::MAX_BODY,
                    },
                )
            }
            Ok(_) => {
                if buf.ends_with(b"\n") {
                    buf.pop();
                    if buf.ends_with(b"\r") {
                        buf.pop();
                    }
                }
                if buf.is_empty() {
                    continue;
                }
                match String::from_utf8(buf) {
                    Ok(line) => Input::Line(n, line),
                    Err(e) => Input::Bad(
                        n,
                        Invalid::NotUtf8 {
                            field: "line",
                            offset: e.utf8_error().valid_up_to(),
                        },
                    ),
                }
            }
        };
        let failed = matches!(msg, Input::Failed(_));
        if tx.blocking_send(msg).is_err() || failed {
            return;
        }
    }
}
