//! Drive the built binary. Every test uses loopback
//! only (`YAPPR_NETWORK=local`), so no test touches the public relays.
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, ChildStderr, Command, Stdio},
    str::FromStr,
};

use yappr::EndpointTicket;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_yappr")
}

/// One machine: its own data directory and so its own identity.
struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn cmd<I, S>(&self, args: I) -> duct::Expression
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        duct::cmd(bin(), args)
            .env("YAPPR_DIR", self.dir.path())
            .env("YAPPR_NETWORK", "local")
            .env_remove("IROH_SECRET")
            .env_remove("NO_COLOR")
            .env_remove("RUST_LOG")
    }

    /// Run a command that must succeed. Returns stdout.
    fn run<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        let out = self
            .cmd(args)
            .stdin_null()
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .run()
            .unwrap();
        assert!(
            out.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Run a command that must fail. Returns (stdout, stderr).
    fn fail<I, S>(&self, args: I) -> (String, String)
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        let out = self
            .cmd(args)
            .stdin_null()
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .run()
            .unwrap();
        assert!(!out.status.success(), "command must fail");
        (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    /// Run a command with `stdin`. Returns (exit code, stdout, stderr).
    fn exit<I, S>(&self, args: I, stdin: &[u8]) -> (i32, String, String)
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        let out = self
            .cmd(args)
            .stdin_bytes(stdin)
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .run()
            .unwrap();
        (
            out.status.code().expect("exit code"),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    fn post(&self, body: &str, args: &[&str]) -> u64 {
        let mut all = vec!["post", "--body", body];
        all.extend_from_slice(args);
        self.run(all).trim().parse().unwrap()
    }

    fn id(&self) -> String {
        self.run(["id"]).trim().to_string()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env("YAPPR_DIR", self.dir.path())
            .env("YAPPR_NETWORK", "local")
            .env_remove("IROH_SECRET")
            .env_remove("NO_COLOR")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        cmd
    }
}

/// A running `yappr serve`. Kills the process on drop.
struct Serve {
    child: Child,
    ticket: String,
}

impl Serve {
    fn start(m: &Machine) -> Self {
        Self::start_with(m, &[])
    }

    fn start_with(m: &Machine, args: &[&str]) -> Self {
        let mut all = vec!["serve"];
        all.extend_from_slice(args);
        let mut child = m
            .command(&all)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let ticket = read_ticket(stderr);
        Self { child, ticket }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Read stderr up to the first `yappr follow <ticket>` line. Keep reading
/// the rest in a thread, so the server never blocks on a full pipe.
fn read_ticket(stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("serve exited before it printed a ticket:\n{seen}");
        }
        seen.push_str(&line);
        if let Some(ticket) = line.trim().strip_prefix("yappr follow ") {
            let ticket = ticket.to_string();
            EndpointTicket::from_str(&ticket).expect("valid ticket");
            std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
            return ticket;
        }
    }
}

fn json_ids(stdout: &str) -> Vec<u64> {
    stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["id"].as_u64().unwrap()
        })
        .collect()
}

/// A writer that serves, and a follower that the writer allows.
fn pair() -> (Machine, Machine) {
    let writer = Machine::new();
    let follower = Machine::new();
    writer.run(["allow", &follower.id()]);
    (writer, follower)
}

#[test]
fn post_prints_the_entry_id() {
    let m = Machine::new();
    assert_eq!(m.post("one", &[]), 1);
    assert_eq!(m.post("two", &[]), 2);
}

