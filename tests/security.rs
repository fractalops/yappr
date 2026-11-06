//! Security and robustness cases: revocation, remote text, file safety,
//! pairing, limits, and input formats. Each case checks the safe result.
use iroh::{
    endpoint::{Connection, VarInt},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, SecretKey,
};
use jiff::{SignedDuration, Timestamp};
use std::{
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier,
    },
    time::Duration,
};
use tokio::time::timeout;
use yappr::{
    choices::{self, Choice, Submission},
    client,
    cursor::Cursor,
    disk,
    entry::Draft,
    net::{self, Network},
    pairing,
    proto::{self, Frame, Request},
    server::Server,
    store::Store,
    EndpointTicket,
};

fn draft(body: &str) -> Draft {
    Draft {
        source: "review".into(),
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
async fn endpoint() -> Endpoint {
    net::bind(SecretKey::generate(), Network::Local, vec![], None)
        .await
        .unwrap()
}
async fn setup() -> (tempfile::TempDir, Store, Server, Endpoint) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let server = Server::spawn(store.clone(), SecretKey::generate(), Network::Local)
        .await
        .unwrap();
    let peer = endpoint().await;
    store.allow_list().allow(peer.id()).unwrap();
    (dir, store, server, peer)
}

/// A loopback proxy that can drop replies from the server. Only one client
/// uses each proxy, so the source address identifies the direction.
async fn packet_loss_proxy(
    server: iroh::EndpointAddr,
    drop_replies: Arc<AtomicBool>,
    dropped: Arc<AtomicUsize>,
) -> (iroh::EndpointAddr, n0_future::task::AbortOnDropHandle<()>) {
    let server_ip = *server.ip_addrs().next().unwrap();
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = iroh::EndpointAddr::new(server.id).with_ip_addr(socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut peer_ip = None;
        let mut buffer = vec![0; 65536];
        loop {
            let (len, from) = socket.recv_from(&mut buffer).await.unwrap();
            if from == server_ip {
                if drop_replies.load(Ordering::SeqCst) {
                    dropped.fetch_add(1, Ordering::SeqCst);
                } else if let Some(to) = peer_ip {
                    socket.send_to(&buffer[..len], to).await.unwrap();
                }
            } else {
                peer_ip = Some(from);
                socket.send_to(&buffer[..len], server_ip).await.unwrap();
            }
        }
    });
    (addr, n0_future::task::AbortOnDropHandle::new(task))
}

#[tokio::test]
async fn revoked_stalled_request_does_not_read_a_new_entry() {
    stalled_request_is_revoked(false).await;
}

#[tokio::test]
async fn revoked_stalled_request_handles_a_lost_denial_packet() {
    stalled_request_is_revoked(true).await;
}

async fn stalled_request_is_revoked(lose_denial: bool) {
    let (_dir, store, server, peer) = setup().await;
    let drop_replies = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicUsize::new(0));
    let (addr, _proxy) = if lose_denial {
        let (addr, proxy) =
            packet_loss_proxy(server.addr().await, drop_replies.clone(), dropped.clone()).await;
        (addr, Some(proxy))
    } else {
        (server.addr().await, None)
    };
    let conn = peer.connect(addr, yappr::ALPN).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[0]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop_replies.store(lose_denial, Ordering::SeqCst);
    store.allow_list().deny(&peer.id()).unwrap();
    store.append(draft("private entry after denial")).unwrap();
    tokio::time::sleep(Duration::from_millis(5500)).await;
    drop_replies.store(false, Ordering::SeqCst);
    if lose_denial {
        assert!(
            dropped.load(Ordering::SeqCst) > 0,
            "no packets were dropped"
        );
    }
    let bytes = serde_json::to_vec(&Request {
        follow: false,
        ..Default::default()
    })
    .unwrap();
    let mut rest = (bytes.len() as u32).to_be_bytes()[1..].to_vec();
    rest.extend(bytes);
    let got = if send.write_all(&rest).await.is_ok() {
        send.finish().ok();
        // The first frame can name the log. Look at every frame.
        let mut entry = None;
        while let Some(frame) = timeout(
            Duration::from_secs(2),
            proto::read_frame::<Frame>(&mut recv, proto::MAX_FRAME),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        {
            if matches!(frame, Frame::Entry { .. }) {
                entry = Some(frame);
                break;
            }
        }
        entry
    } else {
        None
    };
    // A lost denial packet can leave only a stateless reset once the server
    // discards the closed connection. Both outcomes must prevent reads.
    let reason = timeout(Duration::from_secs(5), conn.closed()).await;
    conn.close(VarInt::from_u32(0), b"review complete");
    peer.close().await;
    server.shutdown().await;
    assert!(
        !matches!(got, Some(Frame::Entry { .. })),
        "a revoked device read a new entry"
    );
    assert!(
        matches!(reason, Ok(iroh::endpoint::ConnectionError::Reset))
            || matches!(
                reason,
                Ok(iroh::endpoint::ConnectionError::ApplicationClosed(ref close))
                    if close.error_code == VarInt::from_u32(proto::DENIED_CODE)
            ),
        "the revoked connection did not close as denied or reset: {reason:?}"
    );
}

