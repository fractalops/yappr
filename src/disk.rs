//! Private files for persistent application state.
use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

use crate::store;

pub fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("read {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Replace a file after the new contents reach the disk.
pub fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    write_bytes(path, &serde_json::to_vec(value)?)
}

/// Replace a private file with these bytes after they reach the disk.
pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    store::create_private_dir(dir)?;
    let temp = dir.join(format!(".{}.tmp", random_id()));
    let result = (|| {
        let mut file = store::private_file(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        anyhow::Ok(())
    })();
    if result.is_err() {
        fs::remove_file(temp).ok();
    }
    result.with_context(|| format!("write {}", path.display()))
}

pub fn random_id() -> String {
    data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes())
}

pub fn lock(dir: &Path, name: &str) -> Result<fs::File> {
    store::create_private_dir(dir)?;
    let file = store::private_file(&dir.join(name))?;
    file.lock()?;
    Ok(file)
}
