//! Bounded request streams on reusable authenticated iroh connections.
use std::{collections::BTreeMap, sync::Arc};

use crate::{
    choices::{ChoiceError, Decision, QuestionState, Submission},
    proto,
};
use anyhow::{Context, Result};
use iroh::{
    endpoint::{Connection, ConnectionError, VarInt},
    Endpoint, EndpointAddr, EndpointId,
};
use n0_future::time::{Duration, Instant};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::Mutex;

pub const ALPN: &[u8] = b"choices/0";
const MAX_CONTROL: usize = 16 * 1024;
const IDLE: Duration = Duration::from_secs(60);
const MAX_CONNECTIONS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Submit { submission: Submission },
    Status { question_id: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Accepted { decision: Decision },
    State { state: Box<QuestionState> },
    Rejected { error: ChoiceError },
}

/// The writer side: answers choice and pairing requests.
#[cfg(not(wasm_browser))]
mod writer {
    use iroh::{
        endpoint::{Connection, RecvStream, SendStream, VarInt},
        protocol::{AcceptError, ProtocolHandler},
        EndpointId,
    };
    use tokio::task::JoinSet;

    use super::{Duration, Reply, Request, IDLE, MAX_CONTROL};
    use crate::{
        choices::{self, ChoiceError},
        pairing::{self, Join, PairReply},
        proto,
        store::Store,
    };
    use anyhow::{Context, Result};

    const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
    const MAX_STREAMS: usize = 16;

    #[derive(Debug, Clone)]
    pub struct ChoicesProtocol(pub Store);
    impl ProtocolHandler for ChoicesProtocol {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            serve(conn, self.0.clone(), false).await;
            Ok(())
        }
    }
    #[derive(Debug, Clone)]
    pub struct PairProtocol(pub Store);
    impl ProtocolHandler for PairProtocol {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            serve(conn, self.0.clone(), true).await;
            Ok(())
        }
    }

    async fn serve(conn: Connection, store: Store, pair: bool) {
        let mut requests = JoinSet::new();
        loop {
            // Bound concurrent work. New streams remain flow-controlled by QUIC.
            while requests.len() >= MAX_STREAMS {
                requests.join_next().await;
            }
            let accepted = n0_future::time::timeout(IDLE, conn.accept_bi()).await;
            let Ok(Ok((send, recv))) = accepted else {
                break;
            };
            while requests.try_join_next().is_some() {}
            let store = store.clone();
            let remote = conn.remote_id();
            requests.spawn(async move {
                match n0_future::time::timeout(
                    REQUEST_TIMEOUT,
                    handle(send, recv, store, remote, pair),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::debug!(%error, "control stream failed"),
                    Err(error) => tracing::debug!(%error, "control stream timed out"),
                }
            });
        }
        // A stream can commit a decision even when its caller loses the response.
        while requests.join_next().await.is_some() {}
        conn.close(VarInt::from_u32(0), b"control idle or closed");
    }

    async fn handle(
        mut send: SendStream,
        mut recv: RecvStream,
        store: Store,
        remote: EndpointId,
        pair: bool,
    ) -> Result<()> {
        if pair {
            let request: Join = proto::read_frame(&mut recv, MAX_CONTROL)
                .await?
                .context("no pairing request")?;
            let reply = tokio::task::spawn_blocking(move || pairing::join(&store, remote, request))
                .await?
                .unwrap_or_else(|_| PairReply {
                    status: "rejected".into(),
                    message:
                        "The invitation expired or is not valid. Create a new invitation on the writer."
                            .into(),
                });
            proto::write_frame(&mut send, &reply, MAX_CONTROL).await?;
        } else {
            let req: Request = proto::read_frame(&mut recv, MAX_CONTROL)
                .await?
                .context("no choice request")?;
            let result = tokio::task::spawn_blocking(move || match req {
                Request::Submit { submission } => choices::decide(&store, remote, submission)
                    .map(|decision| Reply::Accepted { decision }),
                Request::Status { question_id } => {
                    if !store.allow_list().contains(&remote) {
                        return Err(
                            ChoiceError::new("denied", "this device cannot read the feed").into(),
                        );
                    }
                    choices::get(&store.entries()?, &question_id).map(|state| Reply::State {
                        state: Box::new(state),
                    })
                }
            })
            .await?;
            let reply = result.unwrap_or_else(|error| Reply::Rejected {
                error: public_error(error),
            });
            proto::write_frame(&mut send, &reply, proto::MAX_FRAME).await?;
        }
        send.finish()?;
        Ok(())
    }
    fn public_error(error: anyhow::Error) -> ChoiceError {
        if let Some(error) = error.downcast_ref::<ChoiceError>() {
            return error.clone();
        }
        tracing::warn!("choice storage error: {error:#}");
        ChoiceError::new(
            "storage",
            "the writer could not save the decision. Try again",
        )
    }
}
#[cfg(not(wasm_browser))]
pub use writer::{ChoicesProtocol, PairProtocol};

