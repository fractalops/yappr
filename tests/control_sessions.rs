//! Reuse real iroh sessions without weakening per-request access checks.
use iroh::{
    endpoint::{Connection, VarInt},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, SecretKey,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use yappr::{
    control::{self, ControlClient, Reply, Request},
    net::{self, Network},
    proto,
    store::Store,
};

#[derive(Debug, Clone)]
struct Counted {
    store: Store,
    accepted: Arc<AtomicUsize>,
    connections: Arc<Mutex<Vec<Connection>>>,
}
impl ProtocolHandler for Counted {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        self.accepted.fetch_add(1, Ordering::SeqCst);
        self.connections.lock().unwrap().push(conn.clone());
        control::ChoicesProtocol(self.store.clone())
            .accept(conn)
            .await
    }
}
async fn endpoint() -> Endpoint {
    net::bind(SecretKey::generate(), Network::Local, vec![], None)
        .await
        .unwrap()
}
async fn setup() -> (tempfile::TempDir, Store, Router, ControlClient, Counted) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let client = ControlClient::new(endpoint().await);
    store.allow_list().allow(client.endpoint().id()).unwrap();
    let handler = Counted {
        store: store.clone(),
        accepted: Arc::default(),
        connections: Arc::default(),
    };
    let router = Router::builder(endpoint().await)
        .accept(control::ALPN, handler.clone())
        .spawn();
    (dir, store, router, client, handler)
}
fn request() -> Request {
    Request::Status {
        question_id: "0".repeat(64),
    }
}
#[tokio::test]
async fn requests_share_one_session_and_revocation_applies_to_open_sessions() {
    let (_dir, store, router, client, handler) = setup().await;
    let addr = net::addr(router.endpoint()).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let client = client.clone();
        let addr = addr.clone();
        tasks.spawn(async move {
            client
                .exchange::<_, Reply>(addr, control::ALPN, &request())
                .await
                .unwrap()
        });
    }
    while let Some(reply) = tasks.join_next().await {
        // The question does not exist. Access is still allowed.
        assert!(matches!(reply.unwrap(), Reply::Rejected { error } if error.code == "not_found"));
    }
    assert_eq!(handler.accepted.load(Ordering::SeqCst), 1);
    store.allow_list().deny(&client.endpoint().id()).unwrap();
    let reply: Reply = client
        .exchange(addr, control::ALPN, &request())
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Rejected { error } if error.code == "denied"));
    assert_eq!(handler.accepted.load(Ordering::SeqCst), 1);
    client.close().await;
    client.endpoint().close().await;
    router.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_closed_session_reconnects() {
    let (_dir, _store, router, client, handler) = setup().await;
    let addr = net::addr(router.endpoint()).await;
    let _: Reply = client
        .exchange(addr.clone(), control::ALPN, &request())
        .await
        .unwrap();
    let first = handler.connections.lock().unwrap()[0].clone();
    first.close(VarInt::from_u32(0), b"test reconnect");
    first.closed().await;
    let _: Reply = client
        .exchange(addr, control::ALPN, &request())
        .await
        .unwrap();
    assert_eq!(handler.accepted.load(Ordering::SeqCst), 2);
    client.close().await;
    client.endpoint().close().await;
    router.shutdown().await.unwrap();
}
#[tokio::test]
async fn one_slow_request_does_not_block_another_stream() {
    let (_dir, _store, router, client, _handler) = setup().await;
    let conn = client
        .endpoint()
        .connect(net::addr(router.endpoint()).await, control::ALPN)
        .await
        .unwrap();
    let (mut slow, _recv) = conn.open_bi().await.unwrap();
    slow.write_all(&[0]).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    proto::write_frame(&mut send, &request(), proto::MAX_REQUEST)
        .await
        .unwrap();
    send.finish().unwrap();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        proto::read_frame::<Reply>(&mut recv, proto::MAX_FRAME),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(matches!(reply, Reply::Rejected { .. }));
    conn.close(VarInt::from_u32(0), b"test complete");
    client.endpoint().close().await;
    router.shutdown().await.unwrap();
}
