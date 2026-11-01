//! The serve side of the `yappr/feed/1` protocol.
use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream, VarInt},
    protocol::{AcceptError, ProtocolHandler, Router},
    EndpointAddr, EndpointId, SecretKey,
};
use n0_future::task::AbortOnDropHandle;
use tokio::sync::{watch, Mutex};

use crate::{
    access::{AccessHook, DENIED_CODE, DENIED_REASON},
    entry::Entry,
    net::{self, NetOptions},
    proto::{self, Frame, Request, Stored},
    store::{LogReader, Store},
    ALPN,
};

/// How often the server checks the log for new entries.
const POLL: Duration = Duration::from_millis(100);
/// How long the server waits for a follower to close a finished connection.
const LINGER: Duration = Duration::from_secs(10);
/// How many entries to read from disk in one step.
const BATCH: usize = 256;
/// How often each connection checks that its follower is still allowed.
const ACCESS_CHECK: Duration = Duration::from_secs(1);
/// How long a follower has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The follower lost access while it was reading.
#[derive(Debug)]
struct Revoked;

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the follower is no longer in the allow list")
    }
}

impl std::error::Error for Revoked {}

/// A running feed server.
#[derive(Debug)]
pub struct Server {
    router: Router,
    _deliveries: AbortOnDropHandle<()>,
}

impl Server {
    /// Start to serve `store`, and deliver its entries to the devices that
    /// receive them.
    pub async fn spawn(
        store: Store,
        secret: SecretKey,
        net: impl Into<NetOptions>,
    ) -> Result<Self> {
        let feed = Feed::open(store.clone()).await?;
        let hook = AccessHook::new(store.allow_list());
        let endpoint = net::bind(secret, net, vec![], Some(hook)).await?;
        let router = Router::builder(endpoint.clone())
            .accept(
                crate::control::ALPN,
                crate::control::ChoicesProtocol(store.clone()),
            )
            .accept(
                crate::pairing::ALPN,
                crate::control::PairProtocol(store.clone()),
            )
            .accept(ALPN, FeedProtocol { feed: feed.clone() })
            .spawn();
        let deliveries = AbortOnDropHandle::new(tokio::spawn(crate::deliver::keep_delivering(
            endpoint, feed,
        )));
        Ok(Self {
            router,
            _deliveries: deliveries,
        })
    }

    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
    }

    /// The address of the server, after it knows at least one way to reach it.
    pub async fn addr(&self) -> EndpointAddr {
        net::addr(self.endpoint()).await
    }

    /// Stop the server.
    pub async fn shutdown(self) {
        self.router.shutdown().await.ok();
    }
}

/// The entries of a store for other devices, with a watch on the id of the
/// last entry.
#[derive(Debug, Clone)]
pub struct Feed {
    store: Store,
    head: watch::Receiver<u64>,
    watcher: Arc<Mutex<Watcher>>,
    _poll: Arc<AbortOnDropHandle<()>>,
}

impl Feed {
    /// Start to watch `store` for new entries.
    pub async fn open(store: Store) -> Result<Self> {
        let mut watcher = Watcher {
            reader: Some(store.reader()?),
            head: watch::Sender::new(0),
        };
        watcher.sync().await?;
        let head = watcher.head.subscribe();
        let watcher = Arc::new(Mutex::new(watcher));
        let poll = AbortOnDropHandle::new(tokio::spawn(poll(watcher.clone())));
        Ok(Self {
            store,
            head,
            watcher,
            _poll: Arc::new(poll),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Read a request from `recv` and send the entries to `remote`, while
    /// `remote` is in the allow list. With `acks`, the frames that `remote`
    /// sends after its request go to `acks`.
    pub async fn answer(
        &self,
        conn: &Connection,
        mut send: SendStream,
        recv: RecvStream,
        acks: Option<watch::Sender<Stored>>,
    ) {
        let remote = conn.remote_id();
        // Close the connection when the follower loses access, also while the
        // server waits for its request or for a slow replay.
        let allow_list = self.store.allow_list();
        let guarded = conn.clone();
        let _guard = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                tokio::time::sleep(ACCESS_CHECK).await;
                if !allow_list.contains(&remote) {
                    guarded.close(VarInt::from_u32(DENIED_CODE), DENIED_REASON);
                    return;
                }
            }
        }));
        if let Err(e) = self.serve(&mut send, recv, remote, acks).await {
            tracing::debug!("{remote}: {e:#}");
            if e.downcast_ref::<Revoked>().is_some() {
                // The follower reads this code as "denied" and does not reconnect.
                conn.close(VarInt::from_u32(DENIED_CODE), DENIED_REASON);
                return;
            }
            let frame = Frame::Error {
                message: format!("{e:#}"),
            };
            proto::write_frame(&mut send, &frame, proto::MAX_FRAME)
                .await
                .ok();
        }
        send.finish().ok();
    }
}

/// Reads new entries from the log and publishes the id of the last one (the
/// head). Followers only read up to the head, and a change of the head wakes
/// the followers that wait for new entries.
#[derive(Debug)]
struct Watcher {
    reader: Option<LogReader>,
    head: watch::Sender<u64>,
}

async fn poll(watcher: Arc<Mutex<Watcher>>) {
    loop {
        tokio::time::sleep(POLL).await;
        if let Err(e) = watcher.lock().await.sync().await {
            tracing::warn!("cannot read the feed: {e:#}");
        }
    }
}

