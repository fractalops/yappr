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

#[tokio::test]
async fn hooks_install_keeps_other_hooks_and_remove_restores_the_file() {
    let (dir, _device) = cli_writer().await;
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join("claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original = "{\n  \"model\": \"x\",\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"other-tool\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
    std::fs::write(&settings, original).unwrap();

    let out = yappr_hook(dir.path(), home.path(), &["hooks", "install", "claude"], "").await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ask phone"), "{stderr}");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        value["hooks"]["Stop"][0]["hooks"][0]["command"],
        "other-tool"
    );
    let command = value["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.contains(" hook --agent claude --to 'phone'"),
        "{command}"
    );
    // The key order of the file stays.
    assert!(
        std::fs::read_to_string(&settings)
            .unwrap()
            .find("\"model\"")
            < std::fs::read_to_string(&settings)
                .unwrap()
                .find("\"hooks\"")
    );
    assert!(home
        .path()
        .join("claude/settings.json.yappr-backup")
        .exists());

    let status = yappr_hook(dir.path(), home.path(), &["hooks", "status"], "").await;
    let listed = String::from_utf8(status.stdout).unwrap();
    assert!(
        listed.contains("PermissionRequest") && listed.contains("Stop"),
        "{listed}"
    );

    let out = yappr_hook(dir.path(), home.path(), &["hooks", "remove", "claude"], "").await;
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    let status = yappr_hook(dir.path(), home.path(), &["hooks", "status", "claude"], "").await;
    assert!(String::from_utf8(status.stderr)
        .unwrap()
        .contains("No yappr hooks are installed"));

    let codex = yappr_hook(dir.path(), home.path(), &["hooks", "install", "codex"], "").await;
    assert!(String::from_utf8_lossy(&codex.stderr).contains("/hooks"));
    let value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("codex/hooks.json")).unwrap(),
    )
    .unwrap();
    assert!(value["hooks"].get("Notification").is_none());
    assert!(value["hooks"].get("PermissionRequest").is_some());
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
        assert_eq!(entry.title.as_deref(), Some("Claude: Bash"));
        assert_eq!(entry.body, "rm -rf build");
        assert_eq!(entry.subject.as_deref(), Some("project/app"));
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
        .unwrap();
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
        ("codex.stop", Some("Codex finished"))
    );
    assert_eq!(entry.body, "All tests pass.");

    // No device, and an event that is not JSON: exit 0 and no decision.
    let ask = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
    for (args, input) in [
        (vec!["hook", "--agent", "claude"], ask),
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
