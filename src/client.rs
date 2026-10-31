//! The follow side: subscribe to a feed.
use std::{fmt, time::Duration};

use anyhow::{bail, Context, Result};
use iroh::{
    endpoint::{Connection, ConnectionError, RecvStream, VarInt},
    Endpoint, EndpointAddr, EndpointId,
};

use crate::{
    proto::{self, Frame, Request, DENIED_CODE},
    ALPN,
};

/// The writer does not allow this follower.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub follower: EndpointId,
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the writer does not allow this endpoint. On the writer, run: yappr allow {}",
            self.follower
        )
    }
}

impl std::error::Error for Denied {}

/// How long the follower tries to connect before it stops.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The follower cannot connect to the writer.
#[derive(Debug)]
pub struct Unreachable {
    pub writer: EndpointId,
    pub reason: String,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot reach the writer {}: {}. Make sure that \"yappr serve\" runs on the writer",
            self.writer, self.reason
        )
    }
}

impl std::error::Error for Unreachable {}

/// An open request to a feed.
#[derive(Debug)]
pub struct Subscription {
    conn: Connection,
    recv: RecvStream,
    me: EndpointId,
    /// The id of the last entry: the request's `since` until an entry arrives.
    last: u64,
}

/// The writer sent an entry id that is not greater than the id before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutOfOrder {
    pub previous: u64,
    pub got: u64,
}

impl fmt::Display for OutOfOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the writer sent entry {} after entry {}. Entry ids must increase",
            self.got, self.previous
        )
    }
}

impl std::error::Error for OutOfOrder {}

impl Subscription {
    /// The next frame, or `None` when the server ends the stream.
    ///
    /// Each entry is checked against the entry rules, and its id must be greater
    /// than the id before it.
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        let frame = proto::read_frame(&mut self.recv, proto::MAX_FRAME)
            .await
            .map_err(|e| explain(&self.conn, self.me, e))?;
        match frame {
            Some(Frame::Entry { entry }) => {
                entry
                    .validate()
                    .with_context(|| format!("entry {} from the writer is not valid", entry.id))?;
                if entry.id <= self.last {
                    return Err(OutOfOrder {
                        previous: self.last,
                        got: entry.id,
                    }
                    .into());
                }
                self.last = entry.id;
                Ok(Some(Frame::Entry { entry }))
            }
            Some(Frame::Error { message }) => {
                bail!("the writer sent an error: {}", proto::printable(&message))
            }
            other => Ok(other),
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.conn.close(VarInt::from_u32(0), b"done");
    }
}

/// Connect to the feed at `addr` and send `req`.
pub async fn subscribe(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    req: &Request,
) -> Result<Subscription> {
    let me = endpoint.id();
    let conn = connect(endpoint, addr, ALPN).await?;
    let opened = async {
        let (mut send, recv) = conn.open_bi().await?;
        proto::write_frame(&mut send, req, proto::MAX_REQUEST).await?;
        send.finish()?;
        anyhow::Ok(recv)
    }
    .await;
    let recv = opened.map_err(|e| explain(&conn, me, e))?;
    Ok(Subscription {
        conn,
        recv,
        me,
        last: req.since,
    })
}

async fn connect(endpoint: &Endpoint, addr: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
    let writer = addr.id;
    #[cfg(wasm_browser)]
    let addr = crate::net::browser_addr(addr);
    match n0_future::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, alpn)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(Unreachable {
            writer,
            reason: format!("{e:#}"),
        }
        .into()),
        Err(_) => Err(Unreachable {
            writer,
            reason: format!("no answer in {}s", CONNECT_TIMEOUT.as_secs()),
        }
        .into()),
    }
}

/// Replace a stream error with [`Denied`] if the writer closed the
/// connection because this endpoint is not in its allow list.
fn explain(conn: &Connection, me: EndpointId, err: anyhow::Error) -> anyhow::Error {
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(close))
            if close.error_code == VarInt::from_u32(DENIED_CODE) =>
        {
            Denied { follower: me }.into()
        }
        _ => err,
    }
}

/// What a [`Follower`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The next entry. Entries arrive in id order, with no duplicates.
    Entry(crate::entry::Entry),
    /// The replay is complete. `head` is the last entry id of the feed.
    CaughtUp { head: u64 },
    /// The connection failed. The follower tries again after `delay`.
    Retrying { delay: Duration, error: String },
}

/// Follows a feed across reconnects.
///
/// After each entry, the follower asks for the entries after that one, so a
/// new connection continues with no duplicates and no gaps. Without
/// `follow`, it ends after the replay.
#[derive(Debug)]
pub struct Follower {
    endpoint: Endpoint,
    addr: EndpointAddr,
    req: Request,
    sub: Option<Subscription>,
    backoff: Duration,
    sleep: Option<Duration>,
}

/// The first delay before a new connection.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// The longest delay before a new connection.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

impl Follower {
    pub fn new(endpoint: Endpoint, addr: EndpointAddr, req: Request) -> Self {
        Self {
            endpoint,
            addr,
            req,
            sub: None,
            backoff: MIN_BACKOFF,
            sleep: None,
        }
    }

    /// A handle for observing the active connection without reading its stream.
    pub fn connection(&self) -> Option<Connection> {
        self.sub.as_ref().map(|sub| sub.conn.clone())
    }

    /// The endpoint that this follower uses.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The next event, or `None` when the follower is done. An error that a
    /// new connection does not fix (for example [`Denied`]) ends it.
    pub async fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some(delay) = self.sleep.take() {
                n0_future::time::sleep(delay).await;
            }
            let sub = match &mut self.sub {
                Some(sub) => sub,
                None => match subscribe(&self.endpoint, self.addr.clone(), &self.req).await {
                    Ok(sub) => self.sub.insert(sub),
                    Err(e) => return self.retry(e).map(Some),
                },
            };
            match sub.next().await {
                Ok(Some(Frame::Entry { entry })) => {
                    self.req.since = entry.id;
                    self.req.tail = None;
                    return Ok(Some(Event::Entry(entry)));
                }
                Ok(Some(Frame::CaughtUp { head })) => {
                    // The writer checked every entry up to `head`, also the
                    // entries that the filter left out.
                    self.req.since = self.req.since.max(head);
                    self.backoff = MIN_BACKOFF;
                    return Ok(Some(Event::CaughtUp { head }));
                }
                Ok(Some(_)) => continue,
                Ok(None) => {
                    self.sub = None;
                    if !self.req.follow {
                        return Ok(None);
                    }
                    let e = anyhow::anyhow!("the writer ended the stream");
                    return self.retry(e).map(Some);
                }
                Err(e) => {
                    self.sub = None;
                    return self.retry(e).map(Some);
                }
            }
        }
    }

    /// Plan a new connection, or give up if a new connection cannot help.
    fn retry(&mut self, err: anyhow::Error) -> Result<Event> {
        if !self.req.follow || is_final(&err) {
            return Err(err);
        }
        let delay = self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        self.sleep = Some(delay);
        Ok(Event::Retrying {
            delay,
            error: format!("{err:#}"),
        })
    }
}

/// Errors that a new connection does not fix.
pub fn is_final(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Denied>().is_some()
        || err.downcast_ref::<crate::entry::Invalid>().is_some()
        || err.downcast_ref::<OutOfOrder>().is_some()
}
