//! `post --json`: one entry for each JSON line of the input.
use std::{
    fmt,
    future::Future,
    io::{self, BufRead, BufReader, Read, Write},
};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::{
    entry::{self, Draft, Invalid},
    store::Store,
};

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

/// Post one entry for each JSON line of `input`, until the input ends or
/// `stop` completes. Write the id of each new entry to `out`, and each skipped
/// line to `err`. Fields that a line does not give come from `defaults`.
pub async fn run(
    store: &Store,
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
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            input = rx.recv() => match input {
                None => break,
                Some(Input::Failed(e)) => return Err(e.into()),
                Some(Input::Bad(n, invalid)) => post.skip(&format!("line {n}"), &invalid)?,
                Some(Input::Line(n, line)) => match Draft::from_json(&unwrap_writer(&line), defaults) {
                    Ok(draft) => post.one(draft, &format!("line {n}"))?,
                    Err(invalid) => post.skip(&format!("line {n}"), &invalid)?,
                },
            },
        }
    }
    match post.rejected {
        0 => Ok(()),
        lines => Err(Rejected { lines }.into()),
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
    fn one(&mut self, draft: Draft, label: &str) -> Result<()> {
        match self.store.append(draft) {
            Ok(entry) => {
                writeln!(self.out, "{}", entry.id)?;
                self.out.flush()?;
                Ok(())
            }
            Err(e) => match e.downcast_ref::<Invalid>() {
                Some(invalid) => self.skip(label, invalid),
                None => Err(e),
            },
        }
    }

    fn skip(&mut self, label: &str, invalid: &Invalid) -> Result<()> {
        writeln!(self.err, "skipped {label}: {invalid}")?;
        self.rejected += 1;
        Ok(())
    }
}

/// The entry of a line from `read --output json` with several writers. That
/// output wraps each entry as `{"writer": ..., "entry": {...}}`. Other lines
/// stay as they are.
fn unwrap_writer(line: &str) -> std::borrow::Cow<'_, str> {
    if let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str(line) {
        if map.len() == 2 && map.contains_key("writer") {
            if let Some(entry @ serde_json::Value::Object(_)) = map.remove("entry") {
                return std::borrow::Cow::Owned(entry.to_string());
            }
        }
    }
    std::borrow::Cow::Borrowed(line)
}

/// Read lines on a blocking thread. A line is at most [`entry::MAX_ENTRY`]
/// bytes, the limit of one entry as JSON, so a line with no end cannot use
/// all the memory. Blank lines are skipped.
fn read_lines(input: impl Read, tx: mpsc::Sender<Input>) {
    let mut reader = BufReader::new(input);
    let limit = entry::MAX_ENTRY as u64 + 2; // the line, "\r" and "\n"
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
                        max: entry::MAX_ENTRY,
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
