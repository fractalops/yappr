//! The `yappr/deliver/1` protocol: the writer opens the connection and
//! delivers its feed to a device that receives it, such as the app.
//!
//! The writer opens one bidirectional stream and sends a [`Hello`]. The
//! device sends a feed [`Request`] on the same stream. From there, the stream
//! is the same as in `yappr/feed/1`, and the device also sends a [`Stored`]
//! frame each time it saved entries. To answer a question, the device opens
//! more streams on the same connection with `yappr/choices/1` requests.
use anyhow::{bail, Context, Result};
use iroh::{endpoint::Connection, EndpointId, SecretKey, Signature};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    client::Subscription,
    proto::{self, Request},
};

pub const ALPN: &[u8] = b"yappr/deliver/1";

/// How long a [`Grant`] is valid.
pub const GRANT_LIFETIME: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

const GRANT_DOMAIN: &[u8] = b"yappr/grant/1\0";

/// How long a device waits for the writer's [`Hello`] on a new connection.
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The first frame from the writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// The identity of the feed.
    pub writer: EndpointId,
    /// Without a grant, the connection must come from `writer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
}

impl Hello {
    /// A delivery from the writer's own endpoint.
    pub fn direct(writer: EndpointId) -> Self {
        Self {
            writer,
            grant: None,
        }
    }

    /// A delivery from the endpoint `transport` of this process. Each process
    /// has its own endpoint, so several processes of one writer can deliver
    /// at the same time.
    pub fn granted(writer: &SecretKey, transport: EndpointId) -> Self {
        Self {
            writer: writer.public(),
            grant: Some(Grant::new(
                writer,
                transport,
                Timestamp::now() + GRANT_LIFETIME,
            )),
        }
    }

    /// Check that the connection from `remote` speaks for the writer.
    pub fn check(&self, remote: EndpointId, now: Timestamp) -> Result<()> {
        match &self.grant {
            None if remote == self.writer => Ok(()),
            None => bail!("the delivery comes from another endpoint than the writer"),
            Some(grant) => grant.verify(self.writer, remote, now),
        }
    }
}

/// The writer's permission for the endpoint `transport` to deliver the
/// writer's feed until `expires_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub transport: EndpointId,
    pub expires_at: Timestamp,
    pub sig: String,
}

impl Grant {
    pub fn new(writer: &SecretKey, transport: EndpointId, expires_at: Timestamp) -> Self {
        let bytes = grant_bytes(writer.public(), transport, expires_at);
        Self {
            transport,
            expires_at,
            sig: data_encoding::HEXLOWER.encode(&writer.sign(&bytes).to_bytes()),
        }
    }

    fn verify(&self, writer: EndpointId, remote: EndpointId, now: Timestamp) -> Result<()> {
        if self.transport != remote {
            bail!("the grant is for another endpoint");
        }
        if self.expires_at <= now {
            bail!("the grant expired");
        }
        let bytes = data_encoding::HEXLOWER
            .decode(self.sig.as_bytes())
            .context("the grant signature is not valid")?;
        let sig =
            Signature::try_from(bytes.as_slice()).context("the grant signature is not valid")?;
        writer
            .verify(&grant_bytes(writer, self.transport, self.expires_at), &sig)
            .context("the grant signature is not valid")
    }
}

/// The domain, the writer, the transport, and the deadline in Unix seconds
/// (8 bytes, big endian).
fn grant_bytes(writer: EndpointId, transport: EndpointId, expires_at: Timestamp) -> Vec<u8> {
    let mut bytes = GRANT_DOMAIN.to_vec();
    bytes.extend(writer.as_bytes());
    bytes.extend(transport.as_bytes());
    bytes.extend(expires_at.as_second().to_be_bytes());
    bytes
}

