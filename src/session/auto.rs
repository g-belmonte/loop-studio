//! Per-track session auto-save.
//!
//! Sessions are written under `$XDG_DATA_HOME/loop-studio/autosessions/`
//! (or `~/.local/share/loop-studio/autosessions/`) and keyed by a stable
//! FNV-1a hash of the canonical track path. Hashing inline (rather than
//! using `std::hash::DefaultHasher`) keeps filenames stable across Rust
//! versions so an auto-saved session survives a toolchain upgrade.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::schema::Session;

pub fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("loop-studio").join("autosessions"))
}

pub fn path_for(track_path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(track_path).unwrap_or_else(|_| track_path.to_path_buf());
    let key = canonical.to_string_lossy();
    let hash = fnv1a_64(key.as_bytes());
    Some(dir()?.join(format!("{hash:016x}.json")))
}

pub fn save_for(track_path: &Path, session: &Session) -> Result<()> {
    let path = path_for(track_path).context("no data directory (HOME / XDG_DATA_HOME unset)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating autosession dir {}", parent.display()))?;
    }
    session.save(&path)
}

pub fn load_for(track_path: &Path) -> Option<Session> {
    let path = path_for(track_path)?;
    if !path.exists() {
        return None;
    }
    match Session::load(&path) {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!(
                "ignoring autosession at {}: {e:#}",
                path.display()
            );
            None
        }
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