impl Watcher {
    async fn sync(&mut self) -> Result<()> {
        loop {
            let reader = self.reader.take().context("reader is gone")?;
            let (reader, batch) = read_batch(reader).await?;
            self.reader = Some(reader);
            let Some(last) = batch.last().map(|e| e.id) else {
                return Ok(());
            };
            self.head.send_replace(last);
        }
    }
}

/// Read up to [`BATCH`] entries without blocking the runtime.
async fn read_batch(mut reader: LogReader) -> Result<(LogReader, Vec<Entry>)> {
    tokio::task::spawn_blocking(move || {
        let mut batch = Vec::new();
        while batch.len() < BATCH {
            match reader.next_entry()? {
                Some(entry) => batch.push(entry),
                None => break,
            }
        }
        Ok((reader, batch))
    })
    .await?
}

/// Entries from the log, but never past the published head.
struct Cursor {
    reader: Option<LogReader>,
    pending: VecDeque<Entry>,
}

impl Cursor {
    async fn next_upto(&mut self, head: u64) -> Result<Option<Entry>> {
        if self.pending.is_empty() {
            let reader = self.reader.take().context("reader is gone")?;
            let (reader, batch) = read_batch(reader).await?;
            self.reader = Some(reader);
            self.pending.extend(batch);
        }
        match self.pending.front() {
            Some(entry) if entry.id <= head => Ok(self.pending.pop_front()),
            _ => Ok(None),
        }
    }
}

#[derive(Debug, Clone)]
struct FeedProtocol {
    feed: Feed,
}

impl ProtocolHandler for FeedProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let (send, recv) = conn.accept_bi().await?;
        self.feed.answer(&conn, send, recv, None).await;
        // The follower closes the connection after it reads the last frame.
        tokio::time::timeout(LINGER, conn.closed()).await.ok();
        Ok(())
    }
}

impl Feed {
    async fn serve(
        &self,
        send: &mut SendStream,
        mut recv: RecvStream,
        remote: EndpointId,
        acks: Option<watch::Sender<Stored>>,
    ) -> Result<()> {
        let req: Request = tokio::time::timeout(
            REQUEST_TIMEOUT,
            proto::read_frame(&mut recv, proto::MAX_REQUEST),
        )
        .await
        .context("the follower sent no request in time")??
        .context("no request")?;
        tracing::debug!("request {req:?}");
        if !self.store.allow_list().contains(&remote) {
            return Err(Revoked.into());
        }
        let _acks = acks.map(|acks| {
            AbortOnDropHandle::new(tokio::spawn(async move {
                while let Ok(Some(stored)) =
                    proto::read_frame::<Stored>(&mut recv, proto::MAX_REQUEST).await
                {
                    acks.send_replace(stored);
                }
            }))
        });
        // Include every entry that is on disk now, not only the entries that
        // the last poll saw.
        self.watcher.lock().await.sync().await?;
        let mut head_rx = self.head.clone();
        let mut head = *head_rx.borrow_and_update();
        let mut cursor = Cursor {
            reader: Some(self.store.reader()?),
            pending: VecDeque::new(),
        };
        let filter = proto::Filter::new(&req)?;
        let mut log = self.store.log()?;
        proto::write_frame(send, &Frame::Feed { log: log.clone() }, proto::MAX_FRAME).await?;
        // A position in another log does not apply to this log.
        let since = match &req.log {
            Some(theirs) if Some(theirs) != log.as_ref() => 0,
            _ => req.since,
        };
        let wanted = |e: &Entry| {
            e.id > since && req.since_time.is_none_or(|t| e.time >= t) && filter.matches(e)
        };
        let mut budget = req.limit.unwrap_or(u64::MAX);

        // Replay up to the current head.
        let mut tail = VecDeque::new();
        while let Some(entry) = cursor.next_upto(head).await? {
            if !wanted(&entry) {
                continue;
            }
            match req.tail {
                Some(n) => {
                    tail.push_back(entry);
                    if tail.len() as u64 > n {
                        tail.pop_front();
                    }
                }
                None => {
                    if !send_entry(send, entry, &mut budget).await? {
                        return Ok(());
                    }
                }
            }
        }
        for entry in tail {
            if !send_entry(send, entry, &mut budget).await? {
                return Ok(());
            }
        }
        proto::write_frame(send, &Frame::CaughtUp { head }, proto::MAX_FRAME).await?;
        if !req.follow {
            return Ok(());
        }

        // Live tail. A follower that loses access gets no more entries. The
        // connection guard closes the connection too.
        let allow_list = self.store.allow_list();
        loop {
            tokio::select! {
                changed = head_rx.changed() => {
                    if changed.is_err() {
                        return Ok(()); // the server stops
                    }
                }
                _ = send.stopped() => return Ok(()), // the follower left
            }
            if !allow_list.contains(&remote) {
                return Err(Revoked.into());
            }
            head = *head_rx.borrow_and_update();
            if log.is_none() {
                log = self.store.log()?;
                proto::write_frame(send, &Frame::Feed { log: log.clone() }, proto::MAX_FRAME)
                    .await?;
            }
            while let Some(entry) = cursor.next_upto(head).await? {
                if wanted(&entry) && !send_entry(send, entry, &mut budget).await? {
                    return Ok(());
                }
            }
        }
    }
}

/// Send one entry. Returns false when the limit is reached.
async fn send_entry(send: &mut SendStream, entry: Entry, budget: &mut u64) -> Result<bool> {
    if *budget == 0 {
        return Ok(false);
    }
    proto::write_frame(send, &Frame::Entry { entry }, proto::MAX_FRAME).await?;
    *budget -= 1;
    Ok(*budget > 0)
}