#[test]
fn post_reads_the_body_from_stdin() {
    let (writer, follower) = pair();
    let out = writer
        .cmd(["post", "--source", "disk", "--title", "disk usage"])
        .stdin_bytes("Filesystem Size\n/dev/sda1 100G\n")
        .read()
        .unwrap();
    assert_eq!(out.trim(), "1");
    let serve = Serve::start(&writer);
    let out = follower.run(["follow", &serve.ticket, "--no-follow", "--output", "json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["source"], "disk");
    assert_eq!(v["title"], "disk usage");
    assert_eq!(v["kind"], "info");
    assert_eq!(v["body"], "Filesystem Size\n/dev/sda1 100G\n");
}

#[test]
fn post_rejects_escape_sequences() {
    let m = Machine::new();
    let out = m
        .cmd(["post"])
        .stdin_bytes("\x1b[31mred\x1b[0m\n")
        .stderr_capture()
        .stdout_capture()
        .unchecked()
        .run()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("control character U+001B"), "{stderr}");
    assert!(out.stdout.is_empty());
}

#[test]
fn post_rejects_an_oversize_body() {
    let m = Machine::new();
    let out = m
        .cmd(["post"])
        .stdin_bytes(vec![b'x'; 256 * 1024 + 1])
        .stderr_capture()
        .stdout_capture()
        .unchecked()
        .run()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("the limit is 262144 bytes"), "{stderr}");
}

#[test]
fn post_rejects_invalid_utf8() {
    let m = Machine::new();
    let out = m
        .cmd(["post"])
        .stdin_bytes(vec![b'o', b'k', 0xff])
        .stderr_capture()
        .unchecked()
        .run()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
}

#[test]
fn allow_deny_allowed() {
    let m = Machine::new();
    let other = Machine::new().id();
    assert_eq!(m.run(["allowed"]), "");
    m.run(["allow", &other]);
    assert_eq!(m.run(["allowed"]).trim(), other);
    m.run(["deny", &other]);
    assert_eq!(m.run(["allowed"]), "");
    let (_, stderr) = m.fail(["allow", "not-an-id"]);
    assert!(stderr.contains("not-an-id"), "{stderr}");
}

#[test]
fn id_and_ticket_are_stable() {
    let m = Machine::new();
    assert_eq!(m.id(), m.id());
    let ticket = m.run(["ticket"]);
    let ticket = EndpointTicket::from_str(ticket.trim()).unwrap();
    assert_eq!(ticket.endpoint_addr().id.to_string(), m.id());
}

#[test]
fn follow_catches_up_then_resumes_from_the_cursor() {
    let (writer, follower) = pair();
    writer.post("one", &[]);
    writer.post("two", &[]);
    let serve = Serve::start(&writer);
    let follow = ["follow", &serve.ticket, "--no-follow", "--output", "json"];

    assert_eq!(json_ids(&follower.run(follow)), vec![1, 2]);
    assert_eq!(json_ids(&follower.run(follow)), Vec::<u64>::new());
    writer.post("three", &[]);
    assert_eq!(json_ids(&follower.run(follow)), vec![3]);
    // a second cursor name starts from the beginning
    let mut other = follow.to_vec();
    other.extend(["--cursor", "jq"]);
    assert_eq!(json_ids(&follower.run(other)), vec![1, 2, 3]);
}

#[test]
fn since_and_tail_do_not_move_the_cursor() {
    let (writer, follower) = pair();
    for b in ["a", "b", "c", "d"] {
        writer.post(b, &[]);
    }
    let serve = Serve::start(&writer);
    let t = serve.ticket.as_str();
    let json = ["--no-follow", "--output", "json"];
    let run = |extra: &[&str]| {
        let mut args = vec!["follow", t];
        args.extend_from_slice(&json);
        args.extend_from_slice(extra);
        json_ids(&follower.run(args))
    };
    assert_eq!(run(&["--tail", "2"]), vec![3, 4]);
    assert_eq!(run(&["--since", "1"]), vec![2, 3, 4]);
    assert_eq!(run(&["--since", "1h"]), vec![1, 2, 3, 4]);
    // the cursor did not move
    assert_eq!(run(&[]), vec![1, 2, 3, 4]);
}

