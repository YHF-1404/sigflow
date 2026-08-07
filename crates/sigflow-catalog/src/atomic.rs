//! Fsync-ordered durable writes.
//!
//! The contract (red-team ratified): the *only* fatal omission is skipping
//! fsync on the artifact — ext4 delayed allocation can leave a size-correct,
//! zero-page file after a crash, and a committed manifest would then
//! vouch for garbage. Hence the full order for one record:
//!
//! ```text
//! write artifact → fsync(artifact) → write manifest.tmp → fsync(tmp)
//!   → rename(tmp → final) → fsync(parent dir)
//! ```
//!
//! Directory fsyncs are best-effort: a lost directory entry merely
//! downgrades `complete` to whatever reindex can salvage (acceptable),
//! while failing a write *after* its rename already happened would
//! misreport a committed record.
//!
//! These helpers are synchronous; the async writer (per-record FIFO, single
//! writer thread) sequences calls but the durability order lives here.
//! Prefer the one-stop committing APIs in [`crate::writer`], which bind this
//! order and the completeness gate together.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::manifest::{RECORDING_SUFFIX, TMP_SUFFIX};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` and fsync the file (artifact write). Creates
/// missing parent directories durably (see [`create_dir_all_durable`]).
pub fn write_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all_durable(parent)?;
    }
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Atomically replace `path` with `bytes`: durable write to a unique
/// `<path>.<pid>.<seq>.tmp` sibling, rename over `path`, fsync the parent
/// directory. Crash leaves either the old file, the new file, or a `.tmp`
/// residue (deleted by reindex) — never a half-written `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    write_durable(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent);
    }
    Ok(())
}

/// Commit a record: rename `record.json.recording` → `record.json` and fsync
/// the directory. The rename IS the state transition; returns the final path.
///
/// This is the raw primitive (also used by salvage rewrites): it does not
/// look at the content. Committing a *record* must go through
/// [`crate::writer::promote_validated`], which runs `validate_complete`
/// first — a manifest missing its blake3 hashes must never become final.
pub fn promote_recording(recording_path: &Path) -> io::Result<PathBuf> {
    let name = recording_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let stem = name.strip_suffix(RECORDING_SUFFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{name}' does not end with {RECORDING_SUFFIX}"),
        )
    })?;
    let final_path = recording_path.with_file_name(stem);
    fs::rename(recording_path, &final_path)?;
    if let Some(parent) = final_path.parent() {
        fsync_dir(parent);
    }
    Ok(final_path)
}

/// Best-effort directory fsync so a rename within it survives power loss.
/// All failures are deliberately swallowed (unsupported filesystems, EACCES,
/// even EIO): by the time this runs the rename has already happened, and the
/// ratified contract classifies a lost directory entry as a
/// reindex-recoverable downgrade — erroring here would misreport a
/// committed write as failed.
pub fn fsync_dir(dir: &Path) {
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

/// `create_dir_all` plus a best-effort fsync of every newly created level
/// and of the deepest pre-existing ancestor (whose entry changed). Without
/// this, a freshly created `records/<shard>/<ulid>/` chain can evaporate in
/// a crash even though the files inside were fsynced — losing the whole
/// record after success was reported.
pub fn create_dir_all_durable(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.exists() {
        return Ok(());
    }
    let mut created = Vec::new();
    let mut cur = dir.to_path_buf();
    while !cur.exists() {
        created.push(cur.clone());
        match cur.parent() {
            Some(p) if !p.as_os_str().is_empty() => cur = p.to_path_buf(),
            _ => break,
        }
    }
    fs::create_dir_all(dir)?;
    for d in &created {
        fsync_dir(d);
    }
    fsync_dir(&cur);
    Ok(())
}

/// Unique `<name>.<pid>.<seq>.tmp` sibling used by [`write_atomic`]. Unique
/// so that two writers racing on the same path can never splice each
/// other's half-written temporary into place (single-writer per record is
/// still the contract; this is cheap insurance). Reindex deletes any
/// `*.tmp` residue regardless of the infix.
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed),
        TMP_SUFFIX
    ));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("record.json");
        write_atomic(&p, b"v1").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"v1");
        write_atomic(&p, b"v2-longer").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"v2-longer");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(TMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn write_durable_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("records/01/X/waveform.npy");
        write_durable(&p, b"data").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"data");
    }

    #[test]
    fn promote_renames_recording_to_final() {
        let dir = tempfile::tempdir().unwrap();
        let rec = dir.path().join("record.json.recording");
        write_atomic(&rec, b"{}").unwrap();
        let final_path = promote_recording(&rec).unwrap();
        assert_eq!(final_path, dir.path().join("record.json"));
        assert!(!rec.exists());
        assert_eq!(fs::read(final_path).unwrap(), b"{}");
    }

    #[test]
    fn promote_refuses_non_recording_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("record.json");
        write_atomic(&p, b"{}").unwrap();
        assert!(promote_recording(&p).is_err());
    }

    #[test]
    fn tmp_path_is_unique_and_keeps_the_tmp_suffix() {
        let a = tmp_path(Path::new("/a/record.json"));
        let b = tmp_path(Path::new("/a/record.json"));
        assert_ne!(a, b, "concurrent writers must not share a tmp name");
        for p in [&a, &b] {
            let n = p.file_name().unwrap().to_str().unwrap();
            assert!(n.starts_with("record.json."), "{n}");
            assert!(n.ends_with(TMP_SUFFIX), "{n}");
        }
    }
}