/// A delivery that this device accepted, before it asks for entries.
#[derive(Debug)]
pub struct Incoming {
    conn: Connection,
    me: EndpointId,
    writer: EndpointId,
    streams: (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
}

/// Read the writer's [`Hello`] on a new connection to `me`, and check that
/// the connection speaks for the writer. The caller then checks that it
/// allows the writer.
pub async fn accept(conn: Connection, me: EndpointId) -> Result<Incoming> {
    let hello = async {
        let (send, mut recv) = conn.accept_bi().await?;
        let hello: Hello = proto::read_frame(&mut recv, proto::MAX_REQUEST)
            .await?
            .context("the writer sent no hello")?;
        anyhow::Ok((hello, send, recv))
    };
    // A connection that sends no hello holds a delivery slot, so it gets
    // little time.
    let (hello, send, recv) = n0_future::time::timeout(HELLO_TIMEOUT, hello)
        .await
        .context("the writer sent no hello in time")??;
    hello.check(conn.remote_id(), Timestamp::now())?;
    Ok(Incoming {
        conn,
        me,
        writer: hello.writer,
        streams: (send, recv),
    })
}

impl Incoming {
    /// The identity of the feed.
    pub fn writer(&self) -> EndpointId {
        self.writer
    }

    /// Close the connection with the code for a device that is not allowed.
    pub fn deny(self) {
        self.conn
            .close(proto::DENIED_CODE.into(), proto::DENIED_REASON);
    }

    /// Ask for the entries in `req`. Read them from the returned
    /// [`Subscription`], and report saved entries with
    /// [`Subscription::stored`].
    pub async fn subscribe(self, req: &Request) -> Result<Subscription> {
        Subscription::delivered(self.conn, self.streams, self.me, self.writer, req).await
    }
}

#[cfg(not(wasm_browser))]
pub use writer::{deliver, deliver_until_stopped, keep_delivering, NotAllowed};

/// The writer side: open connections to the devices that receive the feed.
#[cfg(not(wasm_browser))]
mod writer {
    use std::{collections::HashMap, time::Duration};

    use anyhow::{bail, Context, Result};
    use iroh::{
        endpoint::{ConnectionError, VarInt},
        Endpoint, EndpointAddr,
    };
    use n0_future::task::AbortOnDropHandle;
    use tokio::sync::watch;

    use super::{Hello, ALPN};
    use crate::{
        client::Denied,
        peers,
        proto::{self, Stored},
        server::Feed,
    };

    /// How long the writer tries to connect to a device.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// The first delay before a new delivery to a device that was not
    /// reachable.
    const MIN_BACKOFF: Duration = Duration::from_secs(1);
    /// The longest delay before a new delivery.
    const MAX_BACKOFF: Duration = Duration::from_secs(300);
    /// How long the writer waits for the device to close a finished stream.
    const LINGER: Duration = Duration::from_secs(10);
    /// How often a running writer reads its list of devices again.
    const RECEIVERS_CHECK: Duration = Duration::from_secs(5);

    /// The device is not in this writer's allow list, so the writer does not
    /// deliver to it.
    #[derive(Debug)]
    pub struct NotAllowed {
        pub device: iroh::EndpointId,
    }

