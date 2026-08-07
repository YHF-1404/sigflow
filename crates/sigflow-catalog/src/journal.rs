//! `_journal/annotations.jsonl` — best-effort audit log of annotation writes.
//!
//! Ratified contract: the journal is droppable. A "semi-precious" log — one
//! that sometimes gates writes and sometimes vanishes — is the worst middle
//! ground, so this module commits fully to the cheap end: a failed append
//! never fails the write it describes (errors collapse into a warning string
//! the caller surfaces), nothing here is fsynced, and rotation may discard
//! old generations. Cross-session undo is *not* promised on top of this;
//! dry-run previews are the prevention mechanism.
//!
//! One JSONL line per mutation: `{ts, writer, verb, target, key, value?, rev}`,
//! where `rev` is the annotation-file revision the batch committed as (all
//! lines of one batch share it). The annotation writer appends while still
//! holding the writer flock, so lines are rev-ordered.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::layout::DataRoot;

/// Rotate when the current journal reaches this size; one previous
/// generation is kept as `annotations.jsonl.1`. ~150 B/line → ≈28k entries
/// per file — audit at human scale, not telemetry.
pub const JOURNAL_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// One journal line (one mutation of an annotation batch).
#[derive(Debug, Serialize)]
pub struct JournalEntry<'a> {
    /// RFC 3339 UTC, same instant as the annotation entry's `ts`.
    pub ts: &'a str,
    pub writer: &'a str,
    /// `set` | `unset`.
    pub verb: &'a str,
    /// `records/<ulid>` | `sessions/<ulid>`.
    pub target: &'a str,
    pub key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<&'a serde_json::Value>,
    /// Annotation-file revision the batch committed as.
    pub rev: u64,
}

/// Append entries to the data root's annotation journal. Every failure
/// collapses into `Some(warning)` — by contract the caller's write already
/// committed and must be reported as such.
pub fn append(root: &DataRoot, entries: &[JournalEntry]) -> Option<String> {
    append_at(&root.annotation_journal(), entries, JOURNAL_MAX_BYTES)
}

pub(crate) fn append_at(
    path: &Path,
    entries: &[JournalEntry],
    max_bytes: u64,
) -> Option<String> {
    match try_append(path, entries, max_bytes) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "journal append failed (audit only — the annotation write itself committed): {e}"
        )),
    }
}

fn try_append(path: &Path, entries: &[JournalEntry], max_bytes: u64) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() >= max_bytes {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            let _ = std::fs::rename(path, PathBuf::from(rotated));
        }
    }
    // One buffered write so a batch lands as one append (O_APPEND keeps
    // concurrent appenders from interleaving bytes; the writer flock already
    // serializes annotation batches anyway).
    let mut buf = Vec::new();
    for e in entries {
        serde_json::to_writer(&mut buf, e).map_err(std::io::Error::other)?;
        buf.push(b'\n');
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&buf)
    // Deliberately no fsync: droppable by contract.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(key: &'a str, rev: u64, value: Option<&'a serde_json::Value>) -> JournalEntry<'a> {
        JournalEntry {
            ts: "2026-06-12T05:00:00.000Z",
            writer: "test@host",
            verb: if value.is_some() { "set" } else { "unset" },
            target: "records/01JXAB3C4D5E6F7G8H9JKMNPQR",
            key,
            value,
            rev,
        }
    }

    #[test]
    fn appends_parseable_jsonl_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("_journal/annotations.jsonl");
        let v = serde_json::json!("good");
        assert!(append_at(&p, &[entry("grade", 1, Some(&v))], JOURNAL_MAX_BYTES).is_none());
        assert!(append_at(&p, &[entry("grade", 2, None)], JOURNAL_MAX_BYTES).is_none());
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["verb"], "set");
        assert_eq!(lines[0]["value"], "good");
        assert_eq!(lines[0]["rev"], 1);
        assert_eq!(lines[1]["verb"], "unset");
        assert!(lines[1].get("value").is_none(), "unset carries no value field");
        assert_eq!(lines[1]["rev"], 2);
    }

    #[test]
    fn rotates_at_size_keeping_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("annotations.jsonl");
        let v = serde_json::json!("x");
        assert!(append_at(&p, &[entry("a", 1, Some(&v))], 8).is_none());
        // First file now exceeds 8 bytes → next append rotates it away.
        assert!(append_at(&p, &[entry("b", 2, Some(&v))], 8).is_none());
        let rotated = dir.path().join("annotations.jsonl.1");
        assert!(rotated.exists());
        assert!(std::fs::read_to_string(&rotated).unwrap().contains("\"a\""));
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"b\""));
    }

    #[test]
    fn failure_is_a_warning_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // Parent path occupied by a FILE → create_dir_all fails.
        let blocker = dir.path().join("_journal");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let p = blocker.join("annotations.jsonl");
        let v = serde_json::json!(1);
        let warn = append_at(&p, &[entry("a", 1, Some(&v))], JOURNAL_MAX_BYTES);
        assert!(warn.is_some());
        assert!(warn.unwrap().contains("committed"), "must say the write itself survived");
    }
}
