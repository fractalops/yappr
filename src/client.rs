//! The reading side: subscribe to a feed.
use std::{fmt, time::Duration};

use anyhow::{bail, Context, Result};
use iroh::{
    endpoint::{Connection, ConnectionError, RecvStream, SendStream, VarInt},
    Endpoint, EndpointAddr, EndpointId,
};

use crate::{
    entry::BadSignature,
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
            "cannot reach the writer {}: {}. Make sure that \"yappr start\" runs on the writer",
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
    /// The stream for [`Subscription::stored`], in a delivery.
    send: Option<SendStream>,
    me: EndpointId,
    writer: EndpointId,
    /// The log of the request's position.
    asked: Option<String>,
    /// The name of the writer's log, from [`Frame::Feed`].
    log: Option<String>,
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
    /// A request on a stream that a writer opened to deliver its feed. See
    /// [`crate::deliver`].
    pub(crate) async fn delivered(
        conn: Connection,
        (mut send, recv): (SendStream, RecvStream),
        me: EndpointId,
        writer: EndpointId,
        req: &Request,
    ) -> Result<Self> {
        proto::write_frame(&mut send, req, proto::MAX_REQUEST).await?;
        Ok(Self {
            me,
            conn,
            recv,
            send: Some(send),
            writer,
            asked: req.log.clone(),
            log: None,
            last: req.since,
        })
    }

    /// Tell the writer that this device saved the entries up to `through`
    /// in the current log. Only a delivery has a stream for it.
    pub async fn stored(&mut self, through: u64) -> Result<()> {
        let Some(send) = &mut self.send else {
            bail!("only a delivery can report saved entries");
        };
        let stored = proto::Stored {
            log: self.log.clone(),
            through,
        };
        proto::write_frame(send, &stored, proto::MAX_REQUEST).await
    }

    /// The connection of this request.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The name of the writer's log, after the writer sent it.
    pub fn log(&self) -> Option<&str> {
        self.log.as_deref()
    }

    /// The next frame, or `None` when the server ends the stream.
    ///
    /// Each entry is checked against the entry rules and against its
    /// signature, and its id must be greater than the id before it.
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        let frame = proto::read_frame(&mut self.recv, proto::MAX_FRAME)
            .await
            .map_err(|e| explain(&self.conn, self.me, e))?;
        match frame {
            Some(Frame::Feed { log }) => {
                // A log name is the time of the first entry. Anything else
                // could break the reader's saved position and its requests.
                if log
                    .as_deref()
                    .is_some_and(|l| l.len() > 64 || l.parse::<jiff::Timestamp>().is_err())
                {
                    bail!("the writer sent a log name that is not a time");
                }
                // The writer starts again at the first entry of a new log.
                if self.asked.is_some() && self.asked != log {
                    self.last = 0;
                }
                self.asked = log.clone();
                self.log = log.clone();
                Ok(Some(Frame::Feed { log }))
            }
            Some(Frame::Entry { entry }) => {
                entry
                    .validate()
                    .with_context(|| format!("entry {} from the writer is not valid", entry.id))?;
                if entry.sig.is_some() {
                    let log = self.log.as_deref().ok_or(BadSignature { id: entry.id })?;
                    entry.verify(&self.writer, log)?;
                }
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
    let writer = addr.id;
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
        send: None,
        me,
        writer,
        asked: req.log.clone(),
        log: None,
        last: req.since,
    })
}

async fn connect(endpoint: &Endpoint, addr: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
    let writer = addr.id;
    let addr = crate::net::relay_addr(addr);
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
// Almost every value is an entry, so a box would save no memory.
#[allow(clippy::large_enum_variant)]
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

    /// The name of the writer's log, after the writer sent it. Save it with
    /// a position, and give it in the next request.
    pub fn log(&self) -> Option<&str> {
        self.req.log.as_deref()
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
                Ok(Some(Frame::Feed { log })) => {
                    if self.req.log.is_some() && self.req.log != log {
                        self.req.since = 0;
                    }
                    self.req.log = log;
                }
                Ok(Some(Frame::Entry { entry })) => {
                    self.req.since = entry.id;
                    self.req.tail = None;
                    return Ok(Some(Event::Entry(entry)));
                }
                Ok(Some(Frame::CaughtUp { head })) => {
                    // The writer checked every entry up to `head`, also the
                    // entries that the filter left out. After a reconnect,
                    // the replay starts there, with no tail.
                    self.req.since = self.req.since.max(head);
                    self.req.tail = None;
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
        || err.downcast_ref::<BadSignature>().is_some()
        || err.downcast_ref::<OutOfOrder>().is_some()
}
