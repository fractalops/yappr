//! The local feed store: an append-only NDJSON log.
//!
//! Many processes can append at the same time. An exclusive lock on
//! `feed.lock` gives each entry a unique id. Every append writes one complete
//! line and then calls fsync. A crash can leave a partial last line. Readers
//! never return a line that does not end in a newline, and the next append
//! removes the partial line.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use jiff::Timestamp;

use crate::{
    access::AllowList,
    entry::{self, Draft, Entry},
};

const LOG: &str = "feed.ndjson";
const LOCK: &str = "feed.lock";

/// A feed directory on disk.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Open the feed in `dir`. Create the directory if it does not exist.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        create_private_dir(&dir)?;
        let store = Self { dir };
        store.open_log()?;
        Ok(store)
    }

    /// The feed directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Give the draft the next id and the current time, then append it.
    pub fn append(&self, draft: Draft) -> Result<Entry> {
        draft.validate()?;
        let lock = private_file(&self.dir.join(LOCK))?;
        lock.lock().context("lock feed")?;
        self.append_locked(draft)
    }

    /// Read the feed and append one record under the same process lock.
    pub fn transaction<T>(
        &self,
        apply: impl FnOnce(&[Entry]) -> Result<(Option<Draft>, T)>,
    ) -> Result<T> {
        let lock = private_file(&self.dir.join(LOCK))?;
        lock.lock().context("lock feed")?;
        let entries = self.entries()?;
        let (draft, result) = apply(&entries)?;
        if let Some(draft) = draft {
            self.append_locked(draft)?;
        }
        Ok(result)
    }

    /// Read all complete records.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut reader = self.reader()?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry()? {
            entries.push(entry);
        }
        Ok(entries)
    }

    fn append_locked(&self, draft: Draft) -> Result<Entry> {
        draft.validate()?;
        let mut log = self.open_log()?;
        let (complete_end, last) = last_line(&mut log)?;
        if complete_end != log.metadata()?.len() {
            tracing::warn!("removing a partial entry at byte {complete_end}");
            log.set_len(complete_end)?;
        }
        let last_id = match last {
            Some(line) => parse(&line)?.id,
            None => 0,
        };
        let entry = Entry {
            v: entry::SCHEMA_VERSION,
            id: last_id + 1,
            time: Timestamp::now(),
            source: draft.source,
            kind: draft.kind,
            level: draft.level,
            subject: draft.subject,
            title: draft.title,
            body: draft.body,
            format: draft.format,
            data: draft.data,
            extra: draft.extra,
        };
        let mut line = serde_json::to_vec(&entry)?;
        if line.len() > entry::MAX_ENTRY {
            return Err(entry::Invalid::TooLong {
                field: "entry",
                len: line.len(),
                max: entry::MAX_ENTRY,
            }
            .into());
        }
        line.push(b'\n');
        log.write_all(&line).context("write entry")?;
        log.sync_all().context("sync feed")?;
        Ok(entry)
    }

    fn open_log(&self) -> Result<File> {
        let path = self.dir.join(LOG);
        let mut opts = OpenOptions::new();
        opts.read(true).append(true).create(true);
        private_options(&mut opts);
        let file = opts
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        make_private(&file, &path, 0o600)?;
        Ok(file)
    }

    /// The id of the last complete entry, or 0 if the feed is empty.
    pub fn last_id(&self) -> Result<u64> {
        let mut log = self.open_log()?;
        match last_line(&mut log)?.1 {
            Some(line) => Ok(parse(&line)?.id),
            None => Ok(0),
        }
    }

    /// A reader that starts at the first entry.
    pub fn reader(&self) -> Result<LogReader> {
        Ok(LogReader {
            file: self.open_log()?,
            pos: 0,
            buf: Vec::new(),
        })
    }

    /// The followers that can read this feed.
    pub fn allow_list(&self) -> AllowList {
        AllowList::new(self.dir.join("allowed"))
    }
}

/// Reads entries from the log in order.
///
/// At the end of the log, [`LogReader::next_entry`] returns `None`. Call it
/// again after more entries arrive to continue from the same position.
#[derive(Debug)]
pub struct LogReader {
    file: File,
    pos: u64,
    buf: Vec<u8>,
}