    impl std::fmt::Display for NotAllowed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "the device {} is not allowed to read this feed. Run: yappr allow {}",
                self.device.fmt_short(),
                self.device
            )
        }
    }

    impl std::error::Error for NotAllowed {}

    /// Deliver the feed to the device at `addr`. With `until`, return after
    /// the device saved that entry. Without it, return when the connection
    /// ends. `progress` gets each [`Stored`] report of the device.
    pub async fn deliver(
        endpoint: &Endpoint,
        feed: &Feed,
        addr: EndpointAddr,
        hello: &Hello,
        until: Option<u64>,
        progress: &watch::Sender<Stored>,
    ) -> Result<()> {
        let device = addr.id;
        if !feed.store().allow_list().contains(&device) {
            return Err(NotAllowed { device }.into());
        }
        let addr = crate::net::relay_addr(addr);
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN))
            .await
            .map_err(|_| anyhow::anyhow!("no answer in {}s", CONNECT_TIMEOUT.as_secs()))??;
        let result = async {
            let (mut send, recv) = conn.open_bi().await?;
            proto::write_frame(&mut send, hello, proto::MAX_REQUEST).await?;
            let (_answers, mut replies) =
                crate::control::answer_tracked(conn.clone(), feed.store().clone());
            let mut saved = None;
            let (acks, mut acked) = watch::channel(Stored::default());
            let serving = feed.answer(&conn, send, recv, Some(acks));
            tokio::pin!(serving);
            loop {
                tokio::select! {
                    _ = &mut serving => {
                        // The stream ends when the device leaves or fails.
                        tokio::time::timeout(LINGER, conn.closed()).await.ok();
                        break;
                    }
                    changed = acked.changed() => {
                        if changed.is_err() {
                            continue;
                        }
                        saved = Some(acked.borrow_and_update().clone());
                    }
                    ready = async { replies.wait_for(|count| *count == 0).await.map(|_| ()) },
                        if saved.is_some() =>
                    {
                        ready.context("control replies stopped")?;
                        // A caller may shut the endpoint down as soon as it sees
                        // this report. Keep serving the feed while replies drain.
                        let stored = saved.take().expect("a saved-entry report is pending");
                        progress.send_replace(stored.clone());
                        let log = feed.store().log()?;
                        if until.is_some_and(|n| stored.through >= n && stored.log == log) {
                            return Ok(());
                        }
                    }
                }
            }
            match conn.close_reason() {
                Some(ConnectionError::ApplicationClosed(close))
                    if close.error_code == VarInt::from_u32(proto::DENIED_CODE) =>
                {
                    Err(Denied { follower: device }.into())
                }
                _ if until.is_some() => bail!("the device closed the connection"),
                _ => Ok(()),
            }
        }
        .await;
        conn.close(VarInt::from_u32(0), b"delivered");
        result.with_context(|| format!("deliver to {}", device.fmt_short()))
    }

    /// Deliver to the device at `addr` again and again, with a growing delay
    /// after each failure, until the caller stops the future.
    pub async fn deliver_until_stopped(
        endpoint: Endpoint,
        feed: Feed,
        addr: EndpointAddr,
        hello: impl Fn() -> Hello,
        progress: watch::Sender<Stored>,
    ) {
        let notifications = async {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let saved = progress.borrow().clone();
                crate::push::notify(feed.store(), addr.id, &saved).await;
            }
        };
        let delivery = async {
            let mut backoff = MIN_BACKOFF;
            loop {
                let started = tokio::time::Instant::now();
                if let Err(e) =
                    deliver(&endpoint, &feed, addr.clone(), &hello(), None, &progress).await
                {
                    tracing::debug!("{e:#}");
                }
                // A delivery that ran for some time was not a failed connection.
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = MIN_BACKOFF;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        };
        tokio::join!(delivery, notifications);
    }

    /// Deliver the feed to each allowed device that receives it, for as long
    /// as the future runs. A running writer uses it.
    pub async fn keep_delivering(endpoint: Endpoint, feed: Feed) {
        let mut running: HashMap<iroh::EndpointId, AbortOnDropHandle<()>> = HashMap::new();
        loop {
            let receivers = receivers(&feed).unwrap_or_else(|e| {
                tracing::warn!("cannot read the devices that receive this feed: {e:#}");
                Vec::new()
            });
            running.retain(|id, _| receivers.iter().any(|addr| addr.id == *id));
            for addr in receivers {
                running.entry(addr.id).or_insert_with(|| {
                    let writer = endpoint.id();
                    AbortOnDropHandle::new(tokio::spawn(deliver_until_stopped(
                        endpoint.clone(),
                        feed.clone(),
                        addr,
                        move || Hello::direct(writer),
                        watch::channel(Stored::default()).0,
                    )))
                });
            }
            tokio::time::sleep(RECEIVERS_CHECK).await;
        }
    }

    /// The allowed devices that receive the feed.
    fn receivers(feed: &Feed) -> Result<Vec<EndpointAddr>> {
        let allowed = feed.store().allow_list().load()?;
        Ok(peers::receivers(feed.store().dir())?
            .into_iter()
            .map(|(_, addr)| addr)
            .filter(|addr| allowed.contains(&addr.id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_speaks_for_the_writer_only_from_its_endpoint_and_in_time() {
        let writer = SecretKey::generate();
        let transport = SecretKey::generate().public();
        let hello = Hello::granted(&writer, transport);
        let now = Timestamp::now();
        hello.check(transport, now).unwrap();
        assert!(hello.check(SecretKey::generate().public(), now).is_err());
        assert!(hello.check(transport, now + GRANT_LIFETIME).is_err());

        let mut forged = hello.clone();
        forged.writer = SecretKey::generate().public();
        assert!(forged.check(transport, now).is_err());
        let mut longer = hello;
        longer.grant.as_mut().unwrap().expires_at = now + GRANT_LIFETIME * 2;
        assert!(longer.check(transport, now).is_err());

        let direct = Hello::direct(writer.public());
        direct.check(writer.public(), now).unwrap();
        assert!(direct.check(transport, now).is_err());
    }
}