#[test]
fn source_and_kind_filters() {
    let (writer, follower) = pair();
    writer.post("a", &["--source", "disk"]);
    writer.post("b", &["--source", "cpu", "--kind", "warn"]);
    writer.post("c", &["--source", "disk-2", "--kind", "alert"]);
    let serve = Serve::start(&writer);
    let base = ["follow", &serve.ticket, "--no-follow", "--output", "json"];
    let mut by_source = base.to_vec();
    by_source.extend(["--source", "^disk", "--cursor", "s"]);
    assert_eq!(json_ids(&follower.run(by_source)), vec![1, 3]);
    let mut by_kind = base.to_vec();
    by_kind.extend(["--kind", "warn", "--kind", "alert", "--cursor", "k"]);
    assert_eq!(json_ids(&follower.run(by_kind)), vec![2, 3]);
}

#[test]
fn json_output_passes_unknown_fields_through() {
    let (writer, follower) = pair();
    writer.post("first", &[]);
    // a newer writer added a field
    let line = r#"{"v":1,"id":2,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"b","host":"nas"}"#;
    let log = writer.dir.path().join("feed.ndjson");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str(line);
    text.push('\n');
    std::fs::write(&log, text).unwrap();
    let serve = Serve::start(&writer);
    let out = follower.run(["follow", &serve.ticket, "--no-follow", "--output", "json"]);
    let lines: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["v"], 1);
    assert_eq!(
        lines[1],
        serde_json::from_str::<serde_json::Value>(line).unwrap()
    );
}

#[test]
fn raw_output_prints_bodies_only() {
    let (writer, follower) = pair();
    writer.post("first", &["--title", "t"]);
    writer.post("second\n", &[]);
    let serve = Serve::start(&writer);
    let out = follower.run(["follow", &serve.ticket, "--no-follow", "--output", "raw"]);
    assert_eq!(out, "first\nsecond\n");
}

#[test]
fn text_output_uses_color_only_when_asked() {
    let (writer, follower) = pair();
    writer.post("body", &["--kind", "alert", "--title", "disk full"]);
    let serve = Serve::start(&writer);
    let t = serve.ticket.as_str();
    let plain = follower.run([
        "follow",
        t,
        "--no-follow",
        "--output",
        "text",
        "--cursor",
        "a",
    ]);
    assert!(!plain.contains('\x1b'), "{plain:?}");
    assert!(plain.contains("alert disk full"), "{plain:?}");
    let color = follower.run([
        "follow",
        t,
        "--no-follow",
        "--output",
        "text",
        "--color",
        "always",
        "--cursor",
        "b",
    ]);
    assert!(color.contains("\x1b[1;31malert\x1b[0m"), "{color:?}");
}

#[test]
fn a_pipe_gets_json_lines_by_default() {
    let (writer, follower) = pair();
    writer.post("first", &[]);
    writer.post("second", &["--title", "t"]);
    let serve = Serve::start(&writer);
    let out = follower.run(["follow", &serve.ticket, "--no-follow"]);
    let bodies: Vec<String> = out
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["body"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(bodies, ["first", "second"]);
}

#[test]
fn disallowed_follower_gets_nothing() {
    let writer = Machine::new();
    let stranger = Machine::new();
    writer.post("secret", &[]);
    let serve = Serve::start(&writer);
    let (stdout, stderr) = stranger.fail(["follow", &serve.ticket, "--no-follow"]);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains(&format!("yappr allow {}", stranger.id())),
        "{stderr}"
    );
    // a denied follower does not retry forever in follow mode either
    let (stdout, _) = stranger.fail(["follow", &serve.ticket]);
    assert_eq!(stdout, "");
}