type PoolKey = (EndpointId, Vec<u8>);
#[derive(Debug)]
struct Slot {
    connection: Option<Connection>,
    used: Instant,
}
impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(conn) = &self.connection {
            conn.close(VarInt::from_u32(0), b"control client closed");
        }
    }
}
/// Share this client across pairing polls, question queries, and outbox retries.
#[derive(Debug, Clone)]
pub struct ControlClient {
    endpoint: Endpoint,
    connections: Arc<Mutex<BTreeMap<PoolKey, Arc<Mutex<Slot>>>>>,
}
impl ControlClient {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            connections: Arc::default(),
        }
    }
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub async fn close(&self) {
        self.connections.lock().await.clear();
    }
    async fn slot(&self, key: PoolKey) -> Arc<Mutex<Slot>> {
        let mut connections = self.connections.lock().await;
        if !connections.contains_key(&key) && connections.len() >= MAX_CONNECTIONS {
            // Cache size is bounded. Eviction closes a dormant session.
            if let Some(old) = connections.iter().find_map(|(key, value)| {
                value
                    .try_lock()
                    .ok()
                    .filter(|slot| slot.used.elapsed() > IDLE)
                    .map(|_| key.clone())
            }) {
                connections.remove(&old);
            }
            // Short-lived uncached sessions preserve active streams at capacity.
            if connections.len() >= MAX_CONNECTIONS {
                return Arc::new(Mutex::new(Slot {
                    connection: None,
                    used: Instant::now(),
                }));
            }
        }
        connections
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Mutex::new(Slot {
                    connection: None,
                    used: Instant::now(),
                }))
            })
            .clone()
    }
    async fn connection(
        &self,
        slot: &mut Slot,
        addr: EndpointAddr,
        alpn: &[u8],
    ) -> Result<Connection> {
        let stale = slot.used.elapsed() > IDLE
            || slot
                .connection
                .as_ref()
                .is_some_and(|c| c.close_reason().is_some());
        if stale {
            if let Some(conn) = slot.connection.take() {
                conn.close(VarInt::from_u32(0), b"replace stale control session");
            }
        }
        slot.used = Instant::now();
        if let Some(conn) = &slot.connection {
            return Ok(conn.clone());
        }
        #[cfg(wasm_browser)]
        let addr = crate::net::browser_addr(addr);
        let conn = self.endpoint.connect(addr, alpn).await?;
        slot.connection = Some(conn.clone());
        Ok(conn)
    }
    /// Requests in the Yappr control protocols are idempotent. A closed session
    /// can be replaced once using the same request and submission ID.
    pub async fn exchange<Q: Serialize, A: DeserializeOwned>(
        &self,
        addr: EndpointAddr,
        alpn: &[u8],
        request: &Q,
    ) -> Result<A> {
        n0_future::time::timeout(Duration::from_secs(15), async {
            let shared = self.slot((addr.id, alpn.to_vec())).await;
            let mut slot = shared.lock().await;
            for attempt in 0..2 {
                let conn = self.connection(&mut slot, addr.clone(), alpn).await?;
                let result = async {
                    let (mut send, mut recv) = conn.open_bi().await?;
                    proto::write_frame(&mut send, request, MAX_CONTROL).await?;
                    send.finish()?;
                    proto::read_frame::<A>(&mut recv, proto::MAX_FRAME).await?.context("the writer sent no response")
                }.await;
                match result {
                    Ok(value) => return Ok(value),
                    Err(error) => {
                        let denied = matches!(conn.close_reason(), Some(ConnectionError::ApplicationClosed(ref close)) if close.error_code == VarInt::from_u32(crate::proto::DENIED_CODE));
                        let closed = conn.close_reason().is_some();
                        if slot.connection.as_ref().is_some_and(|c| c.stable_id() == conn.stable_id()) {
                            slot.connection.take();
                        }
                        conn.close(VarInt::from_u32(0), b"control request failed");
                        if denied { return Err(ChoiceError::new("denied", "this device cannot read the feed").into()); }
                        if attempt == 1 || !closed { return Err(error); }
                    }
                }
            }
            unreachable!()
        }).await.context("the writer did not respond within 15 seconds")?
    }
    pub async fn submit(&self, addr: EndpointAddr, submission: Submission) -> Result<Decision> {
        match self
            .exchange(addr, ALPN, &Request::Submit { submission })
            .await?
        {
            Reply::Accepted { decision } => Ok(decision),
            Reply::Rejected { error } => Err(error.into()),
            _ => anyhow::bail!("the writer sent an unexpected response"),
        }
    }
}

/// A one-shot exchange for callers that do not keep a session.
pub async fn exchange<Q: Serialize, A: DeserializeOwned>(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    alpn: &[u8],
    request: &Q,
) -> Result<A> {
    ControlClient::new(endpoint.clone())
        .exchange(addr, alpn, request)
        .await
}
pub async fn submit(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    submission: Submission,
) -> Result<Decision> {
    ControlClient::new(endpoint.clone())
        .submit(addr, submission)
        .await
}
