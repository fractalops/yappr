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
        Ok(Self {
            kinds: req.kinds.clone(),
            not_kinds: req.not_kinds.clone(),
            source,
        })
    }

    pub fn matches(&self, entry: &Entry) -> bool {
        (self.kinds.is_empty() || self.kinds.contains(&entry.kind))
            && !self.not_kinds.contains(&entry.kind)
            && self
                .source
                .as_ref()
                .is_none_or(|re| re.is_match(&entry.source))
    }
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
        }
    }
}

/// One message from the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
