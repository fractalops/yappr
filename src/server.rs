//! The serve side of the `feed/0` protocol.
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
    proto::{self, Frame, Request},
    store::{LogReader, Store},
    ALPN,
};

/// How often the server checks the log for new entries.
const POLL: Duration = Duration::from_millis(100);
/// How long the server waits for a follower to close a finished connection.
const LINGER: Duration = Duration::from_secs(10);
/// How many entries to read from disk in one step.
const BATCH: usize = 256;
/// How often a live follower's access is checked when no entries arrive.
const RECHECK: Duration = Duration::from_secs(5);

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
    _poll: AbortOnDropHandle<()>,
}

impl Server {
    /// Start to serve `store`.
    pub async fn spawn(
        store: Store,
        secret: SecretKey,
        net: impl Into<NetOptions>,
    ) -> Result<Self> {
        let mut watcher = Watcher {
            reader: Some(store.reader()?),
            head: watch::Sender::new(0),
        };
        watcher.sync().await?;
        let head = watcher.head.subscribe();
        let watcher = Arc::new(Mutex::new(watcher));
        let poll = AbortOnDropHandle::new(tokio::spawn(poll(watcher.clone())));

        let hook = AccessHook::new(store.allow_list());
        let endpoint = net::bind(secret, net, vec![], Some(hook)).await?;
        let router = Router::builder(endpoint)
            .accept(
                crate::control::ALPN,
                crate::control::ChoicesProtocol(store.clone()),
            )
            .accept(
                crate::pairing::ALPN,
                crate::control::PairProtocol(store.clone()),
            )
            .accept(
                ALPN,
                FeedProtocol {
                    store,
                    head,
                    watcher,
                },
            )
            .spawn();
        Ok(Self {
            router,
            _poll: poll,
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
    store: Store,
    head: watch::Receiver<u64>,
    watcher: Arc<Mutex<Watcher>>,
}

impl ProtocolHandler for FeedProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();
        let (mut send, mut recv) = conn.accept_bi().await?;
        if let Err(e) = self.serve(&mut send, &mut recv, remote).await {
            tracing::debug!("{remote}: {e:#}");
            if e.downcast_ref::<Revoked>().is_some() {
                // The follower reads this code as "denied" and does not reconnect.
                conn.close(VarInt::from_u32(DENIED_CODE), DENIED_REASON);
                return Ok(());
            }
            let frame = Frame::Error {
                message: format!("{e:#}"),
            };
            proto::write_frame(&mut send, &frame, proto::MAX_FRAME)
                .await
                .ok();
        }
        send.finish().ok();
        // The follower closes the connection after it reads the last frame.
        tokio::time::timeout(LINGER, conn.closed()).await.ok();
        Ok(())
    }
}

impl FeedProtocol {
    async fn serve(
        &self,
        send: &mut SendStream,
        recv: &mut RecvStream,
        remote: EndpointId,
    ) -> Result<()> {
        let req: Request = proto::read_frame(recv, proto::MAX_REQUEST)
            .await?
            .context("no request")?;
        tracing::debug!("request {req:?}");
        // Include every entry that is on disk now, not only the entries that
        // the last poll saw.
        self.watcher.lock().await.sync().await?;
        let mut head_rx = self.head.clone();
        let mut head = *head_rx.borrow_and_update();
        let mut cursor = Cursor {
            reader: Some(self.store.reader()?),
            pending: VecDeque::new(),
        };
        let wanted = |e: &Entry| e.id > req.since && req.since_time.is_none_or(|t| e.time >= t);
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

        // Live tail. A follower that loses access stops at the next check.
        let allow_list = self.store.allow_list();
        let mut recheck = tokio::time::interval(RECHECK);
        loop {
            tokio::select! {
                changed = head_rx.changed() => {
                    if changed.is_err() {
                        return Ok(()); // the server stops
                    }
                }
                _ = recheck.tick() => {}
                _ = send.stopped() => return Ok(()), // the follower left
            }
            if !allow_list.contains(&remote) {
                return Err(Revoked.into());
            }
            head = *head_rx.borrow_and_update();
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
    proto::write_frame(send, &Frame::Entry { entry }, proto::MAX_FRAME).await?;
    *budget -= 1;
    Ok(*budget > 0)
}
