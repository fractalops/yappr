//! Feed entries and the rules that every entry must obey.
//!
//! The writer checks a [`Draft`] before it appends it. A follower checks every
//! [`Entry`] that it receives before it prints it, because the terminal is a
//! sensitive sink.
use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The version of the entry schema that this yappr writes and reads.
///
/// New optional fields do not change the version. A change that removes a
/// field or changes the meaning of a field increments it.
pub const SCHEMA_VERSION: u32 = 1;

/// Maximum size of one entry as a JSON line, in bytes. Extra fields count.
/// It keeps every entry below the frame limit of the wire protocol.
pub const MAX_ENTRY: usize = 768 * 1024;
/// Maximum size of an entry body, in bytes.
pub const MAX_BODY: usize = 256 * 1024;
/// Maximum size of the `source` field, in bytes.
pub const MAX_SOURCE: usize = 64;
/// Maximum size of the `kind` field, in bytes.
pub const MAX_KIND: usize = 32;
/// Maximum size of the `title` field, in bytes.
pub const MAX_TITLE: usize = 256;
/// Maximum size of a file name that yappr creates, in bytes.
pub const MAX_NAME: usize = 255;

/// One line of the feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The schema version, see [`SCHEMA_VERSION`].
    pub v: u32,
    pub id: u64,
    pub time: Timestamp,
    pub source: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub body: String,
    /// Fields that this version does not know. They are kept, so that an
    /// entry goes through a pipeline with no loss.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// An entry before the writer gives it an id and a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub source: String,
    pub kind: String,
    pub title: Option<String>,
    pub body: String,
    /// Fields that yappr does not use. The entry keeps them.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The reason why an entry is not valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// A field is longer than its limit.
    TooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// A field is empty, but it must have a value.
    Empty { field: &'static str },
    /// A field contains a control character that is not permitted.
    ControlChar {
        field: &'static str,
        ch: char,
        offset: usize,
    },
    /// The entry has no title and no body.
    NoContent,
    /// An input line is not a JSON object with the entry fields.
    NotJson { reason: String },
    /// A field is not valid UTF-8.
    NotUtf8 { field: &'static str, offset: usize },
    /// The entry uses a schema version that this yappr does not know.
    UnknownVersion { v: u32 },
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Invalid::TooLong { field, len, max } => {
                write!(f, "{field} is {len} bytes, the limit is {max} bytes")
            }
            Invalid::Empty { field } => write!(f, "{field} is empty"),
            Invalid::ControlChar { field, ch, offset } => write!(
                f,
                "{field} contains control character U+{:04X} at byte {offset}",
                *ch as u32
            ),
            Invalid::NoContent => write!(f, "entry has no title and no body"),
            Invalid::NotJson { reason } => write!(f, "not a JSON entry: {reason}"),
            Invalid::NotUtf8 { field, offset } => {
                write!(f, "{field} is not valid UTF-8 at byte {offset}")
            }
            Invalid::UnknownVersion { v } => write!(
                f,
                "entry has schema version {v}, but this yappr reads version \
                 {SCHEMA_VERSION} only. Install a newer yappr"
            ),
        }
    }
}

impl std::error::Error for Invalid {}

impl Draft {
    /// A draft from one JSON line, for example a line of
    /// `follow --output json`. A field that the line does not have comes from
    /// `defaults`. The writer sets `v`, `id` and `time`, so the line's values
    /// for these fields are dropped. Other fields go to `extra`.
    pub fn from_json(line: &str, defaults: &Draft) -> Result<Draft, Invalid> {
        let not_json = |reason: String| Invalid::NotJson { reason };
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| not_json(e.to_string()))?;
        let serde_json::Value::Object(mut fields) = value else {
            return Err(not_json("the line is not a JSON object".into()));
        };
        let mut text = |name: &str| -> Result<Option<String>, Invalid> {
            match fields.remove(name) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(s)) => Ok(Some(s)),
                Some(_) => Err(not_json(format!("{name} is not a string"))),
            }
        };
        let source = text("source")?.unwrap_or_else(|| defaults.source.clone());
        let kind = text("kind")?.unwrap_or_else(|| defaults.kind.clone());
        let title = text("title")?.or_else(|| defaults.title.clone());
        let body = text("body")?.unwrap_or_else(|| defaults.body.clone());
        for writer_field in ["v", "id", "time"] {
            fields.remove(writer_field);
        }
        Ok(Draft {
            source,
            kind,
            title,
            body,
            extra: fields,
        })
    }

    /// Check the draft against the entry rules.
    pub fn validate(&self) -> Result<(), Invalid> {
        validate(&self.source, &self.kind, self.title.as_deref(), &self.body)
    }
}

