//! Data-root directory layout — the single source of truth for every path
//! the storage manager touches, plus the canonical sync exclude list.
//!
//! ```text
//! <data-root>/
//! ├── records/<shard>/<ulid>/        capture machine writes; immutable once
//! │   ├── record.json                  complete (session dirs hold
//! │   └── waveform.npy                  session.json instead)
//! ├── annotations/records/<ulid>.json   analysis machine writes; mutable truth
//! ├── annotations/sessions/<ulid>.json
//! ├── _session/current.json          session pointer (capture machine only)
//! ├── _sync/synced.json              verified-ULID ack (mac writes, arch reads)
//! ├── _index/                        rebuildable SQLite cache — never synced
//! └── _journal/                      best-effort audit log — droppable
//! ```
//!
//! Underscore-prefixed directories are machine-local by convention and are
//! never pulled; `annotations/` belongs to the analysis machine and must be
//! excluded from capture→analysis pulls (`--delete` would wipe it).

use std::path::{Path, PathBuf};

use crate::id::{is_valid_ulid, ulid_shard};
use crate::manifest::{RECORD_MANIFEST, RECORDING_SUFFIX, SESSION_MANIFEST};
use crate::CatalogError;

pub const RECORDS_DIR: &str = "records";
pub const ANNOTATIONS_DIR: &str = "annotations";
pub const ANNOTATIONS_RECORDS_DIR: &str = "records";
pub const ANNOTATIONS_SESSIONS_DIR: &str = "sessions";
pub const SESSION_DIR: &str = "_session";
pub const SESSION_POINTER: &str = "current.json";
pub const SYNC_DIR: &str = "_sync";
pub const SYNC_ACK: &str = "synced.json";
pub const INDEX_DIR: &str = "_index";
pub const JOURNAL_DIR: &str = "_journal";
pub const ANNOTATION_JOURNAL: &str = "annotations.jsonl";

/// Canonical exclude patterns for the capture→analysis rsync pull. Scripted,
/// not remembered: S8's sync script must take this list from here.
/// `--delete` on the receiving side is only safe because of these.
pub const RSYNC_PULL_EXCLUDES: &[&str] = &[
    "annotations/",
    "_session/",
    "_sync/",
    "_index/",
    "_journal/",
    "*.recording", // half-done manifests never reach the analysis machine
    "*.tmp",
];

/// Root of one data store; all paths derive from here.
#[derive(Debug, Clone)]
pub struct DataRoot(PathBuf);

