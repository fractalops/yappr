//! The follower's position in a feed, so a restart resumes where it stopped.
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use iroh::EndpointId;

use crate::entry::is_plain_file_name;

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

    /// The saved id, or 0 if there is no cursor yet.
    pub fn load(&self) -> Result<u64> {
        match fs::read_to_string(&self.path) {
            Ok(text) => text.trim().parse().with_context(|| {
                format!(
                    "cursor file {} is not valid. Remove it to start again",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    /// Replace the saved value. A unique temporary file and a rename make the
    /// change atomic. Other cursor names use other files.
    pub fn save(&self, id: u64) -> Result<()> {
        crate::disk::write_bytes(&self.path, format!("{id}\n").as_bytes())
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
        assert_eq!(c.load().unwrap(), 0);
    }

    #[test]
    fn save_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed();
        Cursor::new(dir.path(), f, "default")
            .unwrap()
            .save(42)
            .unwrap();
        assert_eq!(
            Cursor::new(dir.path(), f, "default")
                .unwrap()
                .load()
                .unwrap(),
            42
        );
    }

    #[test]
    fn names_and_feeds_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (feed(), feed());
        Cursor::new(dir.path(), a, "tty").unwrap().save(5).unwrap();
        assert_eq!(Cursor::new(dir.path(), a, "jq").unwrap().load().unwrap(), 0);
        assert_eq!(
            Cursor::new(dir.path(), b, "tty").unwrap().load().unwrap(),
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
        c.save(1).unwrap();
        fs::write(&c.path, "garbage").unwrap();
        let err = c.load().unwrap_err();
        assert!(
            format!("{err:#}").contains(&*c.path.to_string_lossy()),
            "{err:#}"
        );
    }
}
