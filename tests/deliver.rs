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
            .unwrap_or_else(|_| panic!("timed out waiting for {n} entries"))
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
    let err = result.unwrap_err();
    assert!(
        err.downcast_ref::<deliver::NotAllowed>().is_some(),
        "{err:#}"
    );
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
    assert_eq!(state.status, yappr::choices::QuestionStatus::Answered);
    // The decision reaches the device too.
    assert_eq!(device.wait_for(2).await[1].kind, "decision");
    delivering.abort();
}

/// A slow control stream must not lose its reply when the feed is saved.
#[tokio::test]
async fn delivery_waits_for_control_replies_before_reporting_saved_entries() {
    use iroh::endpoint::{presets, QuicTransportConfig, VarInt};

    let writer = Writer::new();
    // A reply cannot fit until this device starts reading its stream.
    let device = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .alpns(vec![deliver::ALPN.to_vec()])
        .transport_config(
            QuicTransportConfig::builder()
                .stream_receive_window(VarInt::from_u32(16))
                .build(),
        )
        .bind()
        .await
        .unwrap();
    writer.store.allow_list().allow(device.id()).unwrap();
    let question = choices::create(
        &writer.store,
        choices::new_question(
            "Deploy?".into(),
            String::new(),
            "ci".into(),
            choices::approval_options(),
            device.id(),
            None,
            true,
        ),
    )
    .unwrap();
    let endpoint = net::bind(writer.secret.clone(), Network::Local, vec![], None)
        .await
        .unwrap();
    let feed = Feed::open(writer.store.clone()).await.unwrap();
    let addr = net::addr(&device).await;
    let (progress, mut saved) = watch::channel(Stored::default());
    let mut delivering = tokio::spawn(async move {
        deliver::deliver(
            &endpoint,
            &feed,
            addr,
            &Hello::direct(endpoint.id()),
            Some(2),
            &progress,
        )
        .await
    });
    let conn = timeout(WAIT, device.accept())
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let incoming = deliver::accept(conn.clone(), device.id()).await.unwrap();
    let sub = incoming.subscribe(&Request::default()).await.unwrap();
    let (entries_tx, mut entries) = watch::channel(Vec::new());
    let receiving = n0_future::task::AbortOnDropHandle::new(tokio::spawn(receive(sub, entries_tx)));
    timeout(WAIT, entries.wait_for(|entries| entries.len() == 1))
        .await
        .unwrap()
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    yappr::proto::write_frame(
        &mut send,
        &control::Request::Submit {
            submission: Submission {
                id: choices::random_id(),
                question_id: question.id,
                value: "approve".into(),
            },
        },
        yappr::proto::MAX_REQUEST,
    )
    .await
    .unwrap();
    send.finish().unwrap();
    // The feed keeps moving while the control reply is flow-controlled.
    timeout(WAIT, entries.wait_for(|entries| entries.len() == 2))
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(
            Duration::from_millis(200),
            saved.wait_for(|stored| stored.through >= 2)
        )
        .await
        .is_err(),
        "delivery reported the decision before its control reply reached the device"
    );
    assert!(!delivering.is_finished());
    let reply: control::Reply = timeout(
        WAIT,
        yappr::proto::read_frame(&mut recv, yappr::proto::MAX_FRAME),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(matches!(reply, control::Reply::Accepted { .. }));
    timeout(WAIT, saved.wait_for(|stored| stored.through >= 2))
        .await
        .unwrap()
        .unwrap();
    timeout(WAIT, &mut delivering)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(receiving);
    device.close().await;
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
    yappr_with_key(dir, None, args).await
}

/// Run the built binary with the private key `key` in YAPPR_KEY, as a CI job
/// does.
async fn yappr_with_key(
    dir: &std::path::Path,
    key: Option<&str>,
    args: &[&str],
) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_yappr"));
    command
        .args(args)
        .env("YAPPR_DIR", dir)
        .env("YAPPR_NETWORK", "local")
        .env_remove("IROH_SECRET")
        .env_remove("YAPPR_KEY")
        .stdin(std::process::Stdio::null());
    if let Some(key) = key {
        command.env("YAPPR_KEY", key);
    }
    command.output().await.unwrap()
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

