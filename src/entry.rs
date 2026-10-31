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
/// Maximum size of the `subject` field, in bytes.
pub const MAX_SUBJECT: usize = 256;
/// Maximum size of the `data` field as JSON, in bytes.
pub const MAX_DATA: usize = 64 * 1024;

/// The kind of an entry that has no other kind.
pub const DEFAULT_KIND: &str = "event";
/// The levels from the lowest to the highest. An entry without a level has
/// the lowest level.
pub const LEVELS: [&str; 3] = ["info", "warn", "alert"];
/// The values of the `format` field. A body without a format is text.
pub const FORMATS: [&str; 2] = ["text", "markdown"];

/// One line of the feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The schema version, see [`SCHEMA_VERSION`].
    pub v: u32,
    pub id: u64,
    pub time: Timestamp,
    pub source: String,
    pub kind: String,
    /// How urgent the entry is, one of [`LEVELS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// What the entry is about, for example "backup/nas". Entries with the
    /// same subject belong together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub body: String,
    /// How to show the body, one of [`FORMATS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// A JSON object for programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Map<String, serde_json::Value>>,
    /// Fields that this version does not know. They are kept, so that an
    /// entry goes through a pipeline with no loss.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The optional fields that a post and a question share.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    pub level: Option<String>,
    pub subject: Option<String>,
    pub format: Option<String>,
    pub data: Option<serde_json::Map<String, serde_json::Value>>,
}

/// An entry before the writer gives it an id and a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub source: String,
    pub kind: String,
    pub level: Option<String>,
    pub subject: Option<String>,
    pub title: Option<String>,
    pub body: String,
    pub format: Option<String>,
    pub data: Option<serde_json::Map<String, serde_json::Value>>,
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
    /// A field contains a space, but it must be one word.
    Space { field: &'static str, offset: usize },
    /// A field has a value that is not one of its permitted values.
    NotAllowed {
        field: &'static str,
        value: String,
        allowed: &'static [&'static str],
    },
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
            Invalid::Space { field, offset } => {
                write!(f, "{field} contains a space at byte {offset}")
            }
            Invalid::NotAllowed {
                field,
                value,
                allowed,
            } => write!(
                f,
                "{field} is {value:?}, but it must be one of: {}",
                allowed.join(", ")
            ),
        }
    }
}

impl std::error::Error for Invalid {}

impl Draft {
    /// A draft from one JSON line, for example a line of
    /// `read --output json`. A field that the line does not have comes from
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
        let level = text("level")?.or_else(|| defaults.level.clone());
        let subject = text("subject")?.or_else(|| defaults.subject.clone());
        let title = text("title")?.or_else(|| defaults.title.clone());
        let body = text("body")?.unwrap_or_else(|| defaults.body.clone());
        let format = text("format")?.or_else(|| defaults.format.clone());
        let data = match fields.remove("data") {
            None | Some(serde_json::Value::Null) => defaults.data.clone(),
            Some(serde_json::Value::Object(data)) => Some(data),
            Some(_) => return Err(not_json("data is not a JSON object".into())),
        };
        for writer_field in ["v", "id", "time"] {
            fields.remove(writer_field);
        }
        Ok(Draft {
            source,
            kind,
            level,
            subject,
            title,
            body,
            format,
            data,
            extra: fields,
        })
    }

    /// Check the draft against the entry rules. A draft must also use a
    /// known level and format.
    pub fn validate(&self) -> Result<(), Invalid> {
        validate(&Fields {
            source: &self.source,
            kind: &self.kind,
            level: self.level.as_deref(),
            subject: self.subject.as_deref(),
            title: self.title.as_deref(),
            body: &self.body,
            format: self.format.as_deref(),
            data: self.data.as_ref(),
        })?;
        known("level", self.level.as_deref(), &LEVELS)?;
        known("format", self.format.as_deref(), &FORMATS)
    }
}

impl Entry {
    /// Check a received entry against the entry rules.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.v != SCHEMA_VERSION {
            return Err(Invalid::UnknownVersion { v: self.v });
        }
        validate(&Fields {
            source: &self.source,
            kind: &self.kind,
            level: self.level.as_deref(),
            subject: self.subject.as_deref(),
            title: self.title.as_deref(),
            body: &self.body,
            format: self.format.as_deref(),
            data: self.data.as_ref(),
        })
    }

    /// The level of the entry. An entry from before levels has the level in
    /// its kind: "warn" or "alert". A level that this version does not know
    /// counts as the lowest level.
    pub fn level(&self) -> &str {
        let level = self.level.as_deref().unwrap_or(self.kind.as_str());
        LEVELS
            .iter()
            .copied()
            .find(|l| *l == level)
            .unwrap_or(LEVELS[0])
    }

    /// The kind of the entry. Before levels, an event had the kind "info",
    /// "warn" or "alert", so these kinds are events.
    pub fn kind_name(&self) -> &str {
        match self.kind.as_str() {
            "info" | "warn" | "alert" => DEFAULT_KIND,
            kind => kind,
        }
    }

    /// The position of the level in [`LEVELS`].
    pub fn level_rank(&self) -> usize {
        level_rank(self.level()).unwrap_or(0)
    }
}