#[test]
fn live_follow_prints_new_entries() {
    let (writer, follower) = pair();
    let serve = Serve::start(&writer);
    let mut child = follower
        .command(&["follow", &serve.ticket, "--output", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    writer.post("live", &[]);
    let line = lines.next().unwrap().unwrap();
    child.kill().ok();
    child.wait().ok();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["body"], "live");
}

#[test]
fn writer_restart_keeps_the_ticket_and_the_feed() {
    let (writer, follower) = pair();
    writer.post("before", &[]);
    let first = Serve::start(&writer);
    let follow =
        |t: &str| json_ids(&follower.run(["follow", t, "--no-follow", "--output", "json"]));
    assert_eq!(follow(&first.ticket), vec![1]);
    drop(first);
    writer.post("after", &[]);
    let second = Serve::start(&writer);
    let old = EndpointTicket::from_str(&second.ticket).unwrap();
    assert_eq!(old.endpoint_addr().id.to_string(), writer.id());
    assert_eq!(follow(&second.ticket), vec![2]);
}

/// Status and logs go to stderr, so stdout carries only entries.
#[test]
fn serve_writes_nothing_to_stdout() {
    let m = Machine::new();
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let mut child = m
        .command(&["serve"])
        .env("RUST_LOG", "debug")
        .stdout(stdout.reopen().unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    read_ticket(child.stderr.take().unwrap());
    child.kill().unwrap();
    child.wait().unwrap();
    let mut out = String::new();
    stdout.reopen().unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "");
}

#[cfg(unix)]
#[test]
fn store_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let m = Machine::new();
    m.post("x", &[]);
    m.id();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let feed = m.dir.path();
    assert_eq!(mode(&feed.join("feed.ndjson")), 0o600);
    assert_eq!(mode(&feed.join("secret.key")), 0o600);
}

// Exit codes are part of the interface. docs/scripting.md documents them.

#[test]
fn exit_2_on_a_usage_error() {
    let m = Machine::new();
    assert_eq!(m.exit(["follow"], b"").0, 2);
    assert_eq!(m.exit(["post", "--no-such-option"], b"").0, 2);
}

#[test]
fn exit_3_when_the_writer_denies_the_follower() {
    let writer = Machine::new();
    let stranger = Machine::new();
    writer.post("secret", &[]);
    let serve = Serve::start(&writer);
    let (code, stdout, _) = stranger.exit(["follow", &serve.ticket, "--no-follow"], b"");
    assert_eq!((code, stdout.as_str()), (3, ""));
    // follow mode does not retry a denial
    assert_eq!(stranger.exit(["follow", &serve.ticket], b"").0, 3);
}

#[test]
fn exit_4_when_post_gets_an_invalid_entry() {
    let m = Machine::new();
    for input in [
        b"\x1b[31mred\n".to_vec(),
        vec![b'x'; 256 * 1024 + 1],
        vec![b'o', b'k', 0xff],
        vec![],
    ] {
        let (code, stdout, stderr) = m.exit(["post"], &input);
        assert_eq!(code, 4, "{stderr}");
        assert_eq!(stdout, "");
    }
}

#[test]
fn exit_4_when_follow_gets_an_invalid_entry() {
    let (writer, follower) = pair();
    let line = r#"{"v":1,"id":1,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"\u001b[2J"}"#;
    std::fs::write(writer.dir.path().join("feed.ndjson"), format!("{line}\n")).unwrap();
    let serve = Serve::start(&writer);
    let (code, stdout, stderr) = follower.exit(["follow", &serve.ticket, "--no-follow"], b"");
    assert_eq!(code, 4, "{stderr}");
    assert_eq!(stdout, "");
}

#[test]
fn exit_5_when_the_writer_is_not_reachable() {
    let (writer, follower) = pair();
    // a ticket with no address, and no serve process
    let ticket = writer.run(["ticket"]);
    let (code, stdout, stderr) = follower.exit(["follow", ticket.trim(), "--no-follow"], b"");
    assert_eq!(code, 5, "{stderr}");
    assert_eq!(stdout, "");
}

