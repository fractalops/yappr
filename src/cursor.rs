//! The follower's position in a feed, so a restart resumes where it stopped.
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use iroh::EndpointId;

use crate::entry::is_plain_file_name;

/// A position in a feed: the last entry id, and the log that it belongs to.
/// A position from before log names has no log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Position {
    pub id: u64,
    pub log: Option<String>,
}

/// The last entry id that a follower printed, for one feed and one name.
///
/// Different names give independent cursors, so a person in a terminal and a
/// script can read the same feed without taking entries from each other.
#[derive(Debug, Clone)]
pub struct Cursor {
    path: PathBuf,
}

impl Cursor {
    /// The cursor `name` for `feed`, stored under `dir/cursors/`.
    pub fn new(dir: &Path, feed: EndpointId, name: &str) -> Result<Self> {
        if !is_plain_file_name(name) || name.starts_with('.') {
            bail!("cursor name {name:?} is not a plain name");
        }
        Ok(Self {
            path: dir.join("cursors").join(format!("{feed}.{name}")),
        })
    }

    /// The saved position, or id 0 if there is no cursor yet. The file has
    /// the id, then a space and the log name.
    pub fn load(&self) -> Result<Position> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Position::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.path.display())),
        };
        let mut parts = text.trim().splitn(2, ' ');
        let id = parts.next().unwrap_or_default().parse().with_context(|| {
            format!(
                "cursor file {} is not valid. Remove it to start again",
                self.path.display()
            )
        })?;
        Ok(Position {
            id,
            log: parts.next().map(String::from),
        })
    }

    /// True if a value was saved before.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Replace the saved value. A unique temporary file and a rename make the
    /// change atomic. Other cursor names use other files.
    pub fn save(&self, id: u64, log: Option<&str>) -> Result<()> {
        let text = match log {
            Some(log) => format!("{id} {log}\n"),
            None => format!("{id}\n"),
        };
        crate::disk::write_bytes(&self.path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn feed() -> EndpointId {
        SecretKey::generate().public()
    }

    #[test]
    fn new_cursor_starts_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cursor::new(dir.path(), feed(), "default").unwrap();
        assert_eq!(c.load().unwrap(), Position::default());
    }

    #[test]
    fn save_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed();
        let cursor = Cursor::new(dir.path(), f, "default").unwrap();
        cursor.save(42, Some("2026-10-27T09:30:00.5Z")).unwrap();
        assert_eq!(
            Cursor::new(dir.path(), f, "default")
                .unwrap()
                .load()
                .unwrap(),
            Position {
                id: 42,
                log: Some("2026-10-27T09:30:00.5Z".into())
            }
        );
        fs::write(&cursor.path, "7\n").unwrap();
        assert_eq!(cursor.load().unwrap(), Position { id: 7, log: None });
    }

    #[test]
    fn names_and_feeds_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (feed(), feed());
        Cursor::new(dir.path(), a, "tty")
            .unwrap()
            .save(5, None)
            .unwrap();
        assert_eq!(
            Cursor::new(dir.path(), a, "jq").unwrap().load().unwrap().id,
            0
        );
        assert_eq!(
            Cursor::new(dir.path(), b, "tty")
                .unwrap()
                .load()
                .unwrap()
                .id,
            0
        );
    }

    #[test]
    fn rejects_a_name_with_a_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Cursor::new(dir.path(), feed(), "../x").is_err());
    }

    #[test]
    fn corrupt_file_is_an_error_that_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cursor::new(dir.path(), feed(), "default").unwrap();
        c.save(1, None).unwrap();
        fs::write(&c.path, "garbage").unwrap();
        let err = c.load().unwrap_err();
        assert!(
            format!("{err:#}").contains(&*c.path.to_string_lossy()),
            "{err:#}"
        );
    }
}
