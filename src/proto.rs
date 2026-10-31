//! The wire format of the `feed/0` protocol.
//!
//! The follower opens one bidirectional stream and sends one [`Request`].
//! The server sends [`Frame`]s until the request is complete. Each frame is a
//! u32 big-endian length, then that many bytes of JSON.
use anyhow::{bail, Context, Result};
use jiff::Timestamp;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::entry::Entry;

/// `text` with each control character written as an escape such as
/// `\u{1b}`. Use it for text from another endpoint that reaches a terminal,
/// so that the text cannot send terminal commands.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// The QUIC application error code for a denied connection.
pub const DENIED_CODE: u32 = 403;
/// The QUIC close reason for a denied connection.
pub const DENIED_REASON: &[u8] = b"not allowed";

/// Maximum size of a request frame, in bytes.
pub const MAX_REQUEST: usize = 4 * 1024;
/// Maximum size of a server frame, in bytes.
pub const MAX_FRAME: usize = 1024 * 1024;

/// What the follower wants to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Send entries with an id greater than this.
    #[serde(default)]
    pub since: u64,
    /// Skip entries older than this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_time: Option<Timestamp>,
    /// Send only the last N entries of the replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u64>,
    /// Stop after N entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// After the replay, wait for new entries and send them.
    #[serde(default = "yes")]
    pub follow: bool,
    /// Send only entries of these kinds. Empty means all kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// Do not send entries of these kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_kinds: Vec<String>,
    /// Send only entries with a source that matches this regex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Send only entries with a kind that matches one of these patterns. In a
    /// pattern, "*" matches any text. A writer from before patterns does not
    /// know this field and sends all kinds, so a reader sends its kinds here
    /// and not in `kinds`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kind_patterns: Vec<String>,
    /// Do not send entries with a kind that matches one of these patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_kind_patterns: Vec<String>,
    /// Send only entries with this level or a higher level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<String>,
    /// Send only entries with a subject that matches this pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// The largest compiled source regex, in bytes. The regex comes from another
/// device, so the writer limits the memory that it can use.
const MAX_SOURCE_REGEX: usize = 64 * 1024;

/// The entry filter of a [`Request`].
#[derive(Debug, Clone)]
pub struct Filter {
    kinds: Vec<String>,
    not_kinds: Vec<String>,
    source: Option<regex::Regex>,
    kind_patterns: Vec<String>,
    not_kind_patterns: Vec<String>,
    min_level: usize,
    subject: Option<String>,
}

impl Filter {
    /// Build the filter. An invalid or too large source regex is an error.
    pub fn new(req: &Request) -> Result<Self> {
        let source = req
            .source
            .as_deref()
            .map(|pattern| {
                regex::RegexBuilder::new(pattern)
                    .size_limit(MAX_SOURCE_REGEX)
                    .dfa_size_limit(MAX_SOURCE_REGEX)
                    .build()
                    .context("the source filter is not a valid regex")
            })
            .transpose()?;
        let min_level = match req.min_level.as_deref() {
            None => 0,
            Some(level) => crate::entry::level_rank(level).with_context(|| {
                format!(
                    "the level filter is {level:?}, but it must be one of: {}",
                    crate::entry::LEVELS.join(", ")
                )
            })?,
        };
        Ok(Self {
            kinds: req.kinds.clone(),
            not_kinds: req.not_kinds.clone(),
            source,
            kind_patterns: req.kind_patterns.clone(),
            not_kind_patterns: req.not_kind_patterns.clone(),
            min_level,
            subject: req.subject.clone(),
        })
    }

    pub fn matches(&self, entry: &Entry) -> bool {
        let kind = entry.kind_name();
        (self.kinds.is_empty() || self.kinds.contains(&entry.kind))
            && !self.not_kinds.contains(&entry.kind)
            && (self.kind_patterns.is_empty() || self.kind_patterns.iter().any(|p| glob(p, kind)))
            && !self.not_kind_patterns.iter().any(|p| glob(p, kind))
            && entry.level_rank() >= self.min_level
            && self
                .subject
                .as_deref()
                .is_none_or(|pattern| entry.subject.as_deref().is_some_and(|s| glob(pattern, s)))
            && self
                .source
                .as_ref()
                .is_none_or(|re| re.is_match(&entry.source))
    }
}