#[test]
fn status_goes_to_stderr_only() {
    let (writer, follower) = pair();
    let other = Machine::new().id();
    let (_, stdout, stderr) = writer.exit(["allow", &other], b"");
    assert_eq!(stdout, "");
    assert!(stderr.contains("allowed"), "{stderr}");
    let (_, stdout, _) = writer.exit(["deny", &other], b"");
    assert_eq!(stdout, "");

    // follow mode retries an unreachable writer, and says so on stderr
    let ticket = writer.run(["ticket"]);
    let mut child = follower
        .command(&["follow", ticket.trim()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(line.to_lowercase().contains("retry in"), "{line}");
    assert_eq!(stdout, "");
}

#[test]
fn exit_0_when_the_reader_of_stdout_stops() {
    let (writer, follower) = pair();
    for i in 0..300 {
        writer.post(&format!("entry {i} {}", "x".repeat(1000)), &[]);
    }
    let serve = Serve::start(&writer);
    let mut child = follower
        .command(&["follow", &serve.ticket, "--output", "raw"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // like "| head -1": read one line, then close the pipe
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(first.starts_with("entry 0"), "{first}");
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(stderr, "");
}

/// A free UDP port on loopback.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// With a fixed port, the ticket stays valid after a restart. A running
/// follower connects again and gets the next entry.
#[cfg(unix)]
#[test]
fn a_restart_on_a_fixed_port_keeps_the_ticket_valid() {
    let (writer, follower) = pair();
    writer.post("before", &[]);
    let port = free_port().to_string();
    let first = Serve::start_with(&writer, &["--port", &port]);
    let ticket = first.ticket.clone();

    let mut child = follower
        .command(&["follow", &ticket, "--output", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(lines.next().unwrap().unwrap().contains("before"));

    // Ctrl-C, then wait, so the writer closes its connections. Without the
    // close, the follower waits for the QUIC idle timeout.
    let mut first = first;
    let status = Command::new("kill")
        .args(["-INT", &first.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(first.child.wait().unwrap().success());
    drop(first);

    let second = Serve::start_with(&writer, &["--port", &port]);
    assert_eq!(second.ticket, ticket, "same port, same ticket");
    writer.post("after", &[]);
    let line = lines.next().unwrap().unwrap();
    child.kill().ok();
    child.wait().ok();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["body"], "after");
}

// Streaming input to post.

/// The entries of a feed, read from its log file.
fn log_entries(m: &Machine) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(m.dir.path().join("feed.ndjson")).unwrap_or_default();
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn bodies(m: &Machine) -> Vec<String> {
    log_entries(m)
        .iter()
        .map(|e| e["body"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn post_lines_makes_one_entry_per_line() {
    let m = Machine::new();
    let (code, stdout, stderr) = m.exit(
        ["post", "--lines", "--source", "log"],
        b"first\n\nsecond\r\nthird",
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "1\n2\n3\n");
    assert_eq!(bodies(&m), ["first", "second", "third"]);
    assert!(log_entries(&m).iter().all(|e| e["source"] == "log"));
}

#[test]
fn post_lines_posts_each_line_when_it_arrives() {
    let m = Machine::new();
    let mut child = m
        .command(&["post", "--lines"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut ids = BufReader::new(child.stdout.take().unwrap()).lines();
    use std::io::Write;
    writeln!(stdin, "one").unwrap();
    // stdin is still open, and the entry is already in the feed
    assert_eq!(ids.next().unwrap().unwrap(), "1");
    assert_eq!(bodies(&m), ["one"]);
    writeln!(stdin, "two").unwrap();
    assert_eq!(ids.next().unwrap().unwrap(), "2");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn post_lines_skips_a_bad_line_and_exits_4() {
    let m = Machine::new();
    let (code, stdout, stderr) = m.exit(["post", "--lines"], b"ok\n\x1b[31mred\x1b[0m\nalso ok\n");
    assert_eq!(code, 4, "{stderr}");
    assert_eq!(stdout, "1\n2\n");
    assert_eq!(bodies(&m), ["ok", "also ok"]);
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("U+001B"), "{stderr}");
}

#[test]
fn post_every_groups_lines_into_one_entry() {
    let m = Machine::new();
    let (code, stdout, stderr) = m.exit(["post", "--every", "1m"], b"a\nb\nc\n");
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "1\n");
    assert_eq!(bodies(&m), ["a\nb\nc"]);
}

#[test]
fn post_every_starts_a_new_entry_after_the_interval() {
    let m = Machine::new();
    let mut child = m
        .command(&["post", "--every", "300ms"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut ids = BufReader::new(child.stdout.take().unwrap()).lines();
    use std::io::Write;
    writeln!(stdin, "a").unwrap();
    writeln!(stdin, "b").unwrap();
    // the interval ends, and post writes the first entry with stdin still open
    assert_eq!(ids.next().unwrap().unwrap(), "1");
    writeln!(stdin, "c").unwrap();
    drop(stdin);
    assert_eq!(ids.next().unwrap().unwrap(), "2");
    assert!(child.wait().unwrap().success());
    assert_eq!(bodies(&m), ["a\nb", "c"]);
}

#[cfg(unix)]
#[test]
fn post_every_posts_pending_lines_on_ctrl_c() {
    let m = Machine::new();
    let mut child = m
        .command(&["post", "--every", "1h"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    use std::io::Write;
    writeln!(stdin, "pending").unwrap();
    stdin.flush().unwrap();
    // post opens the feed after it installs its Ctrl-C handler
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !m.dir.path().join("feed.ndjson").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "post did not open the feed"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // let post read the line
    std::thread::sleep(std::time::Duration::from_millis(300));
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert_eq!(bodies(&m), ["pending"]);
}

#[test]
fn post_json_reads_one_entry_per_line() {
    let m = Machine::new();
    let input = concat!(
        r#"{"body":"uses the flags"}"#,
        "\n",
        r#"{"source":"nas","kind":"alert","title":"disk","body":"97%","host":"nas-1"}"#,
        "\n",
        "not json\n",
    );
    let (code, stdout, stderr) = m.exit(["post", "--json", "--source", "relay"], input.as_bytes());
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("line 3"), "{stderr}");
    assert_eq!(stdout, "1\n2\n");
    let entries = log_entries(&m);
    assert_eq!(entries[0]["source"], "relay");
    assert_eq!(entries[1]["source"], "nas");
    assert_eq!(entries[1]["kind"], "alert");
    assert_eq!(entries[1]["host"], "nas-1");
}

/// follow --output json | post --json copies one feed into another.
#[test]
fn post_json_relays_another_feed() {
    let (writer, follower) = pair();
    writer.post("one", &["--source", "a"]);
    writer.post("two", &["--source", "b", "--kind", "warn"]);
    let serve = Serve::start(&writer);
    let json = follower.run(["follow", &serve.ticket, "--no-follow", "--output", "json"]);

    let copy = Machine::new();
    let (code, stdout, stderr) = copy.exit(["post", "--json"], json.as_bytes());
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "1\n2\n");
    let entries = log_entries(&copy);
    assert_eq!(
        entries
            .iter()
            .map(|e| (
                e["source"].as_str().unwrap(),
                e["kind"].as_str().unwrap(),
                e["body"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [("a", "info", "one"), ("b", "warn", "two")]
    );
}

#[test]
fn post_stream_options_do_not_mix() {
    let m = Machine::new();
    assert_eq!(m.exit(["post", "--json", "--every", "1s"], b"").0, 2);
    assert_eq!(m.exit(["post", "--lines", "--body", "x"], b"").0, 2);
}

#[test]
fn questions_and_approvals_return_distinct_outcomes() {
    let (writer, device) = pair();
    let serve = Serve::start(&writer);
    let responder = device.id();
    let question = writer.run([
        "ask",
        &responder,
        "What next?",
        "--choice",
        "retry=Retry",
        "--choice",
        "reject",
        "--no-wait",
    ]);
    let question = question.trim();
    device.run(["answer", &serve.ticket, question, "--choice", "reject"]);
    assert_eq!(writer.exit(["wait", question], b"").0, 0);
    assert_eq!(writer.run(["wait", question]), "reject\n");
    let approval = writer.run([
        "approve",
        "--to",
        &responder,
        "--title",
        "Deploy?",
        "--no-wait",
    ]);
    let approval = approval.trim();
    device.run(["answer", &serve.ticket, approval, "--choice", "reject"]);
    let (code, stdout, _) = writer.exit(["wait", approval], b"");
    assert_eq!(code, 7);
    assert_eq!(stdout, "reject\n");
    let approved = writer.run([
        "approve",
        "--to",
        &responder,
        "--title",
        "Deploy staging?",
        "--no-wait",
    ]);
    device.run([
        "answer",
        &serve.ticket,
        approved.trim(),
        "--choice",
        "approve",
    ]);
    assert_eq!(writer.run(["wait", approved.trim()]), "approve\n");

    let states = writer.run(["questions"]);
    assert_eq!(states.lines().count(), 3);
    for line in states.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    let text = writer.run(["questions", "--output", "text"]);
    assert!(
        text.contains(&format!(
            "answered  Deploy staging?  (answer: approve)  {}",
            approved.trim()
        )),
        "{text}"
    );
}

#[test]
fn ask_needs_a_device_a_question_and_two_choices() {
    let (writer, device) = pair();
    let responder = device.id();
    let (code, _, stderr) = writer.exit(["ask", &responder, "Deploy?"], b"");
    assert_eq!(code, 2);
    assert!(stderr.contains("yappr approve"), "{stderr}");
    let (code, _, stderr) = writer.exit(["ask", "--choice", "a", "--choice", "b"], b"");
    assert_eq!(code, 2);
    assert!(stderr.contains("name the device"), "{stderr}");
    let (code, _, _) = writer.exit(["approve", &responder], b"");
    assert_eq!(code, 2);
}

#[test]
fn questions_report_cancellation_and_expiry() {
    let (writer, device) = pair();
    let responder = device.id();
    let id = writer.run([
        "approve",
        "--to",
        &responder,
        "--title",
        "Continue?",
        "--no-wait",
    ]);
    writer.run(["cancel", id.trim()]);
    assert_eq!(writer.exit(["wait", id.trim()], b"").0, 8);
    let id = writer.run([
        "approve",
        "--to",
        &responder,
        "--title",
        "Continue?",
        "--timeout",
        "100ms",
        "--no-wait",
    ]);
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(writer.exit(["wait", id.trim()], b"").0, 6);
}

#[test]
fn aliases_work_for_questions_choices_and_access() {
    let writer = Machine::new();
    let device = Machine::new();
    writer.run(["alias", "set", "device", &device.id()]);
    writer.run(["allow", "device"]);
    let server = Serve::start(&writer);
    device.run(["alias", "set", "laptop", &server.ticket]);
    let q = writer.run(["approve", "device", "Deploy?", "--no-wait"]);
    let answer = device.run(["answer", "laptop", q.trim(), "--choice", "approve"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer).unwrap()["value"],
        "approve"
    );
    assert_eq!(writer.run(["wait", q.trim()]).trim(), "approve");
    assert!(device.run(["devices"]).starts_with("laptop\t"));
    assert_eq!(
        writer.run(["devices"]),
        format!("device\t{}\tyes\n", device.id())
    );
    writer.run(["deny", "device"]);
    assert_eq!(
        writer.run(["devices"]),
        format!("device\t{}\tno\n", device.id())
    );
}
#[test]
fn multiple_writers_keep_separate_cursors_and_tag_json() {
    let a = Machine::new();
    let b = Machine::new();
    let device = Machine::new();
    a.run(["allow", &device.id()]);
    b.run(["allow", &device.id()]);
    a.post("laptop first", &[]);
    b.post("server first", &[]);
    let sa = Serve::start(&a);
    let sb = Serve::start(&b);
    device.run(["alias", "set", "laptop", &sa.ticket]);
    device.run(["alias", "set", "server", &sb.ticket]);
    let output = device.run([
        "follow",
        "laptop",
        "server",
        "--output",
        "json",
        "--no-follow",
    ]);
    let entries: Vec<serde_json::Value> = output
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(entries
        .iter()
        .any(|e| e["writer"] == "laptop" && e["entry"]["body"] == "laptop first"));
    assert!(entries
        .iter()
        .any(|e| e["writer"] == "server" && e["entry"]["body"] == "server first"));
    a.post("laptop second", &[]);
    let output = device.run([
        "follow",
        "laptop",
        "server",
        "--output",
        "json",
        "--no-follow",
    ]);
    assert_eq!(output.lines().count(), 1);
    assert!(output.contains("laptop second"));
    let (_, error) = device.fail(["follow", "laptop", &sa.ticket, "--no-follow"]);
    assert!(error.contains("more than once"));
}
#[test]
fn health_reports_the_selected_path_and_denied_access() {
    let writer = Machine::new();
    let device = Machine::new();
    writer.run(["allow", &device.id()]);
    let server = Serve::start(&writer);
    device.run(["alias", "set", "laptop", &server.ticket]);
    let health: serde_json::Value =
        serde_json::from_str(&device.run(["health", "laptop"])).unwrap();
    assert_eq!(health["health"]["state"], "connected");
    assert_eq!(health["health"]["route"], "direct");
    assert!(health["health"]["rtt_ms"].as_f64().unwrap() >= 0.0);
    let text = device.run(["health", "laptop", "--output", "text"]);
    assert!(text.starts_with("laptop: connected via direct ("), "{text}");
    assert!(text.trim_end().ends_with(" ms"), "{text}");
    writer.run(["deny", &device.id()]);
    let (exit, output, _) = device.exit(["health", "laptop"], b"");
    assert_eq!(exit, 3);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).unwrap()["health"]["state"],
        "denied"
    );
    let (exit, output, _) = device.exit(["health", "laptop", "--output", "text"], b"");
    assert_eq!((exit, output.as_str()), (3, "laptop: denied\n"));
}
#[test]
fn relay_settings_persist_and_validate_without_replacing_good_settings() {
    let machine = Machine::new();
    machine.run([
        "network",
        "set",
        "--relay",
        "https://relay.example.com",
        "--relay",
        "http://127.0.0.1:3340",
    ]);
    let custom: serde_json::Value =
        serde_json::from_str(&machine.run(["network", "show"])).unwrap();
    assert_eq!(custom["urls"].as_array().unwrap().len(), 2);
    machine.fail(["network", "set", "--relay", "ftp://relay.example.com"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&machine.run(["network", "show"])).unwrap(),
        custom
    );
    machine.run(["network", "set", "--no-relay"]);
    assert!(machine
        .run(["network", "show"])
        .contains("\"disabled\":true"));
    machine.run(["network", "set", "--defaults"]);
    assert_eq!(
        machine.run(["network", "show"]).trim(),
        "{\"disabled\":false,\"urls\":[]}"
    );
}

#[test]
fn one_denied_writer_does_not_discard_another_writers_entries() {
    let good = Machine::new();
    let denied = Machine::new();
    let device = Machine::new();
    good.run(["allow", &device.id()]);
    good.post("healthy feed", &[]);
    let a = Serve::start(&good);
    let b = Serve::start(&denied);
    device.run(["alias", "set", "good", &a.ticket]);
    device.run(["alias", "set", "denied", &b.ticket]);
    let (exit, output, _) = device.exit(
        [
            "follow",
            "denied",
            "good",
            "--no-follow",
            "--output",
            "json",
        ],
        b"",
    );
    assert_eq!(exit, 3);
    assert_eq!(output.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).unwrap()["entry"]["body"],
        "healthy feed"
    );
}