impl LogReader {
    /// The next complete entry, or `None` at the end of the log.
    pub fn next_entry(&mut self) -> Result<Option<Entry>> {
        loop {
            if let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=nl).collect();
                let start = self.pos;
                self.pos += line.len() as u64;
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                let entry = parse(line).with_context(|| format!("entry at byte {start}"))?;
                return Ok(Some(entry));
            }
            self.file
                .seek(SeekFrom::Start(self.pos + self.buf.len() as u64))?;
            let mut chunk = [0u8; 64 * 1024];
            let n = self.file.read(&mut chunk)?;
            if n == 0 {
                // Forget a partial line. The writer can remove it and write
                // a different line in its place.
                self.buf.clear();
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn parse(line: &[u8]) -> Result<Entry> {
    Ok(serde_json::from_slice(line)?)
}

/// The end of the last complete line, and the content of that line.
fn last_line(file: &mut File) -> io::Result<(u64, Option<Vec<u8>>)> {
    let len = file.metadata()?.len();
    let Some(last_nl) = rfind_newline(file, len)? else {
        return Ok((0, None));
    };
    let start = rfind_newline(file, last_nl)?.map_or(0, |p| p + 1);
    let mut line = vec![0u8; (last_nl - start) as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut line)?;
    Ok((last_nl + 1, Some(line)))
}

/// The position of the last newline before `end`.
fn rfind_newline(file: &mut File, end: u64) -> io::Result<Option<u64>> {
    let mut chunk = [0u8; 8 * 1024];
    let mut end = end;
    while end > 0 {
        let start = end.saturating_sub(chunk.len() as u64);
        let buf = &mut chunk[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(buf)?;
        if let Some(i) = buf.iter().rposition(|b| *b == b'\n') {
            return Ok(Some(start + i as u64));
        }
        end = start;
    }
    Ok(None)
}

/// Create a directory that only the owner can read. An existing directory of
/// this user loses access for other users.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let handle = File::open(dir).with_context(|| format!("open {}", dir.display()))?;
    make_private(&handle, dir, 0o700)
}

/// Open or create a file that only the owner can read. The path must not be a
/// symbolic link.
pub(crate) fn private_file(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    private_options(&mut opts);
    let file = opts
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    make_private(&file, path, 0o600)?;
    Ok(file)
}

/// Create a new file that only the owner can read. The call fails if the path
/// exists, also as a symbolic link, so a planted link cannot redirect the write.
pub(crate) fn new_private_file(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    private_options(&mut opts);
    opts.open(path)
        .with_context(|| format!("create {}", path.display()))
}

/// Owner-only mode for a new file, and no symbolic link at the last path part.
fn private_options(opts: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = opts;
}

/// Remove access for other users from a file or directory of this user. A file
/// of another user is an error, because its owner controls its content.
fn make_private(file: &File, path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file
            .metadata()
            .with_context(|| format!("read {}", path.display()))?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!("{} belongs to another user", path.display());
        }
        if meta.mode() & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(mode))
                .with_context(|| format!("set the permissions of {}", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    let _ = (file, path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(body: &str) -> Draft {
        Draft {
            source: "test".into(),
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

    fn all(store: &Store) -> Vec<Entry> {
        let mut reader = store.reader().unwrap();
        let mut out = vec![];
        while let Some(entry) = reader.next_entry().unwrap() {
            out.push(entry);
        }
        out
    }

    #[test]
    fn empty_feed_has_last_id_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("feed")).unwrap();
        assert_eq!(store.last_id().unwrap(), 0);
        assert!(all(&store).is_empty());
    }

    #[test]
    fn open_creates_an_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path()).unwrap();
        assert_eq!(fs::read(dir.path().join(LOG)).unwrap(), b"");
    }

    #[test]
    fn append_gives_increasing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ids: Vec<u64> = (0..3)
            .map(|i| store.append(draft(&format!("n{i}"))).unwrap().id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(store.last_id().unwrap(), 3);
        let bodies: Vec<String> = all(&store).into_iter().map(|e| e.body).collect();
        assert_eq!(bodies, vec!["n0", "n1", "n2"]);
    }

    #[test]
    fn ids_continue_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path()).unwrap().append(draft("a")).unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.append(draft("b")).unwrap().id, 2);
    }

    #[test]
    fn append_rejects_invalid_draft_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let err = store.append(draft("\x1b[2J")).unwrap_err();
        assert!(err.downcast_ref::<entry::Invalid>().is_some(), "{err:#}");
        assert_eq!(store.last_id().unwrap(), 0);
    }

    #[test]
    fn append_rejects_an_entry_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut d = draft("small body");
        d.extra
            .insert("big".into(), "x".repeat(entry::MAX_ENTRY).into());
        let err = store.append(d).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<entry::Invalid>(),
                Some(entry::Invalid::TooLong { field: "entry", .. })
            ),
            "{err:#}"
        );
        assert_eq!(store.last_id().unwrap(), 0);
    }

    #[test]
    fn reader_continues_after_new_appends() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.append(draft("a")).unwrap();
        let mut reader = store.reader().unwrap();
        assert_eq!(reader.next_entry().unwrap().unwrap().id, 1);
        assert!(reader.next_entry().unwrap().is_none());
        store.append(draft("b")).unwrap();
        assert_eq!(reader.next_entry().unwrap().unwrap().id, 2);
    }

    #[test]
    fn partial_last_line_is_invisible_and_next_append_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.append(draft("a")).unwrap();
        // a crash in the middle of a write
        let mut log = OpenOptions::new()
            .append(true)
            .open(dir.path().join(LOG))
            .unwrap();
        log.write_all(br#"{"id":2,"time":"#).unwrap();
        drop(log);

        assert_eq!(store.last_id().unwrap(), 1);
        assert_eq!(all(&store).len(), 1);

        assert_eq!(store.append(draft("b")).unwrap().id, 2);
        let bodies: Vec<String> = all(&store).into_iter().map(|e| e.body).collect();
        assert_eq!(bodies, vec!["a", "b"]);
    }

    #[test]
    fn parallel_appends_get_unique_contiguous_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // each thread opens its own store, like separate processes
                    let store = Store::open(&path).unwrap();
                    (0..10)
                        .map(|_| store.append(draft("x")).unwrap().id)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut ids: Vec<u64> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        ids.sort();
        assert_eq!(ids, (1..=80).collect::<Vec<_>>());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let feed = dir.path().join("feed");
        let store = Store::open(&feed).unwrap();
        store.append(draft("a")).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&feed), 0o700);
        assert_eq!(mode(&feed.join(LOG)), 0o600);
    }
}
