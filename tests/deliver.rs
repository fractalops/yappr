//! A writer delivers its feed to a device that receives it, on loopback.
use std::time::Duration;

use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::{sync::watch, time::timeout};
use yappr::{
    choices::{self, Submission},
    client::{Denied, Subscription},
    control,
    deliver::{self, Hello},
    entry::{Draft, Entry},
    net::{self, Network},
    peers,
    proto::{Frame, Request, Stored},
    server::{Feed, Server},
    store::Store,
    EndpointTicket,
};

const WAIT: Duration = Duration::from_secs(10);

fn draft(body: &str) -> Draft {
    Draft {
        source: "ci".into(),
        kind: "event".into(),
        level: None,
        subject: None,
        format: None,
        data: None,
        title: None,
        body: body.into(),
        extra: Default::default(),
    }
}

struct Writer {
    _dir: tempfile::TempDir,
    secret: SecretKey,
    store: Store,
}

impl Writer {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let secret = SecretKey::generate();
        let store = Store::open(dir.path()).unwrap().signed_by(secret.clone());
        Self {
            _dir: dir,
            secret,
            store,
        }
    }
}

/// A device that receives deliveries. It saves each entry, then reports it.
struct Device {
    endpoint: Endpoint,
    entries: watch::Receiver<Vec<Entry>>,
    /// The subscription of the last delivery, to answer on its connection.
    last: watch::Receiver<Option<iroh::endpoint::Connection>>,
    _task: n0_future::task::AbortOnDropHandle<()>,
}

impl Device {
    async fn start(allowed: EndpointId) -> Self {
        let endpoint = net::bind(
            SecretKey::generate(),
            Network::Local,
            vec![deliver::ALPN.to_vec()],
            None,
        )
        .await
        .unwrap();
        let (entries_tx, entries) = watch::channel(Vec::new());
        let (last_tx, last) = watch::channel(None);
        let accepting = endpoint.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = accepting.accept().await {
                let Ok(conn) = incoming.await else { continue };
                let Ok(incoming) = deliver::accept(conn, accepting.id()).await else {
                    continue;
                };
                if incoming.writer() != allowed {
                    incoming.deny();
                    continue;
                }
                let since = entries_tx.borrow().last().map_or(0, |e: &Entry| e.id);
                let req = Request {
                    since,
                    ..Default::default()
                };
                let Ok(sub) = incoming.subscribe(&req).await else {
                    continue;
                };
                last_tx.send_replace(Some(sub.connection().clone()));
                let entries_tx = entries_tx.clone();
                tokio::spawn(receive(sub, entries_tx));
            }
        });
        Self {
            endpoint,
            entries,
            last,
            _task: n0_future::task::AbortOnDropHandle::new(task),
        }
    }

    async fn addr(&self) -> EndpointAddr {
        net::addr(&self.endpoint).await
    }

    async fn wait_for(&mut self, n: usize) -> Vec<Entry> {
        timeout(WAIT, self.entries.wait_for(|e| e.len() >= n))
            .await
            .expect("timed out")
            .unwrap()
            .clone()
    }
}

async fn receive(mut sub: Subscription, entries: watch::Sender<Vec<Entry>>) {
    while let Ok(Some(frame)) = sub.next().await {
        match frame {
            Frame::Entry { entry } => {
                let id = entry.id;
                entries.send_modify(|e| e.push(entry));
                if sub.stored(id).await.is_err() {
                    return;
                }
            }
            Frame::CaughtUp { head } if sub.stored(head).await.is_err() => return,
            _ => {}
        }
    }
}

