//! Which follower identities can read this feed.
//!
//! A ticket is an address, not a secret. Only the endpoint ids in the allow
//! list can connect. The list is empty by default, so the default is deny.
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use iroh::{
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, Side, VarInt},
    EndpointId,
};

/// The QUIC application error code for a denied connection.
pub const DENIED_CODE: u32 = 403;
/// The QUIC close reason for a denied connection.
pub const DENIED_REASON: &[u8] = b"not allowed";

/// The allow list file: one endpoint id per line.
#[derive(Debug, Clone)]
pub struct AllowList {
    path: PathBuf,
}

impl AllowList {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Read the list. A missing file is an empty list.
    pub fn load(&self) -> Result<BTreeSet<EndpointId>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.path.display())),
        };
        let mut ids = BTreeSet::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line.parse() {
                Ok(id) => {
                    ids.insert(id);
                }
                Err(_) => tracing::warn!("{}: ignoring {line:?}", self.path.display()),
            }
        }
        Ok(ids)
    }

    /// True if `id` is in the list. Any read error gives `false`.
    pub fn contains(&self, id: &EndpointId) -> bool {
        match self.load() {
            Ok(ids) => ids.contains(id),
            Err(e) => {
                tracing::warn!("cannot read the allow list, denying {id}: {e:#}");
                false
            }
        }
    }

    /// Add `id`. Returns false if it was already in the list.
    pub fn allow(&self, id: EndpointId) -> Result<bool> {
        let _lock = crate::disk::lock(
            self.path.parent().context("allow list has no directory")?,
            "access.lock",
        )?;
        let mut ids = self.load()?;
        let added = ids.insert(id);
        if added {
            self.save(&ids)?;
        }
        Ok(added)
    }

    /// Remove `id`. Returns false if it was not in the list.
    pub fn deny(&self, id: &EndpointId) -> Result<bool> {
        let _lock = crate::disk::lock(
            self.path.parent().context("allow list has no directory")?,
            "access.lock",
        )?;
        let mut ids = self.load()?;
        let removed = ids.remove(id);
        if removed {
            self.save(&ids)?;
        }
        Ok(removed)
    }

    /// Replace the file in one step, so a reader never sees half a list.
    fn save(&self, ids: &BTreeSet<EndpointId>) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut file = crate::store::private_file(&tmp)?;
        file.set_len(0)?;
        for id in ids {
            writeln!(file, "{id}")?;
        }
        file.sync_all()?;
        fs::rename(&tmp, &self.path).with_context(|| format!("write {}", self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Reject unknown endpoints. The exception is the pairing protocol while an
/// invitation is open. Pairing then requires explicit writer confirmation.
#[derive(Debug, Clone)]
pub struct AccessHook {
    list: AllowList,
}

impl AccessHook {
    pub fn new(list: AllowList) -> Self {
        Self { list }
    }
}

impl EndpointHooks for AccessHook {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        if conn.side() == Side::Client {
            return AfterHandshakeOutcome::accept();
        }
        let remote = conn.remote_id();
        if self.list.contains(&remote) {
            tracing::info!("accepted {remote}");
            AfterHandshakeOutcome::accept()
        } else if conn.alpn() == crate::pairing::ALPN
            && self.list.path().parent().is_some_and(crate::pairing::open)
        {
            tracing::info!("accepted {remote} to pair");
            AfterHandshakeOutcome::accept()
        } else {
            tracing::info!("denied {remote}: not in the allow list");
            AfterHandshakeOutcome::Reject {
                error_code: VarInt::from_u32(DENIED_CODE),
                reason: DENIED_REASON.to_vec(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn id() -> EndpointId {
        SecretKey::generate().public()
    }

    #[test]
    fn missing_file_denies_everyone() {
        let dir = tempfile::tempdir().unwrap();
        let list = AllowList::new(dir.path().join("allowed"));
        assert!(list.load().unwrap().is_empty());
        assert!(!list.contains(&id()));
    }

    #[test]
    fn allow_then_deny() {
        let dir = tempfile::tempdir().unwrap();
        let list = AllowList::new(dir.path().join("allowed"));
        let (a, b) = (id(), id());
        assert!(list.allow(a).unwrap());
        assert!(!list.allow(a).unwrap());
        assert!(list.allow(b).unwrap());
        assert!(list.contains(&a) && list.contains(&b));
        assert!(list.deny(&a).unwrap());
        assert!(!list.deny(&a).unwrap());
        assert!(!list.contains(&a));
        assert!(list.contains(&b));
    }

    #[test]
    fn ignores_comments_blank_lines_and_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allowed");
        let a = id();
        fs::write(&path, format!("# laptop\n\n  {a}  \nnot-an-id\n")).unwrap();
        let list = AllowList::new(&path);
        assert_eq!(list.load().unwrap().into_iter().collect::<Vec<_>>(), [a]);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allowed");
        AllowList::new(&path).allow(id()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
