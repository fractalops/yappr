//! Writer and follower in one process, on loopback, with no relays.
use std::time::Duration;

use iroh::{Endpoint, SecretKey};
use tokio::time::timeout;
use yappr::{
    client::{self, Denied, Event, Follower, Subscription},
    entry::{Draft, Entry},
    net::{self, NetOptions, Network},
    proto::{Frame, Request},
    server::Server,
    store::Store,
};

const WAIT: Duration = Duration::from_secs(10);

fn draft(source: &str, body: &str) -> Draft {
    Draft {
        source: source.into(),
        kind: "info".into(),
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
    store: Store,
    secret: SecretKey,
    server: Server,
}

impl Writer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("feed")).unwrap();
        let secret = SecretKey::generate();
        let server = Server::spawn(store.clone(), secret.clone(), Network::Local)
            .await
            .unwrap();
        Self {
            _dir: dir,
            store,
            secret,
            server,
        }
    }

    fn post(&self, source: &str, body: &str) -> Entry {
        self.store.append(draft(source, body)).unwrap()
    }

    fn allow(&self, follower: &Endpoint) {
        self.store.allow_list().allow(follower.id()).unwrap();
    }

    async fn restart(self) -> Self {
        self.server.shutdown().await;
        let server = Server::spawn(self.store.clone(), self.secret.clone(), Network::Local)
            .await
            .unwrap();
        Self { server, ..self }
    }
}

async fn follower() -> Endpoint {
    net::bind(SecretKey::generate(), Network::Local, vec![], None)
        .await
        .unwrap()
}

async fn subscribe(writer: &Writer, follower: &Endpoint, req: Request) -> Subscription {
    let addr = writer.server.addr().await;
    timeout(WAIT, client::subscribe(follower, addr, &req))
        .await
        .expect("subscribe timed out")
        .unwrap()
}

/// Read frames until `n` entries arrive. Fail on any other end.
async fn entries(sub: &mut Subscription, n: usize) -> Vec<Entry> {
    let mut out = vec![];
    while out.len() < n {
        match timeout(WAIT, sub.next()).await.expect("timed out").unwrap() {
            Some(Frame::Entry { entry }) => out.push(entry),
            Some(_) => {}
            None => panic!("stream ended after {} entries", out.len()),
        }
    }
    out
}

/// Read all frames until the server ends the stream.
async fn drain(mut sub: Subscription) -> Vec<Frame> {
    let mut out = vec![];
    while let Some(frame) = timeout(WAIT, sub.next()).await.expect("timed out").unwrap() {
        out.push(frame);
    }
    out
}

fn ids(frames: &[Frame]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|f| match f {
            Frame::Entry { entry } => Some(entry.id),
            _ => None,
        })
        .collect()
}