#[tokio::test]
async fn revoked_slow_replay_closes_within_five_seconds() {
    let (_dir, store, server, peer) = setup().await;
    for _ in 0..80 {
        store
            .append(draft(&"x".repeat(yappr::entry::MAX_BODY)))
            .unwrap();
    }
    let conn = peer
        .connect(server.addr().await, yappr::ALPN)
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    proto::write_frame(&mut send, &Request::default(), proto::MAX_REQUEST)
        .await
        .unwrap();
    send.finish().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    store.allow_list().deny(&peer.id()).unwrap();
    let closed = timeout(Duration::from_millis(5500), conn.closed()).await;
    conn.close(VarInt::from_u32(0), b"review complete");
    peer.close().await;
    server.shutdown().await;
    assert!(
        closed.is_ok(),
        "a stalled replay remains connected after denial"
    );
}

#[derive(Debug)]
struct HostileWriter;
impl ProtocolHandler for HostileWriter {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let _: Option<Request> = proto::read_frame(&mut recv, proto::MAX_REQUEST)
            .await
            .unwrap();
        proto::write_frame(
            &mut send,
            &Frame::Error {
                message: "failure\u{1b}[2Jreview".into(),
            },
            proto::MAX_FRAME,
        )
        .await
        .unwrap();
        send.finish()?;
        timeout(Duration::from_secs(5), conn.closed()).await.ok();
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_error_does_not_send_terminal_controls_to_stderr() {
    let router = Router::builder(endpoint().await)
        .accept(yappr::ALPN, HostileWriter)
        .spawn();
    let ticket = EndpointTicket::new(net::addr(router.endpoint()).await).to_string();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_yappr"))
            .args(["read", &ticket])
            .env("YAPPR_DIR", path)
            .env("YAPPR_NETWORK", "local")
            .env_remove("IROH_SECRET")
            .env_remove("YAPPR_KEY")
            .env_remove("RUST_LOG")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    router.shutdown().await.unwrap();
    assert!(!output.status.success());
    assert!(
        !output.stderr.contains(&0x1b),
        "stderr contains a control byte from the writer"
    );
}

#[test]
fn rejected_invitation_closes_unknown_admission() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let remote = SecretKey::generate().public();
    let ticket = EndpointTicket::new(iroh::EndpointAddr::new(SecretKey::generate().public()));
    let invitation = pairing::create(
        &store,
        ticket.to_string(),
        "Writer".into(),
        Timestamp::now()
            .checked_add(SignedDuration::from_secs(60))
            .unwrap(),
    )
    .unwrap();
    pairing::join(
        &store,
        remote,
        pairing::Join {
            token: invitation.token,
            name: "Reader".into(),
            ticket: None,
        },
    )
    .unwrap();
    pairing::confirm(&store, remote, false).unwrap();
    assert!(
        !pairing::open(dir.path()),
        "a rejected invitation still admits unknown endpoints"
    );
}

#[cfg(unix)]
#[test]
fn preexisting_feed_file_has_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("feed.ndjson");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.append(draft("private body")).unwrap();
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "existing files keep access for other users"
    );
}

#[cfg(unix)]
#[test]
fn cursor_save_does_not_truncate_a_symbolic_link_target() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "keep this content").unwrap();
    let feed = SecretKey::generate().public();
    std::fs::create_dir(dir.path().join("cursors")).unwrap();
    symlink(
        &victim,
        dir.path().join("cursors").join(format!("{feed}.tmp")),
    )
    .unwrap();
    Cursor::new(dir.path(), feed, "script")
        .unwrap()
        .save(42, None)
        .ok();
    assert_eq!(
        std::fs::read_to_string(victim).unwrap(),
        "keep this content"
    );
}

#[test]
fn cursor_names_do_not_share_a_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let feed = SecretKey::generate().public();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = [("first", 11), ("second", 99)]
        .into_iter()
        .map(|(name, value)| {
            let cursor = Cursor::new(dir.path(), feed, name).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut failures = 0;
                for _ in 0..250 {
                    barrier.wait();
                    if cursor.save(value, None).is_err() {
                        failures += 1;
                    }
                    barrier.wait();
                    if cursor.load().ok().map(|p| p.id) != Some(value) {
                        failures += 1;
                    }
                }
                failures
            })
        })
        .collect();
    let failures: usize = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .sum();
    assert_eq!(
        failures, 0,
        "named cursors exchanged values or failed to save"
    );
}

