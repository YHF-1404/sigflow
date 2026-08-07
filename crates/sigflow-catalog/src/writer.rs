//! One-stop committing APIs.
//!
//! They bind two contracts together so callers cannot skip either:
//! the completeness gate (`validate_complete` — a final manifest must carry
//! hashes, channels, a safe artifact list) and the fsync order
//! (artifacts durable → manifest tmp+fsync → rename → dir fsync).
//!
//! Two commit shapes match the two sink modes:
//! - [`commit_complete_record`] — the segment-buffered sink (today's
//!   storage plugin): the whole record is in memory, written in one shot,
//!   no provisional manifest, crash leaves zero residue.
//! - [`promote_validated`] — the streaming sink (future video): a
//!   `record.json.recording` was written at first frame; this validates its
//!   content and commits it via the rename.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Map;

use crate::atomic::{promote_recording, write_atomic, write_durable};
use crate::layout::DataRoot;
use crate::manifest::{Artifact, RecordManifest, RECORD_MANIFEST, STATE_COMPLETE};
use crate::CatalogError;

/// One artifact to commit: the manifest entry's size and blake3 are computed
/// from `bytes`, so they can never disagree with what lands on disk.
pub struct ArtifactPayload<'a> {
    /// Semantic role, e.g. `waveform`.
    pub role: &'a str,
    /// Flat filename inside the record directory.
    pub path: &'a str,
    /// Byte-layout self-description, e.g. `npy/f32-le/C/(n,ch)`.
    pub format: &'a str,
    pub bytes: &'a [u8],
}

/// Commit a whole record in one shot (segment-sink path).
///
/// Fills `manifest.artifacts` (size + blake3 from the payloads) and sets
/// `state = complete`, validates, then writes: every artifact durable first,
/// then the manifest atomically as `record.json` — whose appearance IS the
/// commit point. Validation happens before anything touches the disk.
/// Returns the manifest path.
pub fn commit_complete_record(
    root: &DataRoot,
    manifest: &mut RecordManifest,
    artifacts: &[ArtifactPayload<'_>],
) -> Result<PathBuf, CatalogError> {
    let dir = root.record_dir(&manifest.id)?;
    manifest.state = STATE_COMPLETE.to_string();
    manifest.artifacts = artifacts
        .iter()
        .map(|a| Artifact {
            role: a.role.to_string(),
            path: a.path.to_string(),
            format: a.format.to_string(),
            size_bytes: a.bytes.len() as u64,
            blake3: Some(blake3::hash(a.bytes).to_hex().to_string()),
            extra: Map::new(),
        })
        .collect();
    manifest.validate_complete()?;

    for a in artifacts {
        write_durable(&dir.join(a.path), a.bytes)?;
    }
    let manifest_path = dir.join(RECORD_MANIFEST);
    write_atomic(&manifest_path, &manifest.to_json_vec()?)?;
    Ok(manifest_path)
}

/// Commit a streaming record: read the `record.json.recording`, run the
/// version gate and `validate_complete` on its content, and only then
/// promote it via the rename. An incomplete manifest (state still
/// `recording`, artifacts missing hashes) is refused and left in place.
pub fn promote_validated(recording_path: &Path) -> Result<PathBuf, CatalogError> {
    let bytes = fs::read(recording_path)?;
    let m = RecordManifest::from_slice(&bytes)?;
    m.validate_complete()?;
    Ok(promote_recording(recording_path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt::{check_record_dir, Verdict};
    use crate::atomic::write_atomic;
    use crate::manifest::tests_support::{complete_manifest, npy_bytes};
    use crate::manifest::{RECORDING_SUFFIX, STATE_RECORDING};

    const U: &str = "01JXAB3C4D5E6F7G8H9JKMNPQR";

    #[test]
    fn commit_complete_record_end_to_end() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let npy = npy_bytes(4, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.artifacts.clear(); // commit fills them from the payloads
        m.state = "whatever".into(); // commit forces complete

        let path = commit_complete_record(
            &root,
            &mut m,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &npy,
            }],
        )
        .unwrap();

        assert_eq!(path, root.record_manifest(U).unwrap());
        // The committed record must scan as Complete (hash verified).
        let out = check_record_dir(path.parent().unwrap()).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Complete);
        assert_eq!(out.manifest.unwrap().artifacts[0].size_bytes, npy.len() as u64);
    }

    #[test]
    fn commit_validates_before_touching_disk() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let npy = npy_bytes(4, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.intrinsic.channels.clear(); // invalid: no channel descriptions
        let err = commit_complete_record(
            &root,
            &mut m,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &npy,
            }],
        )
        .unwrap_err();
        assert!(matches!(err, CatalogError::Invalid(_)), "{err}");
        // Nothing was written.
        assert!(!root.record_dir(U).unwrap().exists());
    }

    #[test]
    fn promote_validated_gates_on_completeness() {
        let t = tempfile::tempdir().unwrap();
        let npy = npy_bytes(4, 2);
        let rec_path = t.path().join(format!("record.json{RECORDING_SUFFIX}"));

        // An incomplete manifest (recording state, no hash) must be refused
        // and left in place.
        let mut m = complete_manifest("waveform.npy", &npy);
        m.state = STATE_RECORDING.into();
        m.artifacts[0].blake3 = None;
        write_atomic(&rec_path, &m.to_json_vec().unwrap()).unwrap();
        assert!(promote_validated(&rec_path).is_err());
        assert!(rec_path.exists(), "refused manifest must stay in place");

        // A complete one passes and the rename commits it.
        let m = complete_manifest("waveform.npy", &npy);
        write_atomic(&rec_path, &m.to_json_vec().unwrap()).unwrap();
        let final_path = promote_validated(&rec_path).unwrap();
        assert_eq!(final_path, t.path().join("record.json"));
        assert!(!rec_path.exists());
    }
}