fn once() -> Request {
    Request {
        follow: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn catch_up_then_caught_up_marker() {
    let writer = Writer::start().await;
    writer.post("disk", "one");
    writer.post("disk", "two");
    writer.post("cpu", "three");
    let f = follower().await;
    writer.allow(&f);

    let frames = drain(subscribe(&writer, &f, once()).await).await;
    assert_eq!(ids(&frames), vec![1, 2, 3]);
    assert_eq!(frames.last(), Some(&Frame::CaughtUp { head: 3 }));
}

#[tokio::test]
async fn live_tail_gets_new_entries() {
    let writer = Writer::start().await;
    writer.post("disk", "old");
    let f = follower().await;
    writer.allow(&f);

    let mut sub = subscribe(&writer, &f, Request::default()).await;
    assert_eq!(entries(&mut sub, 1).await[0].body, "old");

    // a separate store handle, like a separate `yappr post` process
    let post = Store::open(writer.store.dir()).unwrap();
    post.append(draft("disk", "new")).unwrap();
    let got = entries(&mut sub, 1).await;
    assert_eq!((got[0].id, got[0].body.as_str()), (2, "new"));
}

#[tokio::test]
async fn since_resumes_without_duplicates_or_gaps() {
    let writer = Writer::start().await;
    for i in 1..=5 {
        writer.post("s", &format!("{i}"));
    }
    let f = follower().await;
    writer.allow(&f);
    let req = Request { since: 2, ..once() };
    assert_eq!(
        ids(&drain(subscribe(&writer, &f, req).await).await),
        vec![3, 4, 5]
    );
}

#[tokio::test]
async fn tail_and_limit() {
    let writer = Writer::start().await;
    for i in 1..=5 {
        writer.post("s", &format!("{i}"));
    }
    let f = follower().await;
    writer.allow(&f);
    let tail = Request {
        tail: Some(2),
        ..once()
    };
    assert_eq!(
        ids(&drain(subscribe(&writer, &f, tail).await).await),
        vec![4, 5]
    );
    let limit = Request {
        since: 1,
        limit: Some(1),
        ..once()
    };
    assert_eq!(
        ids(&drain(subscribe(&writer, &f, limit).await).await),
        vec![2]
    );
}

#[tokio::test]
async fn since_time_skips_older_entries() {
    let writer = Writer::start().await;
    writer.post("s", "old");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let cut = jiff::Timestamp::now();
    writer.post("s", "new");
    let f = follower().await;
    writer.allow(&f);
    let req = Request {
        since_time: Some(cut),
        ..once()
    };
    assert_eq!(
        ids(&drain(subscribe(&writer, &f, req).await).await),
        vec![2]
    );
}

#[tokio::test]
async fn unknown_follower_is_denied_and_gets_nothing() {
    let writer = Writer::start().await;
    writer.post("secret", "do not leak");
    let f = follower().await;

    let addr = writer.server.addr().await;
    let result = timeout(WAIT, async {
        let mut sub = client::subscribe(&f, addr, &once()).await?;
        let mut got = vec![];
        while let Some(frame) = sub.next().await? {
            got.push(frame);
        }
        anyhow::Ok(got)
    })
    .await
    .expect("timed out");
    let err = result.expect_err("a denied follower must get an error");
    let denied = err
        .downcast_ref::<Denied>()
        .unwrap_or_else(|| panic!("want Denied, got {err:#}"));
    assert_eq!(denied.follower, f.id());
}

#[tokio::test]
async fn allow_takes_effect_without_restart_and_deny_revokes() {
    let writer = Writer::start().await;
    writer.post("s", "hello");
    let f = follower().await;
    writer.allow(&f);
    assert_eq!(
        ids(&drain(subscribe(&writer, &f, once()).await).await),
        vec![1]
    );

    writer.store.allow_list().deny(&f.id()).unwrap();
    let addr = writer.server.addr().await;
    let err = timeout(WAIT, async {
        let mut sub = client::subscribe(&f, addr, &once()).await?;
        while sub.next().await?.is_some() {}
        anyhow::Ok(())
    })
    .await
    .expect("timed out")
    .unwrap_err();
    assert!(err.downcast_ref::<Denied>().is_some(), "{err:#}");
}

#[tokio::test]
async fn deny_closes_a_live_feed_before_the_next_entry() {
    let writer = Writer::start().await;
    writer.post("s", "hello");
    let f = follower().await;
    writer.allow(&f);
    let mut sub = subscribe(&writer, &f, Request::default()).await;
    assert_eq!(entries(&mut sub, 1).await[0].body, "hello");

    writer.store.allow_list().deny(&f.id()).unwrap();
    writer.post("s", "secret");
    let err = timeout(WAIT, async {
        loop {
            match sub.next().await {
                Ok(Some(Frame::Entry { entry })) => panic!("a denied follower got {entry:?}"),
                Ok(Some(_)) => continue,
                Ok(None) => panic!("ended without an error"),
                Err(e) => break e,
            }
        }
    })
    .await
    .expect("timed out");
    assert!(err.downcast_ref::<Denied>().is_some(), "{err:#}");
}

#[tokio::test]
async fn the_writer_sends_only_the_entries_that_match_the_filter() {
    let writer = Writer::start().await;
    let post = |source: &str, kind: &str, body: &str| {
        writer
            .store
            .append(Draft {
                kind: kind.into(),
                level: None,
                subject: None,
                format: None,
                data: None,
                ..draft(source, body)
            })
            .unwrap()
    };
    post("nginx", "log", "GET /");
    post("disk", "warn", "92%");
    post("nginx", "log", "GET /a");
    post("backup", "info", "done");
    let f = follower().await;
    writer.allow(&f);
    let inbox = Request {
        not_kinds: vec!["log".into()],
        ..Default::default()
    };
    let mut sub = subscribe(&writer, &f, inbox).await;
    let bodies: Vec<String> = entries(&mut sub, 2)
        .await
        .into_iter()
        .map(|e| e.body)
        .collect();
    assert_eq!(bodies, ["92%", "done"]);
    // The live tail uses the same filter.
    post("nginx", "log", "GET /b");
    post("disk", "alert", "99%");
    assert_eq!(entries(&mut sub, 1).await[0].body, "99%");

    let disk = Request {
        kinds: vec!["warn".into(), "alert".into()],
        source: Some("^disk$".into()),
        ..once()
    };
    let frames = drain(subscribe(&writer, &f, disk).await).await;
    assert_eq!(ids(&frames), vec![2, 6]);
    // The resume position comes from the last entry that the follower got.
    let resumed = Request {
        since: 2,
        not_kinds: vec!["log".into()],
        ..once()
    };
    assert_eq!(
        ids(&drain(subscribe(&writer, &f, resumed).await).await),
        vec![4, 6]
    );
}

#[tokio::test]
async fn a_bad_source_filter_is_an_error_from_the_writer() {
    let writer = Writer::start().await;
    writer.post("s", "hello");
    let f = follower().await;
    writer.allow(&f);
    let bad = Request {
        source: Some("(".into()),
        ..once()
    };
    let mut sub = subscribe(&writer, &f, bad).await;
    let err = timeout(WAIT, sub.next())
        .await
        .expect("timed out")
        .unwrap_err();
    assert!(format!("{err:#}").contains("not a valid regex"), "{err:#}");
}

#[tokio::test]
async fn writer_restart_keeps_entries_and_ids() {
    let writer = Writer::start().await;
    writer.post("s", "before");
    let f = follower().await;
    writer.allow(&f);
    let writer = writer.restart().await;
    assert_eq!(writer.post("s", "after").id, 2);

    let frames = drain(subscribe(&writer, &f, once()).await).await;
    assert_eq!(ids(&frames), vec![1, 2]);
}

#[tokio::test]
async fn follower_rejects_an_invalid_entry_from_the_writer() {
    let writer = Writer::start().await;
    // a log line that `post` would never write: an escape sequence in the body
    let line = r#"{"v":1,"id":1,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"\u001b]0;pwned\u0007"}"#;
    std::fs::write(writer.store.dir().join("feed.ndjson"), format!("{line}\n")).unwrap();
    let f = follower().await;
    writer.allow(&f);

    let mut sub = subscribe(&writer, &f, once()).await;
    let err = timeout(WAIT, sub.next())
        .await
        .expect("timed out")
        .unwrap_err();
    assert!(format!("{err:#}").contains("control character"), "{err:#}");
}

/// Write raw lines into the writer's log, like a newer writer would.
fn write_log(writer: &Writer, lines: &[&str]) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(writer.store.dir().join("feed.ndjson"), text).unwrap();
}

#[tokio::test]
async fn unknown_fields_reach_the_follower() {
    let writer = Writer::start().await;
    write_log(
        &writer,
        &[
            r#"{"v":1,"id":1,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"b","host":"nas"}"#,
        ],
    );
    let f = follower().await;
    writer.allow(&f);
    let mut sub = subscribe(&writer, &f, once()).await;
    let entry = entries(&mut sub, 1).await.remove(0);
    assert_eq!(entry.extra["host"], "nas");
}

#[tokio::test]
async fn follower_rejects_an_unknown_schema_version() {
    let writer = Writer::start().await;
    write_log(
        &writer,
        &[r#"{"v":2,"id":1,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"b"}"#],
    );
    let f = follower().await;
    writer.allow(&f);
    let mut sub = subscribe(&writer, &f, once()).await;
    let err = timeout(WAIT, sub.next())
        .await
        .expect("timed out")
        .unwrap_err();
    assert!(
        err.downcast_ref::<yappr::entry::Invalid>().is_some(),
        "{err:#}"
    );
}

/// Like a short ticket: the follower knows only the endpoint id, and finds
/// the address over mDNS. No relay and no DNS server.
#[tokio::test]
async fn mdns_finds_a_writer_from_its_id_only() {
    let mdns = NetOptions {
        mdns: true,
        ..Network::Local.into()
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("feed")).unwrap();
    store.append(draft("s", "found you")).unwrap();
    let server = Server::spawn(store.clone(), SecretKey::generate(), mdns.clone())
        .await
        .unwrap();
    let f = net::bind(SecretKey::generate(), mdns, vec![], None)
        .await
        .unwrap();
    store.allow_list().allow(f.id()).unwrap();

    let id_only = iroh::EndpointAddr::new(server.endpoint().id());
    let mut sub = timeout(WAIT, client::subscribe(&f, id_only, &once()))
        .await
        .expect("timed out")
        .unwrap();
    let got = entries(&mut sub, 1).await;
    assert_eq!(got[0].body, "found you");
}

// The follow loop in the library: reconnects, backoff and the resume position.

/// Collect events until `n` entries arrive.
async fn follow_entries(f: &mut Follower, n: usize) -> (Vec<Entry>, Vec<Event>) {
    let mut entries = vec![];
    let mut events = vec![];
    while entries.len() < n {
        match timeout(WAIT, f.next()).await.expect("timed out").unwrap() {
            Some(Event::Entry(entry)) => entries.push(entry),
            Some(other) => events.push(other),
            None => panic!("follower ended after {} entries", entries.len()),
        }
    }
    (entries, events)
}

#[tokio::test]
async fn follower_without_follow_ends_after_the_replay() {
    let writer = Writer::start().await;
    writer.post("s", "one");
    writer.post("s", "two");
    let ep = follower().await;
    writer.allow(&ep);
    let mut f = Follower::new(ep, writer.server.addr().await, once());
    let (entries, _) = follow_entries(&mut f, 2).await;
    assert_eq!(entries.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2]);
    let mut rest = vec![];
    while let Some(event) = timeout(WAIT, f.next()).await.expect("timed out").unwrap() {
        rest.push(event);
    }
    assert_eq!(rest, [Event::CaughtUp { head: 2 }]);
}

#[tokio::test]
async fn follower_reconnects_and_resumes_without_duplicates() {
    // a fixed port, so the address stays valid after the restart
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let net = NetOptions {
        port: Some(port),
        ..Network::Local.into()
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("feed")).unwrap();
    let secret = SecretKey::generate();
    store.append(draft("s", "before")).unwrap();
    let server = Server::spawn(store.clone(), secret.clone(), net.clone())
        .await
        .unwrap();
    let ep = follower().await;
    store.allow_list().allow(ep.id()).unwrap();

    let mut f = Follower::new(ep, server.addr().await, Request::default());
    let (first, _) = follow_entries(&mut f, 1).await;
    assert_eq!(first[0].body, "before");

    server.shutdown().await;
    store.append(draft("s", "while away")).unwrap();
    let server = Server::spawn(store.clone(), secret, net).await.unwrap();
    store.append(draft("s", "after")).unwrap();

    let (rest, events) = follow_entries(&mut f, 2).await;
    assert_eq!(
        rest.iter().map(|e| e.body.as_str()).collect::<Vec<_>>(),
        ["while away", "after"]
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Retrying { .. })),
        "{events:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn follower_stops_when_denied() {
    let writer = Writer::start().await;
    writer.post("s", "secret");
    let ep = follower().await;
    let me = ep.id();
    let mut f = Follower::new(ep, writer.server.addr().await, Request::default());
    let err = timeout(WAIT, async {
        loop {
            match f.next().await {
                Ok(Some(Event::Entry(e))) => panic!("a denied follower got {e:?}"),
                Ok(Some(_)) => continue,
                Ok(None) => panic!("ended without an error"),
                Err(e) => break e,
            }
        }
    })
    .await
    .expect("timed out");
    assert_eq!(err.downcast_ref::<Denied>().map(|d| d.follower), Some(me));
}