#[test]
fn full_length_choice_label_can_produce_a_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let remote = SecretKey::generate().public();
    store.allow_list().allow(remote).unwrap();
    let q = choices::create(
        &store,
        choices::new_question(
            "Continue?".into(),
            "".into(),
            "review".into(),
            vec![
                Choice {
                    value: "yes".into(),
                    label: "x".repeat(256),
                },
                Choice {
                    value: "no".into(),
                    label: "No".into(),
                },
            ],
            remote,
            None,
            false,
        ),
    )
    .unwrap();
    choices::decide(
        &store,
        remote,
        Submission {
            id: disk::random_id(),
            question_id: q.id,
            value: "yes".into(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn zero_limit_does_not_stop_the_router() {
    let (_dir, store, server, peer) = setup().await;
    store.append(draft("one entry")).unwrap();
    let addr = server.addr().await;
    let mut sub = client::subscribe(
        &peer,
        addr.clone(),
        &Request {
            follow: false,
            limit: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _ = timeout(Duration::from_secs(2), sub.next()).await;
    drop(sub);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let healthy = client::subscribe(
        &peer,
        addr,
        &Request {
            follow: false,
            ..Default::default()
        },
    )
    .await;
    let alive = match healthy {
        Ok(mut sub) => {
            matches!(
                timeout(Duration::from_secs(2), sub.next()).await,
                Ok(Ok(Some(Frame::Feed { .. })))
            ) && matches!(
                timeout(Duration::from_secs(2), sub.next()).await,
                Ok(Ok(Some(Frame::Entry { .. })))
            )
        }
        Err(_) => false,
    };
    peer.close().await;
    server.shutdown().await;
    assert!(alive, "a zero limit stopped service for the next reader");
}

#[test]
fn accepted_pairing_result_survives_the_claim_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let remote = SecretKey::generate().public();
    let ticket = EndpointTicket::new(iroh::EndpointAddr::new(SecretKey::generate().public()));
    let invitation = pairing::create(
        &store,
        ticket.to_string(),
        "Writer".into(),
        Timestamp::now()
            .checked_add(SignedDuration::from_mins(5))
            .unwrap(),
    )
    .unwrap();
    let join = pairing::Join {
        token: invitation.token,
        name: "Reader".into(),
        ticket: None,
    };
    pairing::join(&store, remote, join.clone()).unwrap();
    pairing::confirm(&store, remote, true).unwrap();
    // Move the saved invitation past its claim deadline after confirmation.
    // This tests result polling without racing file operations against a timer.
    let mut offer = pairing::get(store.dir(), &join.token).unwrap();
    offer.invitation.expires_at = Timestamp::now() - SignedDuration::from_secs(1);
    disk::write(
        &store.dir().join("pairing.json"),
        &serde_json::json!({"offers": {(join.token.clone()): &offer}}),
    )
    .unwrap();
    assert!(store.allow_list().contains(&remote));
    assert_eq!(
        pairing::join(&store, remote, join).unwrap().status,
        "accepted"
    );
}

#[test]
fn device_aliases_for_one_identity_report_the_same_access() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let id = SecretKey::generate().public();
    store.allow_list().allow(id).unwrap();
    for name in ["alpha", "beta"] {
        yappr::peers::set(dir.path(), name, &id.to_string()).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_yappr"))
        .arg("devices")
        .env("YAPPR_DIR", dir.path())
        .env("YAPPR_NETWORK", "local")
        .env_remove("IROH_SECRET")
        .env_remove("YAPPR_KEY")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(output.status.success());
    let rows = String::from_utf8(output.stdout).unwrap();
    assert_eq!(rows.lines().count(), 2);
    assert!(
        rows.lines().all(|row| row.ends_with("\tyes")),
        "aliases disagree about access"
    );
}

#[test]
fn several_writer_json_output_is_valid_post_json_input() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let input = serde_json::json!({"writer": "writer", "entry": {
        "v": 1, "id": 1, "time": "2026-10-31T00:00:00Z",
        "source": "review", "kind": "info", "body": "hello"
    }});
    let mut child = Command::new(env!("CARGO_BIN_EXE_yappr"))
        .args(["post", "--json"])
        .env("YAPPR_DIR", dir.path())
        .env("YAPPR_NETWORK", "local")
        .env_remove("IROH_SECRET")
        .env_remove("YAPPR_KEY")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{input}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "the documented JSON pipeline rejected the writer envelope"
    );
}
