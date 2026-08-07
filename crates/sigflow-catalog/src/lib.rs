//! sigflow-catalog: the data-storage-manager foundation.
//!
//! Truth lives in files that travel with the data; any index is a rebuildable
//! cache. This crate owns the on-disk contract:
//!
//!   - [`manifest`] — `record.json` v1 types (+ the session manifest, which is
//!     a "record without artifacts"), with unknown-field preservation and a
//!     `format_version` gate.
//!   - [`id`] — ULID generation: monotonic per process, plus an
//!     explicit-timestamp variant for migrating legacy captures.
//!   - [`layout`] — the data-root directory layout
//!     (`records/<shard>/<ulid>/`, `annotations/{records,sessions}/`,
//!     `_session/`, `_sync/`, `_index/`, `_journal/`) and the canonical rsync
//!     exclude list.
//!   - [`atomic`] — fsync-ordered durable writes: write artifact → fsync(file)
//!     → manifest tmp → fsync → rename → fsync(dir). The manifest rename *is*
//!     the commit point; `record.json.recording` → `record.json` is the state
//!     transition.
//!   - [`adopt`] — the reindex adoption table as a pure function over scanned
//!     directory facts, plus the IO scanner that produces those facts
//!     (blake3 verification for final manifests, npy self-validation for
//!     recording residue).

pub mod adopt;
pub mod annot;
pub mod atomic;
pub mod gc;
pub mod id;
#[cfg(feature = "index")]
pub mod index;
pub mod journal;
pub mod layout;
pub mod manifest;
pub mod resolve;
pub mod session;
pub mod sync;
pub mod writer;

pub use adopt::{adjudicate, check_record_dir, Adoption, ArtifactStatus, DirFacts, Verdict};
pub use annot::{
    annotate, read_annotation_file, AnnotEntity, AnnotMutation, AnnotValue, AnnotWriteOutcome,
    AnnotationFile,
};
#[cfg(feature = "index")]
pub use index::{Filter, Index, QueryOpts, RecordRow, ReindexOptions, ReindexReport};
pub use resolve::{
    effective_session_id, resolve_record_attrs, resolve_session_attrs, Layer, ResolvedAttr,
};
pub use atomic::{
    create_dir_all_durable, fsync_dir, promote_recording, write_atomic, write_durable,
};
pub use gc::{apply_gc, plan_gc, GcCandidate, GcOutcome, GcPlan, DEFAULT_RETENTION};
pub use id::{is_valid_ulid, ulid_shard, UlidGen};
pub use layout::DataRoot;
pub use sync::{read_sync_ack, write_sync_ack, SyncAck};
pub use session::{
    begin_session, end_session, read_active_session, SessionPointer, SessionReading,
    MAX_POINTER_AGE,
};
pub use manifest::{
    AnnotationEntry, Artifact, ChannelDesc, ChannelRole, ClockBase, FsSource, Intrinsic,
    Provenance, RecordManifest, RecordState, SessionManifest,
};
pub use writer::{commit_complete_record, promote_validated, ArtifactPayload};

use std::fmt;

/// Errors for catalog operations.
#[derive(Debug)]
pub enum CatalogError {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// `format_version` has an unknown major (or does not parse at all):
    /// refuse to read rather than misinterpret — and never destroy.
    Version { found: String },
    InvalidUlid(String),
    /// Semantic validation failure (e.g. a `complete` manifest with no
    /// artifacts). The string lists every problem found.
    Invalid(String),
    /// Optimistic-concurrency failure in the annotation writer: the file's
    /// `rev` is not what the caller staged its edit against (someone else
    /// wrote in between). Re-read and retry.
    Conflict { expected: u64, found: u64 },
    /// SQLite error from the index cache (feature `index`).
    #[cfg(feature = "index")]
    Sqlite(rusqlite::Error),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Io(e) => write!(f, "io error: {e}"),
            CatalogError::Json(e) => write!(f, "json error: {e}"),
            CatalogError::Version { found } => {
                write!(f, "unsupported format_version '{found}' (this tool reads major 1)")
            }
            CatalogError::InvalidUlid(s) => write!(f, "invalid ULID '{s}'"),
            CatalogError::Invalid(s) => write!(f, "invalid manifest: {s}"),
            CatalogError::Conflict { expected, found } => write!(
                f,
                "annotation rev conflict: expected rev {expected}, file is at rev {found} (someone else wrote — re-read and retry)"
            ),
            #[cfg(feature = "index")]
            CatalogError::Sqlite(e) => write!(f, "index error: {e}"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<std::io::Error> for CatalogError {
    fn from(e: std::io::Error) -> Self {
        CatalogError::Io(e)
    }
}

impl From<serde_json::Error> for CatalogError {
    fn from(e: serde_json::Error) -> Self {
        CatalogError::Json(e)
    }
}

#[cfg(feature = "index")]
impl From<rusqlite::Error> for CatalogError {
    fn from(e: rusqlite::Error) -> Self {
        CatalogError::Sqlite(e)
    }
}

/// Current UTC wall-clock time as an RFC 3339 string with millisecond
/// precision — the `provenance.created_utc` anchor (frame `t0_ns` is
/// CLOCK_MONOTONIC and cannot locate a capture in absolute time).
pub fn utc_now_rfc3339() -> String {
    humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string()
}

/// RFC 3339 string for a unix-epoch millisecond timestamp (used by the
/// legacy-capture migration, which derives time from file mtime).
pub fn utc_ms_rfc3339(unix_ms: u64) -> String {
    let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(unix_ms);
    humantime::format_rfc3339_millis(t).to_string()
}

/// Best-effort hostname for provenance (sink and CLI both stamp it).
/// Cheap, fork-free sources first; the subprocess is the last resort
/// (HOSTNAME is often unset in non-interactive shells, /etc/hostname is the
/// reliable answer on the archlinux capture machine, the binary covers macOS).
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_ms_formats_rfc3339_with_millis() {
        assert_eq!(super::utc_ms_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::utc_ms_rfc3339(1_750_000_000_123), "2025-06-15T15:06:40.123Z");
    }
}