impl Entry {
    /// Check a received entry against the entry rules.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.v != SCHEMA_VERSION {
            return Err(Invalid::UnknownVersion { v: self.v });
        }
        validate(&self.source, &self.kind, self.title.as_deref(), &self.body)
    }
}

fn validate(source: &str, kind: &str, title: Option<&str>, body: &str) -> Result<(), Invalid> {
    label("source", source, MAX_SOURCE)?;
    label("kind", kind, MAX_KIND)?;
    if let Some(title) = title {
        limit("title", title, MAX_TITLE)?;
        no_control("title", title, |_| false)?;
    }
    limit("body", body, MAX_BODY)?;
    no_control("body", body, |c| c == '\n' || c == '\t')?;
    if body.is_empty() && title.is_none_or(str::is_empty) {
        return Err(Invalid::NoContent);
    }
    Ok(())
}

/// A short, one-line, non-empty field.
fn label(field: &'static str, value: &str, max: usize) -> Result<(), Invalid> {
    if value.is_empty() {
        return Err(Invalid::Empty { field });
    }
    limit(field, value, max)?;
    no_control(field, value, |_| false)
}

fn limit(field: &'static str, value: &str, max: usize) -> Result<(), Invalid> {
    if value.len() > max {
        return Err(Invalid::TooLong {
            field,
            len: value.len(),
            max,
        });
    }
    Ok(())
}

fn no_control(
    field: &'static str,
    value: &str,
    allowed: impl Fn(char) -> bool,
) -> Result<(), Invalid> {
    match value
        .char_indices()
        .find(|(_, c)| c.is_control() && !allowed(*c))
    {
        Some((offset, ch)) => Err(Invalid::ControlChar { field, ch, offset }),
        None => Ok(()),
    }
}