/// True if `text` matches `pattern`, where "*" matches any text, also an
/// empty text. Other characters match only themselves.
pub fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else {
        // no "*" in the pattern
        return rest.is_empty();
    };
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

fn yes() -> bool {
    true
}

impl Default for Request {
    fn default() -> Self {
        Self {
            since: 0,
            since_time: None,
            tail: None,
            limit: None,
            follow: true,
            kinds: Vec::new(),
            not_kinds: Vec::new(),
            source: None,
            kind_patterns: Vec::new(),
            not_kind_patterns: Vec::new(),
            min_level: None,
            subject: None,
        }
    }
}

/// One message from the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Almost every value is an entry, so a box would save no memory.
#[allow(clippy::large_enum_variant)]
pub enum Frame {
    /// One feed entry.
    Entry { entry: Entry },
    /// The replay is complete. `head` is the id of the last entry in the feed.
    CaughtUp { head: u64 },
    /// The server cannot complete the request.
    Error { message: String },
    /// A frame type from a newer server. Followers ignore it.
    #[serde(other)]
    Unknown,
}

/// Write one length-prefixed JSON frame.
pub async fn write_frame<T: Serialize>(
    w: &mut (impl AsyncWrite + Unpin),
    value: &T,
    max: usize,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > max {
        bail!("frame is {} bytes, the limit is {max} bytes", bytes.len());
    }
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

/// Read one length-prefixed JSON frame. Returns `None` at the end of the
/// stream, if the stream ends between frames.
pub async fn read_frame<T: DeserializeOwned>(
    r: &mut (impl AsyncRead + Unpin),
    max: usize,
) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    // A clean end of stream can only come before the first byte of a frame.
    if r.read(&mut len[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut len[1..]).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        bail!("frame is {len} bytes, the limit is {max} bytes");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = Frame::CaughtUp { head: 7 };
        write_frame(&mut a, &sent, MAX_FRAME).await.unwrap();
        drop(a);
        let got: Frame = read_frame(&mut b, MAX_FRAME).await.unwrap().unwrap();
        assert_eq!(got, sent);
        assert!(read_frame::<Frame>(&mut b, MAX_FRAME)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn read_rejects_length_over_the_cap() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&(MAX_REQUEST as u32 + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_frame::<Request>(&mut b, MAX_REQUEST)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[tokio::test]
    async fn write_rejects_frame_over_the_cap() {
        let (mut a, _b) = tokio::io::duplex(1024);
        let big = Frame::Error {
            message: "x".repeat(100),
        };
        assert!(write_frame(&mut a, &big, 50).await.is_err());
    }

    #[tokio::test]
    async fn read_fails_on_a_partial_frame() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&10u32.to_be_bytes()).await.unwrap();
        a.write_all(b"{\"ty").await.unwrap();
        drop(a);
        assert!(read_frame::<Frame>(&mut b, MAX_FRAME).await.is_err());
    }

    #[tokio::test]
    async fn read_fails_on_a_partial_length() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&[0, 0]).await.unwrap();
        drop(a);
        assert!(read_frame::<Frame>(&mut b, MAX_FRAME).await.is_err());
    }

    #[test]
    fn glob_matches_with_stars() {
        for (pattern, text) in [
            ("deploy.*", "deploy.finished"),
            ("*", ""),
            ("a*b*c", "aXbYc"),
            ("backup/*", "backup/nas"),
            ("event", "event"),
            ("*.failed", "build.failed"),
        ] {
            assert!(glob(pattern, text), "{pattern} {text}");
        }
        for (pattern, text) in [
            ("deploy.*", "deploy"),
            ("a*a", "a"),
            ("event", "events"),
            ("ab*b", "ab"),
            ("*.failed", "failed"),
        ] {
            assert!(!glob(pattern, text), "{pattern} {text}");
        }
    }

    #[test]
    fn empty_request_means_everything_then_follow() {
        let req: Request = serde_json::from_str("{}").unwrap();
        assert_eq!(req, Request::default());
        assert!(req.follow);
    }

    #[test]
    fn filter_checks_kinds_and_source() {
        let entry = |kind: &str, source: &str| Entry {
            v: crate::entry::SCHEMA_VERSION,
            id: 1,
            time: Timestamp::UNIX_EPOCH,
            source: source.into(),
            kind: kind.into(),
            level: None,
            subject: None,
            format: None,
            data: None,
            title: None,
            body: "x".into(),
            extra: Default::default(),
        };
        let all = Filter::new(&Request::default()).unwrap();
        assert!(all.matches(&entry("log", "nginx")));
        let inbox = Filter::new(&Request {
            not_kinds: vec!["log".into()],
            ..Default::default()
        })
        .unwrap();
        assert!(!inbox.matches(&entry("log", "nginx")));
        assert!(inbox.matches(&entry("question", "yappr")));
        let alerts = Filter::new(&Request {
            kinds: vec!["warn".into(), "alert".into()],
            source: Some("^disk$".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(alerts.matches(&entry("alert", "disk")));
        assert!(!alerts.matches(&entry("alert", "diskette")));
        assert!(!alerts.matches(&entry("info", "disk")));
    }

    #[test]
    fn filter_checks_patterns_levels_and_subjects() {
        let entry = |kind: &str, level: Option<&str>, subject: Option<&str>| Entry {
            v: crate::entry::SCHEMA_VERSION,
            id: 1,
            time: Timestamp::UNIX_EPOCH,
            source: "ci".into(),
            kind: kind.into(),
            level: level.map(Into::into),
            subject: subject.map(Into::into),
            format: None,
            data: None,
            title: None,
            body: "x".into(),
            extra: Default::default(),
        };
        let filter = |req: Request| Filter::new(&req).unwrap();
        let deploys = filter(Request {
            kind_patterns: vec!["deploy.*".into()],
            ..Default::default()
        });
        assert!(deploys.matches(&entry("deploy.finished", None, None)));
        assert!(!deploys.matches(&entry("build.failed", None, None)));
        // an old entry of kind "warn" is an event
        let events = filter(Request {
            kind_patterns: vec!["event".into()],
            ..Default::default()
        });
        assert!(events.matches(&entry("warn", None, None)));
        let urgent = filter(Request {
            min_level: Some("warn".into()),
            ..Default::default()
        });
        assert!(urgent.matches(&entry("event", Some("alert"), None)));
        assert!(urgent.matches(&entry("warn", None, None)));
        assert!(!urgent.matches(&entry("event", None, None)));
        let backups = filter(Request {
            subject: Some("backup/*".into()),
            ..Default::default()
        });
        assert!(backups.matches(&entry("state", None, Some("backup/nas"))));
        assert!(!backups.matches(&entry("state", None, Some("deploy/web"))));
        assert!(!backups.matches(&entry("state", None, None)));
        assert!(Filter::new(&Request {
            min_level: Some("loud".into()),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn filter_rejects_a_bad_or_large_regex() {
        let with = |source: &str| {
            Filter::new(&Request {
                source: Some(source.into()),
                ..Default::default()
            })
        };
        assert!(with("(").is_err());
        assert!(with(&"a{1000}".repeat(100)).is_err());
    }

    #[test]
    fn request_without_filters_has_no_filter_fields() {
        let json = serde_json::to_string(&Request::default()).unwrap();
        assert!(
            !json.contains("kinds") && !json.contains("source"),
            "{json}"
        );
    }

    #[test]
    fn request_ignores_unknown_fields() {
        let req: Request = serde_json::from_str(r#"{"since":3,"from_the_future":true}"#).unwrap();
        assert_eq!(req.since, 3);
    }

    #[test]
    fn frame_json_shape() {
        assert_eq!(
            serde_json::to_string(&Frame::CaughtUp { head: 3 }).unwrap(),
            r#"{"type":"caught_up","head":3}"#
        );
        let f: Frame = serde_json::from_str(r#"{"type":"from_the_future","x":1}"#).unwrap();
        assert_eq!(f, Frame::Unknown);
    }
}