#[tokio::test]
async fn text_from_another_endpoint_reaches_the_terminal_escaped() {
    // A writer that closes each connection with escape codes in its reason.
    let hostile = net::bind(
        SecretKey::generate(),
        Network::Local,
        vec![yappr::ALPN.to_vec()],
        None,
    )
    .await
    .unwrap();
    let accepting = hostile.clone();
    let _task = n0_future::task::AbortOnDropHandle::new(tokio::spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            if let Ok(conn) = incoming.await {
                conn.close(7u32.into(), b"\x1b]0;owned\x07\x1b[2J");
            }
        }
    }));
    let ticket = EndpointTicket::new(net::addr(&hostile).await).to_string();
    let dir = tempfile::tempdir().unwrap();
    let out = yappr(dir.path(), &["read", &ticket, "--since", "0"]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("owned"), "{stderr}");
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "{stderr:?}"
    );
}

/// A CI job with its own key reaches a device that allowed that key, with no
/// pairing: the flow of `yappr key` and `allow --receives`.
#[tokio::test]
async fn a_ci_job_with_a_key_asks_a_device_that_allowed_the_key() {
    let laptop = tempfile::tempdir().unwrap();
    let out = yappr(
        laptop.path(),
        &["key", "--name", "CI deploys", "--output", "json"],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let key: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let public: EndpointId = key["public_key"].as_str().unwrap().parse().unwrap();
    let code = yappr::pairing::MachineCode::decode(key["machine_code"].as_str().unwrap()).unwrap();
    assert_eq!((code.id, code.name.as_str()), (public, "CI deploys"));

    // The device allows the public key of the job, as the app does after a scan.
    let mut device = Device::start(public).await;
    let address = EndpointTicket::new(device.addr().await).to_string();

    // A fresh feed directory for each run, as on a CI runner.
    let ci = tempfile::tempdir().unwrap();
    let private = key["private_key"].as_str().unwrap();
    let allowed = yappr_with_key(
        ci.path(),
        Some(private),
        &["allow", &address, "--alias", "phone", "--receives"],
    )
    .await;
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let id = yappr_with_key(ci.path(), Some(private), &["id"]).await;
    assert_eq!(
        String::from_utf8(id.stdout).unwrap().trim(),
        public.to_string()
    );

    let path = ci.path().to_owned();
    let private = private.to_string();
    let asking = tokio::spawn(async move {
        yappr_with_key(
            &path,
            Some(&private),
            &["approve", "phone", "Deploy v1.4.2?"],
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
    let out = timeout(WAIT, asking).await.unwrap().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn key_prints_env_lines_with_the_address_of_a_paired_device() {
    let (dir, device) = cli_writer().await;
    let out = yappr(dir.path(), &["key", "--for", "phone", "--output", "env"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let key = lines[0].strip_prefix("YAPPR_KEY=").unwrap();
    assert!(key.parse::<SecretKey>().is_ok());
    let address: EndpointTicket = lines[1]
        .strip_prefix("YAPPR_PHONE=")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(address.endpoint_addr().id, device.endpoint.id());
    let unknown = yappr(dir.path(), &["key", "--for", "tablet"]).await;
    assert!(!unknown.status.success());
}

/// Run the built binary with `stdin`, and with the agent settings under
/// `home`, as an agent runs a hook.
async fn yappr_hook(
    dir: &std::path::Path,
    home: &std::path::Path,
    args: &[&str],
    stdin: &str,
) -> std::process::Output {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_yappr"))
        .args(args)
        .env("YAPPR_DIR", dir)
        .env("YAPPR_NETWORK", "local")
        .env("YAPPR_PRESENCE", "away")
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("CODEX_HOME", home.join("codex"))
        .env_remove("IROH_SECRET")
        .env_remove("YAPPR_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.as_bytes()).await.unwrap();
    drop(input);
    child.wait_with_output().await.unwrap()
}

/// The handler of `event` in the plugin hook file of `agent`.
fn plugin_handler(agent: &str, event: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("hooks/{agent}.json"));
    let hooks: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    hooks["hooks"][event][0]["hooks"][0].clone()
}

/// Run the plugin hook of `agent` for `event` as the agent runs it, with the
/// built binary as yappr and `env` added.
#[cfg(unix)]
async fn plugin_hook(
    dir: &std::path::Path,
    agent: &str,
    event: &str,
    stdin: &str,
    env: &[(&str, &str)],
) -> std::process::Output {
    use tokio::io::AsyncWriteExt;
    let root = env!("CARGO_MANIFEST_DIR");
    let handler = plugin_handler(agent, event);
    let command = handler["command"].as_str().unwrap();
    let mut process = match handler.get("args") {
        // Claude Code runs the exec form without a shell.
        Some(args) => {
            let mut process = tokio::process::Command::new(command);
            for arg in args.as_array().unwrap() {
                process.arg(arg.as_str().unwrap().replace("${CLAUDE_PLUGIN_ROOT}", root));
            }
            process
        }
        // Codex runs the command with a shell.
        None => {
            let mut process = tokio::process::Command::new("sh");
            process.args(["-c", command]);
            process
        }
    };
    process
        .env("CLAUDE_PLUGIN_ROOT", root)
        .env("PLUGIN_ROOT", root)
        .env("YAPPR_BIN", env!("CARGO_BIN_EXE_yappr"))
        .env("YAPPR_DIR", dir)
        .env("YAPPR_NETWORK", "local")
        .env("YAPPR_PRESENCE", "away")
        .env_remove("YAPPR_FEED")
        .env_remove("IROH_SECRET")
        .env_remove("YAPPR_KEY")
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = process.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .await
        .unwrap();
    timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn filtered_hooks_skip_unmatched_commands_and_post_results_for_both_agents() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for agent in ["claude", "codex"] {
        let args = [
            "hook",
            "--agent",
            agent,
            "--events",
            "PostToolUse:Bash:cargo test*",
        ];
        for tool_input in [
            serde_json::json!({"command": "cargo build"}),
            serde_json::json!({"file_path": "/work/file"}),
        ] {
            let event = serde_json::json!({
                "hook_event_name": "PostToolUse", "tool_name": "Bash",
                "tool_input": tool_input, "tool_response": "done",
            })
            .to_string();
            let out = yappr_hook(dir.path(), home.path(), &args, &event).await;
            assert!(out.status.success());
            assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
        }
        assert_eq!(
            Store::open(dir.path()).unwrap().last_id().unwrap(),
            if agent == "claude" { 0 } else { 1 }
        );
        let event = serde_json::json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": {"command": "cargo test --release"},
            "tool_response": if agent == "claude" { serde_json::json!({"stdout": "2 passed"}) } else { serde_json::json!("Exit code: 1\n1 failed") },
        }).to_string();
        let out = yappr_hook(dir.path(), home.path(), &args, &event).await;
        assert!(out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
        let entries = Store::open(dir.path()).unwrap().entries().unwrap();
        let entry = entries.last().unwrap();
        assert_eq!(entry.kind, "state");
        assert_eq!(entry.subject.as_deref(), Some(agent));
        assert!(entry.body.contains("cargo test --release"));
        assert!(entry.body.contains(if agent == "claude" {
            "2 passed"
        } else {
            "1 failed"
        }));
    }
    let event = r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_input":{"command":"cargo test"},"error":"Exit code 1: test failed"}"#;
    let out = yappr_hook(
        dir.path(),
        home.path(),
        &[
            "hook",
            "--agent",
            "claude",
            "--events",
            "PostToolUseFailure:Bash:cargo test*",
        ],
        event,
    )
    .await;
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let entries = Store::open(dir.path()).unwrap().entries().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].kind, "state");
    // The failure is the second step on the card of Claude.
    assert!(entries[2]
        .body
        .starts_with("- `Bash` · `cargo test --release`\n- `Bash` failed · `cargo test`"));
    assert!(entries[2].body.contains("Exit code 1: test failed"));
}

#[cfg(unix)]
#[tokio::test]
async fn configured_command_filters_deliver_results_and_keep_pretool_approvals() {
    let (dir, mut device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let events = "permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*,posttooluse:Bash:cargo build*,posttooluse:Bash:*test*";
    let saved = yappr_hook(
        dir.path(),
        home.path(),
        &["config", "--hook-events", events, "--hook-timeout", "5s"],
        "",
    )
    .await;
    assert!(saved.status.success(), "{saved:?}");
    let config_path = dir.path().join("config.json");
    let original = std::fs::read(&config_path).unwrap();
    for events in [
        "permission:Bash:git *",
        "posttooluse:Bash:cargo test*,posttooluse:Bash|Read:*test*",
        "pretooluse:Bash:git push*,pretooluse:Bash|Read:git *",
        "permission:(Bash",
    ] {
        let invalid = yappr_hook(
            dir.path(),
            home.path(),
            &["config", "--hook-events", events],
            "",
        )
        .await;
        assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
    }
    for (event, tool, command) in [
        ("PostToolUse", "Bash", "cargo fmt"),
        ("PreToolUse", "Bash", "git status"),
        ("PostToolUse", "Read", "cargo test"),
    ] {
        let input = serde_json::json!({"hook_event_name": event, "tool_name": tool, "tool_input": {"command": command}}).to_string();
        let out = plugin_hook(dir.path(), "claude", event, &input, &[]).await;
        assert!(
            out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
            "{out:?}"
        );
    }
    assert_eq!(Store::open(dir.path()).unwrap().last_id().unwrap(), 0);
    for (n, command) in [(1, "cargo test --release"), (2, "cargo build")] {
        let input = serde_json::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": command}, "tool_response": {"stdout": "done"}}).to_string();
        let out = plugin_hook(dir.path(), "claude", "PostToolUse", &input, &[]).await;
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
        let entries = device.wait_for(n).await;
        assert_eq!(
            entries.len(),
            n,
            "overlapping filters sent duplicate updates"
        );
        assert_eq!(entries[n - 1].kind, "state");
        assert!(entries[n - 1].body.contains(command));
    }
    let path = dir.path().to_owned();
    let asking = tokio::spawn(async move {
        plugin_hook(&path, "codex", "PreToolUse", r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git push origin main"}}"#, &[]).await
    });
    let entries = device.wait_for(3).await;
    let question = choices::question(&entries[2]).unwrap();
    let conn = device.last.borrow().clone().unwrap();
    let reply: control::Reply = control::exchange_on(
        &conn,
        &control::Request::Submit {
            submission: Submission {
                id: choices::random_id(),
                question_id: question.id,
                value: "approve".into(),
            },
        },
    )
    .await
    .unwrap();
    assert!(matches!(reply, control::Reply::Accepted { .. }));
    let out = timeout(WAIT, asking).await.unwrap().unwrap();
    assert!(out.status.success(), "{out:?}");
    let decision: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        decision["hookSpecificOutput"]["hookEventName"],
        "PreToolUse"
    );
    assert_eq!(
        decision["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    device.wait_for(4).await;
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_options_replace_the_saved_settings_and_empty_options_keep_them() {
    let (dir, _device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let out = yappr_hook(
        dir.path(),
        home.path(),
        &["config", "--hook-events", "stop"],
        "",
    )
    .await;
    assert!(out.status.success(), "{out:?}");
    let input = r#"{"hook_event_name":"SubagentStop","cwd":"/work/app"}"#;
    // The saved events have no subagent event.
    let out = plugin_hook(
        dir.path(),
        "claude",
        "SubagentStop",
        input,
        &[("CLAUDE_PLUGIN_OPTION_EVENTS", "")],
    )
    .await;
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(Store::open(dir.path()).unwrap().last_id().unwrap(), 0);
    // The plugin option comes first.
    let out = plugin_hook(
        dir.path(),
        "claude",
        "SubagentStop",
        input,
        &[("CLAUDE_PLUGIN_OPTION_EVENTS", "subagent")],
    )
    .await;
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    let entries = Store::open(dir.path()).unwrap().entries().unwrap();
    assert_eq!(
        entries.last().unwrap().title.as_deref(),
        Some("Claude: a subagent finished in app")
    );
    let out = plugin_hook(
        dir.path(),
        "codex",
        "SubagentStop",
        input,
        &[("CLAUDE_PLUGIN_OPTION_EVENTS", "subagent")],
    )
    .await;
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(Store::open(dir.path()).unwrap().last_id().unwrap(), 1);
    // An unknown device is an error on stderr, not a decision.
    let out = plugin_hook(
        dir.path(),
        "claude",
        "PermissionRequest",
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        &[("CLAUDE_PLUGIN_OPTION_EVENTS", "permission"), ("CLAUDE_PLUGIN_OPTION_DEVICE", "tablet")],
    )
    .await;
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("tablet"),
        "{out:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn config_saves_and_clears_the_hook_settings() {
    let (dir, _device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let config = |args: Vec<&'static str>| {
        let dir = dir.path().to_owned();
        let home = home.path().to_owned();
        async move { yappr_hook(&dir, &home, &args, "").await }
    };
    let out = config(vec![
        "config",
        "--hook-to",
        "phone",
        "--hook-timeout",
        "5m",
        "--hook-events",
        "permission,subagent",
    ])
    .await;
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Agent approvals go to: phone"), "{text}");
    assert!(text.contains("Agent approvals wait: 5m"), "{text}");
    assert!(text.contains("Agent events: permission,subagent"), "{text}");
    for invalid in [
        vec!["config", "--hook-to", "tablet"],
        vec!["config", "--hook-timeout", "2h"],
        vec!["config", "--hook-events", "loud"],
    ] {
        let out = config(invalid.clone()).await;
        assert!(!out.status.success(), "{invalid:?}");
    }
    let out = config(vec![
        "config",
        "--hook-to",
        "",
        "--hook-timeout",
        "",
        "--hook-events",
        "",
        "--output",
        "json",
    ])
    .await;
    assert!(out.status.success(), "{out:?}");
    let saved: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(saved["hooks"], serde_json::json!({}));
}

#[test]
fn plugin_hook_files_cover_every_event_and_outlast_the_wait() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let all = [
        "PermissionRequest",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "Notification",
        "Stop",
        "SubagentStop",
    ];
    for agent in ["claude", "codex"] {
        let hooks: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(format!("hooks/{agent}.json"))).unwrap(),
        )
        .unwrap();
        let events: Vec<&String> = hooks["hooks"].as_object().unwrap().keys().collect();
        let expected: Vec<&str> = all
            .iter()
            .copied()
            .filter(|e| agent == "claude" || !matches!(*e, "Notification" | "PostToolUseFailure"))
            .collect();
        assert_eq!(events, expected, "{agent}");
        for event in expected {
            let handler = plugin_handler(agent, event);
            assert!(
                hooks["hooks"][event][0].get("matcher").is_none(),
                "{agent} {event}"
            );
            let wanted = match (agent, event) {
                (_, "PermissionRequest" | "PreToolUse") => yappr::hooks::ASK_HOOK_TIMEOUT,
                _ => 30,
            };
            assert_eq!(handler["timeout"], wanted, "{agent} {event}");
            assert!(yappr::hooks::MAX_WAIT.as_secs() < yappr::hooks::ASK_HOOK_TIMEOUT);
        }
    }
    let codex: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".codex-plugin/plugin.json")).unwrap())
            .unwrap();
    assert_eq!(codex["version"], env!("CARGO_PKG_VERSION"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_launcher_without_yappr_gives_a_hint_and_no_decision() {
    let empty = tempfile::tempdir().unwrap();
    let out = tokio::process::Command::new("sh")
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/yappr-hook"))
        .arg("claude")
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", empty.path())
        .env_remove("CARGO_HOME")
        .env_remove("YAPPR_BIN")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not installed"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_codex_permission_hook_shows_the_install_hint() {
    let empty = tempfile::tempdir().unwrap();
    let out = plugin_hook(
        empty.path(),
        "codex",
        "PermissionRequest",
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#,
        &[
            ("PATH", "/usr/bin:/bin"),
            ("CARGO_HOME", empty.path().to_str().unwrap()),
            ("YAPPR_BIN", ""),
        ],
    )
    .await;
    assert!(out.status.success(), "{out:?}");
    let hint = "yappr is not installed";
    assert!(String::from_utf8_lossy(&out.stderr).contains(hint));
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(result["systemMessage"].as_str().unwrap().contains(hint));
    assert!(result.get("hookSpecificOutput").is_none());
}

#[tokio::test]
async fn configured_regex_matchers_keep_their_colons() {
    let (dir, _device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let events = r"permission:(?:Bash|Write),pretooluse:(?:Bash|Write):git push*,posttooluse:mcp__server\:tool:curl https://host/*";
    let out = yappr_hook(
        dir.path(),
        home.path(),
        &["config", "--hook-events", events, "--output", "json"],
        "",
    )
    .await;
    assert!(out.status.success(), "{out:?}");
    let saved: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(saved["hooks"]["events"], events);
}

#[tokio::test]
async fn a_permission_request_waits_for_the_device_and_returns_its_decision() {
    let (dir, mut device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let event = r#"{"hook_event_name":"PermissionRequest","session_id":"s","cwd":"/work/app","tool_name":"Bash","tool_input":{"command":"rm -rf build"}}"#;
    for (n, value, behavior) in [(1, "approve", "allow"), (2, "reject", "deny")] {
        let path = dir.path().to_owned();
        let home_path = home.path().to_owned();
        let asking = tokio::spawn(async move {
            yappr_hook(
                &path,
                &home_path,
                &[
                    "hook",
                    "--agent",
                    "claude",
                    "--to",
                    "phone",
                    "--timeout",
                    "30s",
                ],
                event,
            )
            .await
        });
        let entry = device.wait_for(n * 2 - 1).await.remove(n * 2 - 2);
        assert_eq!(
            entry.title.as_deref(),
            Some("Claude asks to use Bash in app")
        );
        assert_eq!(entry.body, "rm -rf build");
        assert_eq!(entry.subject.as_deref(), Some("claude/app"));
        let question = choices::question(&entry).unwrap();
        let conn = device.last.borrow().clone().unwrap();
        let _: control::Reply = control::exchange_on(
            &conn,
            &control::Request::Submit {
                submission: Submission {
                    id: choices::random_id(),
                    question_id: question.id,
                    value: value.into(),
                },
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{value} reply failed: {error:#}"));
        let out = timeout(WAIT, asking).await.unwrap().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let decision: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            decision["hookSpecificOutput"]["decision"]["behavior"],
            behavior
        );
        // The decision entry reaches the device too.
        device.wait_for(n * 2).await;
    }
}

#[tokio::test]
async fn other_events_post_and_failures_never_block_the_agent() {
    let (dir, mut device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let stop = r#"{"hook_event_name":"Stop","cwd":"/work/app","last_assistant_message":"All tests pass."}"#;
    let out = yappr_hook(dir.path(), home.path(), &["hook", "--agent", "codex"], stop).await;
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let entry = device.wait_for(1).await.remove(0);
    assert_eq!(
        (entry.kind.as_str(), entry.title.as_deref()),
        ("state", Some("Codex finished in app"))
    );
    assert_eq!(entry.body, "All tests pass.");

    // An unknown device, and an event that is not JSON: exit 0 and no
    // decision.
    let ask = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
    for (args, input) in [
        (vec!["hook", "--agent", "claude", "--to", "tablet"], ask),
        (
            vec!["hook", "--agent", "claude", "--to", "phone"],
            "not json",
        ),
    ] {
        let out = yappr_hook(dir.path(), home.path(), &args, input).await;
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("yappr hook: "));
    }
}

#[tokio::test]
async fn a_post_hook_does_not_wait_for_a_device_that_is_not_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let gone = iroh::SecretKey::generate().public();
    let ticket = EndpointTicket::new(iroh::EndpointAddr::new(gone));
    peers::set_receiver(dir.path(), "phone", &ticket).unwrap();
    let stop = r#"{"hook_event_name":"Stop","cwd":"/work/app","last_assistant_message":"Done."}"#;
    let started = std::time::Instant::now();
    let out = yappr_hook(
        dir.path(),
        home.path(),
        &["hook", "--agent", "claude"],
        stop,
    )
    .await;
    assert!(out.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    // The entry stays in the feed for a later delivery.
    let read = yappr(dir.path(), &["status", "--output", "json"]).await;
    let status: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(status["feed"]["entries"], 1, "{status}");
}

#[tokio::test]
async fn at_the_computer_the_hooks_stay_quiet_unless_set_to_always() {
    let (dir, _device) = cli_writer().await;
    let ask = r#"{"hook_event_name":"PermissionRequest","cwd":"/work/app","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let stop = r#"{"hook_event_name":"Stop","cwd":"/work/app","last_assistant_message":"Done."}"#;
    for input in [ask, stop] {
        let out = plugin_hook(
            dir.path(),
            "claude",
            "Stop",
            input,
            &[("YAPPR_PRESENCE", "here")],
        )
        .await;
        assert!(
            out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
            "{out:?}"
        );
    }
    assert_eq!(Store::open(dir.path()).unwrap().last_id().unwrap(), 0);
    let home = tempfile::tempdir().unwrap();
    let out = yappr_hook(
        dir.path(),
        home.path(),
        &["config", "--hook-when", "always"],
        "",
    )
    .await;
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Agent hooks act: always"));
    let out = plugin_hook(
        dir.path(),
        "claude",
        "Stop",
        stop,
        &[("YAPPR_PRESENCE", "here")],
    )
    .await;
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(Store::open(dir.path()).unwrap().last_id().unwrap(), 1);
    // A plugin option replaces the saved setting.
    let out = plugin_hook(
        dir.path(),
        "claude",
        "Stop",
        stop,
        &[
            ("YAPPR_PRESENCE", "here"),
            ("CLAUDE_PLUGIN_OPTION_WHEN", "away"),
        ],
    )
    .await;
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(Store::open(dir.path()).unwrap().last_id().unwrap(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn tool_steps_collect_on_one_card_until_the_turn_ends() {
    let (dir, _device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let out = yappr_hook(
        dir.path(),
        home.path(),
        &["config", "--hook-events", "posttooluse,stop"],
        "",
    )
    .await;
    assert!(out.status.success(), "{out:?}");
    for n in 1..=7 {
        let input = serde_json::json!({"hook_event_name": "PostToolUse", "cwd": "/work/app", "tool_name": "Bash",
            "tool_input": {"command": format!("step {n}")}, "tool_response": "ok"}).to_string();
        let out = plugin_hook(dir.path(), "claude", "PostToolUse", &input, &[]).await;
        assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    }
    // Another project has a card of its own.
    let other = r#"{"hook_event_name":"PostToolUse","cwd":"/work/site","tool_name":"Read","tool_input":{"file_path":"/work/site/a"}}"#;
    plugin_hook(dir.path(), "claude", "PostToolUse", other, &[]).await;
    let entries = Store::open(dir.path()).unwrap().entries().unwrap();
    let card = &entries[6];
    assert_eq!(
        (
            card.kind.as_str(),
            card.subject.as_deref(),
            card.title.as_deref()
        ),
        (
            "state",
            Some("claude/app"),
            Some("Claude is working in app")
        )
    );
    assert_eq!(card.format.as_deref(), Some("markdown"));
    let steps: Vec<&str> = card
        .body
        .lines()
        .take_while(|l| l.starts_with("- "))
        .collect();
    assert_eq!(
        steps,
        [
            "- `Bash` · `step 3`",
            "- `Bash` · `step 4`",
            "- `Bash` · `step 5`",
            "- `Bash` · `step 6`",
            "- `Bash` · `step 7`"
        ]
    );
    assert!(
        card.body.contains("Last result:\n```\nok\n```"),
        "{}",
        card.body
    );
    assert_eq!(entries[7].subject.as_deref(), Some("claude/site"));
    assert!(entries[7].body.starts_with("- `Read` · `a`"));

    let stop =
        r#"{"hook_event_name":"Stop","cwd":"/work/app","last_assistant_message":"All done."}"#;
    plugin_hook(dir.path(), "claude", "Stop", stop, &[]).await;
    let input = r#"{"hook_event_name":"PostToolUse","cwd":"/work/app","tool_name":"Bash","tool_input":{"command":"next"}}"#;
    plugin_hook(dir.path(), "claude", "PostToolUse", input, &[]).await;
    let entries = Store::open(dir.path()).unwrap().entries().unwrap();
    assert_eq!(
        (entries[8].title.as_deref(), entries[8].body.as_str()),
        (Some("Claude finished in app"), "All done.")
    );
    assert_eq!(entries[9].body, "- `Bash` · `next`");
}