async fn transport() -> Endpoint {
    net::bind(SecretKey::generate(), Network::Local, vec![], None)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_writer_delivers_until_the_device_saved_the_entry() {
    let writer = Writer::new();
    let mut device = Device::start(writer.secret.public()).await;
    writer
        .store
        .allow_list()
        .allow(device.endpoint.id())
        .unwrap();
    writer.store.append(draft("one")).unwrap();
    let two = writer.store.append(draft("two")).unwrap();

    let endpoint = net::bind(writer.secret.clone(), Network::Local, vec![], None)
        .await
        .unwrap();
    let feed = Feed::open(writer.store.clone()).await.unwrap();
    let (progress, seen) = watch::channel(Stored::default());
    timeout(
        WAIT,
        deliver::deliver(
            &endpoint,
            &feed,
            device.addr().await,
            &Hello::direct(writer.secret.public()),
            Some(two.id),
            &progress,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let got = device.wait_for(2).await;
    assert_eq!(
        got.iter().map(|e| e.body.as_str()).collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(seen.borrow().through, 2);
    assert_eq!(seen.borrow().log, writer.store.log().unwrap());
}

#[tokio::test]
async fn a_short_lived_endpoint_delivers_with_a_grant() {
    let writer = Writer::new();
    let mut device = Device::start(writer.secret.public()).await;
    writer
        .store
        .allow_list()
        .allow(device.endpoint.id())
        .unwrap();
    let entry = writer.store.append(draft("from ci")).unwrap();
    let feed = Feed::open(writer.store.clone()).await.unwrap();
    let progress = watch::channel(Stored::default()).0;

    // Without a grant, the endpoint does not speak for the writer.
    let endpoint = transport().await;
    let without = Hello::direct(writer.secret.public());
    let err = timeout(
        WAIT,
        deliver::deliver(
            &endpoint,
            &feed,
            device.addr().await,
            &without,
            Some(entry.id),
            &progress,
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(device.entries.borrow().is_empty(), "{err:#}");

    let hello = Hello::granted(&writer.secret, endpoint.id());
    timeout(
        WAIT,
        deliver::deliver(
            &endpoint,
            &feed,
            device.addr().await,
            &hello,
            Some(entry.id),
            &progress,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(device.wait_for(1).await[0].body, "from ci");
}

#[tokio::test]
async fn a_device_refuses_a_writer_that_it_does_not_allow() {
    let writer = Writer::new();
    let device = Device::start(SecretKey::generate().public()).await;
    writer
        .store
        .allow_list()
        .allow(device.endpoint.id())
        .unwrap();
    let entry = writer.store.append(draft("one")).unwrap();
    let endpoint = transport().await;
    let feed = Feed::open(writer.store.clone()).await.unwrap();
    let err = timeout(
        WAIT,
        deliver::deliver(
            &endpoint,
            &feed,
            device.addr().await,
            &Hello::granted(&writer.secret, endpoint.id()),
            Some(entry.id),
            &watch::channel(Stored::default()).0,
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(err.downcast_ref::<Denied>().is_some(), "{err:#}");
}

#[tokio::test]
async fn a_writer_delivers_only_to_an_allowed_device() {
    let writer = Writer::new();
    let device = Device::start(writer.secret.public()).await;
    let entry = writer.store.append(draft("private")).unwrap();
    let endpoint = transport().await;
    let feed = Feed::open(writer.store.clone()).await.unwrap();
    let result = timeout(
        WAIT,
        deliver::deliver(
            &endpoint,
            &feed,
            device.addr().await,
            &Hello::granted(&writer.secret, endpoint.id()),
            Some(entry.id),
            &watch::channel(Stored::default()).0,
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(device.entries.borrow().is_empty());
}

#[tokio::test]
async fn the_device_answers_on_the_delivery_connection() {
    let writer = Writer::new();
    let mut device = Device::start(writer.secret.public()).await;
    writer
        .store
        .allow_list()
        .allow(device.endpoint.id())
        .unwrap();
    let question = choices::create(
        &writer.store,
        choices::new_question(
            "Deploy?".into(),
            String::new(),
            "ci".into(),
            choices::approval_options(),
            device.endpoint.id(),
            None,
            true,
        ),
    )
    .unwrap();
    let endpoint = transport().await;
    let feed = Feed::open(writer.store.clone()).await.unwrap();
    let addr = device.addr().await;
    let secret = writer.secret.clone();
    let delivering = tokio::spawn(async move {
        deliver::deliver_until_stopped(
            endpoint.clone(),
            feed,
            addr,
            move || Hello::granted(&secret, endpoint.id()),
            watch::channel(Stored::default()).0,
        )
        .await
    });
    device.wait_for(1).await;
    let conn = device.last.borrow().clone().unwrap();
    let reply: control::Reply = control::exchange_on(
        &conn,
        &control::Request::Submit {
            submission: Submission {
                id: choices::random_id(),
                question_id: question.id.clone(),
                value: "approve".into(),
            },
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(reply, control::Reply::Accepted { .. }),
        "{reply:?}"
    );
    let state = choices::get(&writer.store.entries().unwrap(), &question.id).unwrap();
    assert_eq!(state.status, "answered");
    // The decision reaches the device too.
    assert_eq!(device.wait_for(2).await[1].kind, "decision");
    delivering.abort();
}

#[tokio::test]
async fn a_running_writer_delivers_new_entries_to_its_receivers() {
    let writer = Writer::new();
    let mut device = Device::start(writer.secret.public()).await;
    writer
        .store
        .allow_list()
        .allow(device.endpoint.id())
        .unwrap();
    let ticket = EndpointTicket::new(device.addr().await);
    peers::set_receiver(writer.store.dir(), "phone", &ticket).unwrap();
    let server = Server::spawn(writer.store.clone(), writer.secret.clone(), Network::Local)
        .await
        .unwrap();
    writer.store.append(draft("first")).unwrap();
    device.wait_for(1).await;
    writer.store.append(draft("second")).unwrap();
    assert_eq!(device.wait_for(2).await[1].body, "second");
    server.shutdown().await;
}

/// Run the built binary with the feed in `dir`, on loopback.
async fn yappr(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_yappr"))
        .args(args)
        .env("YAPPR_DIR", dir)
        .env("YAPPR_NETWORK", "local")
        .env_remove("IROH_SECRET")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

/// A feed directory with a device that receives its entries.
async fn cli_writer() -> (tempfile::TempDir, Device) {
    let dir = tempfile::tempdir().unwrap();
    let id = yappr(dir.path(), &["id"]).await;
    let writer: EndpointId = String::from_utf8(id.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let device = Device::start(writer).await;
    let allowed = yappr(dir.path(), &["allow", &device.endpoint.id().to_string()]).await;
    assert!(allowed.status.success());
    let ticket = EndpointTicket::new(device.addr().await);
    peers::set_receiver(dir.path(), "phone", &ticket).unwrap();
    (dir, device)
}

#[tokio::test]
async fn post_delivers_to_the_receivers_when_no_writer_runs() {
    let (dir, mut device) = cli_writer().await;
    let out = yappr(dir.path(), &["post", "--body", "deployed"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "1\n");
    assert_eq!(device.wait_for(1).await[0].body, "deployed");

    let status = yappr(dir.path(), &["status", "--output", "json"]).await;
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["peers"][0]["receives"], true);

    drop(device);
    let out = yappr(dir.path(), &["post", "--body", "lost"]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "{stderr}");
    assert!(stderr.contains("phone did not confirm entry 2"), "{stderr}");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "2\n");
}

#[tokio::test]
async fn ask_delivers_the_question_and_takes_the_answer_from_the_device() {
    let (dir, mut device) = cli_writer().await;
    let path = dir.path().to_owned();
    let asking = tokio::spawn(async move {
        yappr(
            &path,
            &[
                "ask", "phone", "Deploy?", "--choice", "yes", "--choice", "no",
            ],
        )
        .await
    });
    let entry = device.wait_for(1).await.remove(0);
    let question = choices::question(&entry).expect("a question entry");
    let conn = device.last.borrow().clone().unwrap();
    let reply: control::Reply = control::exchange_on(
        &conn,
        &control::Request::Submit {
            submission: Submission {
                id: choices::random_id(),
                question_id: question.id,
                value: "yes".into(),
            },
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(reply, control::Reply::Accepted { .. }),
        "{reply:?}"
    );
    let out = timeout(WAIT, asking).await.unwrap().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "yes\n");
    assert_eq!(device.wait_for(2).await[1].kind, "decision");
}