/// The position of `level` in [`LEVELS`].
pub fn level_rank(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

/// The fields that the entry rules check.
struct Fields<'a> {
    source: &'a str,
    kind: &'a str,
    level: Option<&'a str>,
    subject: Option<&'a str>,
    title: Option<&'a str>,
    body: &'a str,
    format: Option<&'a str>,
    data: Option<&'a serde_json::Map<String, serde_json::Value>>,
}

/// A received entry can have a level or a format from a newer version, so
/// only a draft must use the known values.
fn validate(f: &Fields) -> Result<(), Invalid> {
    let Fields {
        source,
        kind,
        level,
        subject,
        title,
        body,
        format,
        data,
    } = *f;
    label("source", source, MAX_SOURCE)?;
    label("kind", kind, MAX_KIND)?;
    if let Some(level) = level {
        label("level", level, MAX_KIND)?;
    }
    if let Some(format) = format {
        label("format", format, MAX_KIND)?;
    }
    if let Some(subject) = subject {
        label("subject", subject, MAX_SUBJECT)?;
        if let Some((offset, _)) = subject.char_indices().find(|(_, c)| c.is_whitespace()) {
            return Err(Invalid::Space {
                field: "subject",
                offset,
            });
        }
    }
    if let Some(data) = data {
        let len = serde_json::to_string(data).map_or(usize::MAX, |s| s.len());
        if len > MAX_DATA {
            return Err(Invalid::TooLong {
                field: "data",
                len,
                max: MAX_DATA,
            });
        }
    }
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

fn known(
    field: &'static str,
    value: Option<&str>,
    allowed: &'static [&'static str],
) -> Result<(), Invalid> {
    match value {
        Some(value) if !allowed.contains(&value) => Err(Invalid::NotAllowed {
            field,
            value: value.into(),
            allowed,
        }),
        _ => Ok(()),
    }
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
            level: None,
            subject: None,
            format: None,
            data: None,
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
            level: None,
            subject: None,
            format: None,
            data: None,
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
        // a line from "read --output json" of another feed
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
    fn a_draft_must_use_a_known_level_and_format() {
        let mut d = draft("body");
        d.level = Some("urgent".into());
        assert!(matches!(
            d.validate(),
            Err(Invalid::NotAllowed { field: "level", .. })
        ));
        d.level = Some("alert".into());
        d.format = Some("html".into());
        assert!(matches!(
            d.validate(),
            Err(Invalid::NotAllowed {
                field: "format",
                ..
            })
        ));
        d.format = Some("markdown".into());
        assert_eq!(d.validate(), Ok(()));
    }

    #[test]
    fn a_subject_is_one_word() {
        let mut d = draft("body");
        d.subject = Some("backup/nas".into());
        assert_eq!(d.validate(), Ok(()));
        d.subject = Some("backup nas".into());
        assert_eq!(
            d.validate(),
            Err(Invalid::Space {
                field: "subject",
                offset: 6
            })
        );
    }

    #[test]
    fn data_has_a_size_limit() {
        let mut d = draft("body");
        let mut data = serde_json::Map::new();
        data.insert("x".into(), "y".repeat(MAX_DATA).into());
        d.data = Some(data);
        assert!(matches!(
            d.validate(),
            Err(Invalid::TooLong { field: "data", .. })
        ));
    }

    #[test]
    fn json_draft_reads_the_new_fields() {
        let line = r#"{"body":"b","level":"warn","subject":"deploy/web","format":"markdown","data":{"v":"1.4.2"}}"#;
        let d = Draft::from_json(line, &defaults()).unwrap();
        assert_eq!(d.level.as_deref(), Some("warn"));
        assert_eq!(d.subject.as_deref(), Some("deploy/web"));
        assert_eq!(d.format.as_deref(), Some("markdown"));
        assert_eq!(d.data.unwrap()["v"], "1.4.2");
        assert!(d.extra.is_empty());
        let bad = Draft::from_json(r#"{"body":"b","data":[1]}"#, &defaults());
        assert!(matches!(bad, Err(Invalid::NotJson { .. })));
    }

    #[test]
    fn an_old_entry_has_its_level_in_the_kind() {
        let mut e = sample();
        for (kind, level, name) in [
            ("info", "info", "event"),
            ("warn", "warn", "event"),
            ("alert", "alert", "event"),
            ("deploy", "info", "deploy"),
        ] {
            e.kind = kind.into();
            assert_eq!((e.level(), e.kind_name()), (level, name), "{kind}");
        }
        e.kind = "deploy".into();
        e.level = Some("alert".into());
        assert_eq!(e.level(), "alert");
        // a level from a newer version counts as the lowest level
        e.level = Some("critical".into());
        assert_eq!((e.level(), e.validate()), ("info", Ok(())));
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
            level: None,
            subject: None,
            format: None,
            data: None,
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
            level: None,
            subject: None,
            format: None,
            data: None,
            title: None,
            body: "\x1b]0;pwned\x07".into(),
            extra: Default::default(),
        };
        assert!(matches!(entry.validate(), Err(Invalid::ControlChar { .. })));
    }
}