/// True if `name` is one path component that is safe to create in a directory.
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(body: &str) -> Draft {
        Draft {
            source: "disk".into(),
            kind: "info".into(),
            title: None,
            body: body.into(),
            extra: Default::default(),
        }
    }

    #[test]
    fn accepts_newline_and_tab_in_body() {
        assert_eq!(draft("a\tb\nc\n").validate(), Ok(()));
    }

    #[test]
    fn accepts_body_at_the_limit() {
        assert_eq!(draft(&"x".repeat(MAX_BODY)).validate(), Ok(()));
    }

    #[test]
    fn rejects_body_over_the_limit() {
        assert_eq!(
            draft(&"x".repeat(MAX_BODY + 1)).validate(),
            Err(Invalid::TooLong {
                field: "body",
                len: MAX_BODY + 1,
                max: MAX_BODY
            })
        );
    }

    #[test]
    fn rejects_escape_sequences_in_body() {
        assert_eq!(
            draft("ok\x1b[31mred").validate(),
            Err(Invalid::ControlChar {
                field: "body",
                ch: '\x1b',
                offset: 2
            })
        );
    }

    #[test]
    fn rejects_carriage_return_in_body() {
        assert!(matches!(
            draft("a\r\n").validate(),
            Err(Invalid::ControlChar { ch: '\r', .. })
        ));
    }

    #[test]
    fn rejects_c1_control_chars_in_body() {
        assert!(matches!(
            draft("a\u{9b}31m").validate(),
            Err(Invalid::ControlChar { ch: '\u{9b}', .. })
        ));
    }

    #[test]
    fn rejects_newline_in_title() {
        let mut d = draft("body");
        d.title = Some("two\nlines".into());
        assert!(matches!(
            d.validate(),
            Err(Invalid::ControlChar { field: "title", .. })
        ));
    }

    #[test]
    fn rejects_empty_source() {
        let mut d = draft("body");
        d.source = String::new();
        assert_eq!(d.validate(), Err(Invalid::Empty { field: "source" }));
    }

    #[test]
    fn rejects_long_kind() {
        let mut d = draft("body");
        d.kind = "k".repeat(MAX_KIND + 1);
        assert!(matches!(
            d.validate(),
            Err(Invalid::TooLong { field: "kind", .. })
        ));
    }

    #[test]
    fn rejects_entry_without_title_or_body() {
        assert_eq!(draft("").validate(), Err(Invalid::NoContent));
    }

    #[test]
    fn accepts_title_without_body() {
        let mut d = draft("");
        d.title = Some("backup done".into());
        assert_eq!(d.validate(), Ok(()));
    }

    fn defaults() -> Draft {
        Draft {
            source: "host".into(),
            kind: "info".into(),
            title: Some("default title".into()),
            body: String::new(),
            extra: Default::default(),
        }
    }

    #[test]
    fn json_draft_takes_missing_fields_from_the_defaults() {
        let d = Draft::from_json(r#"{"body":"hello"}"#, &defaults()).unwrap();
        assert_eq!(d.source, "host");
        assert_eq!(d.kind, "info");
        assert_eq!(d.title.as_deref(), Some("default title"));
        assert_eq!(d.body, "hello");
    }

    #[test]
    fn json_draft_fields_replace_the_defaults() {
        let d = Draft::from_json(
            r#"{"source":"nas","kind":"alert","title":"t","body":"b"}"#,
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            (d.source.as_str(), d.kind.as_str(), d.title.as_deref()),
            ("nas", "alert", Some("t"))
        );
    }

    #[test]
    fn json_draft_drops_writer_fields_and_keeps_the_others() {
        // a line from "follow --output json" of another feed
        let line = r#"{"v":1,"id":7,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"b","host":"nas"}"#;
        let d = Draft::from_json(line, &defaults()).unwrap();
        assert_eq!(d.extra.len(), 1, "{:?}", d.extra);
        assert_eq!(d.extra["host"], "nas");
    }

    #[test]
    fn json_draft_rejects_bad_json() {
        for line in ["not json", "[1,2]", r#"{"body":5}"#] {
            assert!(
                matches!(
                    Draft::from_json(line, &defaults()),
                    Err(Invalid::NotJson { .. })
                ),
                "{line}"
            );
        }
    }

    #[test]
    fn plain_file_names() {
        assert!(is_plain_file_name("default"));
        for bad in ["", ".", "..", "a/b", "a\\b", "a\nb"] {
            assert!(!is_plain_file_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn error_message_names_the_field_and_the_char() {
        let err = draft("\x07").validate().unwrap_err();
        assert_eq!(
            err.to_string(),
            "body contains control character U+0007 at byte 0"
        );
    }

    fn sample() -> Entry {
        serde_json::from_str(
            r#"{"v":1,"id":42,"time":"2026-10-27T09:30:00Z","source":"disk","kind":"info","title":"disk usage","body":"..."}"#,
        )
        .unwrap()
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let line = r#"{"v":1,"id":1,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"b","host":"nas","tags":["a"]}"#;
        let entry: Entry = serde_json::from_str(line).unwrap();
        assert_eq!(entry.extra["host"], "nas");
        let again: serde_json::Value = serde_json::to_value(&entry).unwrap();
        let want: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(again, want);
    }

    #[test]
    fn rejects_an_unknown_schema_version() {
        let mut entry = sample();
        entry.v = SCHEMA_VERSION + 1;
        assert_eq!(
            entry.validate(),
            Err(Invalid::UnknownVersion {
                v: SCHEMA_VERSION + 1
            })
        );
    }

    #[test]
    fn version_is_required() {
        let line =
            r#"{"id":1,"time":"2026-10-27T09:30:00Z","source":"s","kind":"info","body":"b"}"#;
        assert!(serde_json::from_str::<Entry>(line).is_err());
    }

    #[test]
    fn entry_json_matches_the_documented_shape() {
        let entry = Entry {
            v: SCHEMA_VERSION,
            id: 42,
            time: "2026-10-27T09:30:00Z".parse().unwrap(),
            source: "disk".into(),
            kind: "info".into(),
            title: Some("disk usage".into()),
            body: "...".into(),
            extra: Default::default(),
        };
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            r#"{"v":1,"id":42,"time":"2026-10-27T09:30:00Z","source":"disk","kind":"info","title":"disk usage","body":"..."}"#
        );
    }

    #[test]
    fn entry_validate_uses_the_same_rules() {
        let entry = Entry {
            v: SCHEMA_VERSION,
            id: 1,
            time: Timestamp::UNIX_EPOCH,
            source: "disk".into(),
            kind: "info".into(),
            title: None,
            body: "\x1b]0;pwned\x07".into(),
            extra: Default::default(),
        };
        assert!(matches!(entry.validate(), Err(Invalid::ControlChar { .. })));
    }
}