impl DataRoot {
    pub fn new(p: impl Into<PathBuf>) -> Self {
        DataRoot(p.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn records_dir(&self) -> PathBuf {
        self.0.join(RECORDS_DIR)
    }

    /// `records/<shard>/<ulid>/` — validates the ULID (a malformed id must
    /// never become a path component).
    pub fn record_dir(&self, ulid: &str) -> Result<PathBuf, CatalogError> {
        if !is_valid_ulid(ulid) {
            return Err(CatalogError::InvalidUlid(ulid.to_string()));
        }
        Ok(self.records_dir().join(ulid_shard(ulid)).join(ulid))
    }

    /// Final record manifest path (the commit point).
    pub fn record_manifest(&self, ulid: &str) -> Result<PathBuf, CatalogError> {
        Ok(self.record_dir(ulid)?.join(RECORD_MANIFEST))
    }

    /// In-progress manifest path (`record.json.recording`).
    pub fn record_manifest_recording(&self, ulid: &str) -> Result<PathBuf, CatalogError> {
        Ok(self
            .record_dir(ulid)?
            .join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")))
    }

    /// Session manifest path — a session is a record without artifacts,
    /// stored in the same sharded tree as `session.json`.
    pub fn session_manifest(&self, ulid: &str) -> Result<PathBuf, CatalogError> {
        Ok(self.record_dir(ulid)?.join(SESSION_MANIFEST))
    }

    /// Mutable human-annotation file for a record (analysis machine writes).
    pub fn record_annotation(&self, ulid: &str) -> Result<PathBuf, CatalogError> {
        if !is_valid_ulid(ulid) {
            return Err(CatalogError::InvalidUlid(ulid.to_string()));
        }
        Ok(self
            .0
            .join(ANNOTATIONS_DIR)
            .join(ANNOTATIONS_RECORDS_DIR)
            .join(format!("{ulid}.json")))
    }

    /// Mutable human-annotation file for a session.
    pub fn session_annotation(&self, ulid: &str) -> Result<PathBuf, CatalogError> {
        if !is_valid_ulid(ulid) {
            return Err(CatalogError::InvalidUlid(ulid.to_string()));
        }
        Ok(self
            .0
            .join(ANNOTATIONS_DIR)
            .join(ANNOTATIONS_SESSIONS_DIR)
            .join(format!("{ulid}.json")))
    }

    /// `_session/current.json` — the active-session pointer. Read per record
    /// flush (µs-scale), never cached, so a stale value has no window.
    pub fn session_pointer(&self) -> PathBuf {
        self.0.join(SESSION_DIR).join(SESSION_POINTER)
    }

    /// `_sync/synced.json` — verified-ULID ack written by the analysis
    /// machine, read by the capture machine's GC (the one designed backflow).
    pub fn sync_ack(&self) -> PathBuf {
        self.0.join(SYNC_DIR).join(SYNC_ACK)
    }

    pub fn index_dir(&self) -> PathBuf {
        self.0.join(INDEX_DIR)
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.0.join(JOURNAL_DIR)
    }

    /// `annotations/` — the mutable-truth tree, and the writer-library's
    /// flock target. flock's exclusion domain is the open file's *inode*; a
    /// dedicated lock *file* could be unlinked and recreated out from under
    /// two writers (a tmp-reaper on the data root, say), after which each
    /// flocks a different inode and neither excludes the other — a silently
    /// lost write on the one irreplaceable layer. The directory's inode is
    /// as durable as the data it guards: deleting it to break the lock would
    /// delete every annotation too, so the race buys nothing.
    pub fn annotations_dir(&self) -> PathBuf {
        self.0.join(ANNOTATIONS_DIR)
    }

    /// `_journal/annotations.jsonl` — best-effort audit log of annotation
    /// writes (droppable by contract, size-rotated).
    pub fn annotation_journal(&self) -> PathBuf {
        self.journal_dir().join(ANNOTATION_JOURNAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "01JXAB3C4D5E6F7G8H9JKMNPQR";

    #[test]
    fn paths_have_the_contracted_shape() {
        let r = DataRoot::new("/data");
        assert_eq!(
            r.record_dir(U).unwrap(),
            PathBuf::from("/data/records/01/01JXAB3C4D5E6F7G8H9JKMNPQR")
        );
        assert!(r.record_manifest(U).unwrap().ends_with("record.json"));
        assert!(r
            .record_manifest_recording(U)
            .unwrap()
            .ends_with("record.json.recording"));
        assert!(r.session_manifest(U).unwrap().ends_with("session.json"));
        assert_eq!(
            r.record_annotation(U).unwrap(),
            PathBuf::from(format!("/data/annotations/records/{U}.json"))
        );
        assert_eq!(
            r.session_annotation(U).unwrap(),
            PathBuf::from(format!("/data/annotations/sessions/{U}.json"))
        );
        assert_eq!(r.session_pointer(), PathBuf::from("/data/_session/current.json"));
        assert_eq!(r.sync_ack(), PathBuf::from("/data/_sync/synced.json"));
    }

    #[test]
    fn malformed_ulid_never_becomes_a_path() {
        let r = DataRoot::new("/data");
        for bad in ["", "../../etc", "01jxab3c4d5e6f7g8h9jkmnpqr"] {
            assert!(r.record_dir(bad).is_err(), "{bad}");
            assert!(r.record_annotation(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn exclude_list_covers_every_machine_local_tree() {
        for needle in ["annotations/", "_session/", "_sync/", "_index/", "_journal/", "*.recording", "*.tmp"]
        {
            assert!(RSYNC_PULL_EXCLUDES.contains(&needle), "{needle}");
        }
    }
}
