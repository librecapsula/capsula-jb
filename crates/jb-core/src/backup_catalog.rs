//! Backup catalog v2: the unified completed-backup manifest
//! (`backup/<key>/backup.json`) shared by both platforms, its atomic
//! publication, verification and lookup.
//!
//! Old (pre-v2) backup directories are NOT read and NOT migrated — a
//! capture is simpler to re-take than to trust: the catalog ignores
//! them, the platform re-dumps into the same directory and publishes
//! v2 over it.

use crate::backup_store::{BackupStore, StoreError, file_sha256};
use crate::session::Model;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The manifest file name inside a backup directory.
pub const MANIFEST: &str = "backup.json";

pub const FORMAT: u32 = 2;

/// The overall capture state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Everything captured intact.
    Complete,
    /// Captured with caveats (the first-generation 0xCC mask, amlogic
    /// bad blocks) — not restorable, noted in `notes`.
    Partial,
}

/// One partition image of the capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    /// The partition as the platform calls it (userdata / data / ...).
    pub partition: String,
    /// File name inside the backup directory.
    pub file: String,
    pub sha256: String,
    pub size: u64,
    /// The image content is fully intact (false = the platform knows
    /// part of it is garbage/short: masked or shortened partitions).
    pub complete: bool,
}

/// A non-image file of the capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub file: String,
    pub sha256: String,
    pub size: u64,
    /// What the file is: gpt, partition-table, serial, env, dtb,
    /// reserved, nand-raw, ...
    pub role: String,
}

/// The unified completed-backup manifest, `backup/<key>/backup.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format: u32,
    pub model: Model,
    /// The column identity key the directory is named after (the
    /// loader serial on rockchip, the deviceid on amlogic).
    pub key: String,
    pub created_utc: String,
    pub tool_version: String,
    pub status: Status,
    /// Human caveats for partial captures (shown by UIs).
    pub notes: Vec<String>,
    pub images: Vec<ImageEntry>,
    pub extras: Vec<FileEntry>,
}

/// A parsed backup directory.
#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub dir: PathBuf,
    pub manifest: BackupManifest,
}

impl BackupEntry {
    fn from_manifest(dir: PathBuf, manifest: BackupManifest) -> Self {
        BackupEntry { dir, manifest }
    }

    /// Verify every file against the manifest (streamed SHA256). This
    /// is the gate before a capture becomes the base of the only copy
    /// or gets restored.
    pub fn verify(&self) -> Result<(), StoreError> {
        for img in &self.manifest.images {
            check(&self.dir, &img.file, &img.sha256, img.size)?;
        }
        for x in &self.manifest.extras {
            check(&self.dir, &x.file, &x.sha256, x.size)?;
        }
        Ok(())
    }

    /// The image file of a partition, with its recorded size.
    pub fn image(&self, partition: &str) -> Option<(PathBuf, u64)> {
        self.manifest
            .images
            .iter()
            .find(|i| i.partition == partition)
            .map(|i| (self.dir.join(&i.file), i.size))
    }

    /// Total bytes of all recorded files (what a UI shows as the
    /// backup size).
    pub fn total_bytes(&self) -> u64 {
        self.manifest
            .images
            .iter()
            .map(|i| i.size)
            .chain(self.manifest.extras.iter().map(|x| x.size))
            .sum()
    }

    /// Delete the whole backup directory.
    pub fn delete(self) -> std::io::Result<()> {
        std::fs::remove_dir_all(self.dir)
    }
}

fn check(dir: &Path, file: &str, sha256: &str, size: u64) -> Result<(), StoreError> {
    let p = dir.join(file);
    let got = file_sha256(&p)?;
    if got != sha256.to_lowercase() {
        return Err(StoreError::Checksum(format!("SHA256 не сошёлся: {file}")));
    }
    let len = std::fs::metadata(&p).map_err(StoreError::Io)?.len();
    if len != size {
        return Err(StoreError::Checksum(format!(
            "размер не сошёлся: {file} ({len} != {size})"
        )));
    }
    Ok(())
}

/// Publish a manifest as `backup.json` atomically (tmp + rename) and
/// return the entry. This is the LAST step of a capture: until it
/// succeeds, the directory is not a v2 backup.
pub fn publish(dir: &Path, manifest: BackupManifest) -> Result<BackupEntry, StoreError> {
    if manifest.format != FORMAT {
        return Err(StoreError::Bad {
            path: dir.to_path_buf(),
            reason: format!("не тот формат: {}", manifest.format),
        });
    }
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| StoreError::Bad {
        path: dir.to_path_buf(),
        reason: format!("сериализация: {e}"),
    })?;
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, dir.join(MANIFEST))?;
    Ok(BackupEntry::from_manifest(dir.to_path_buf(), manifest))
}

/// Read a v2 backup directory. None = no (readable) `backup.json` —
/// in particular, every pre-v2 layout counts as "no backup".
pub fn read(dir: &Path) -> Option<BackupEntry> {
    let raw = std::fs::read(dir.join(MANIFEST)).ok()?;
    let manifest: BackupManifest = serde_json::from_slice(&raw).ok()?;
    (manifest.format == FORMAT).then(|| BackupEntry::from_manifest(dir.to_path_buf(), manifest))
}

/// The v2 entry of one column's backup directory, or None when the
/// column has no completed v2 backup. The layout is per-model
/// (`backup/<model>/<key>/`): identity keys of different platforms may
/// collide.
pub fn open(root: &Path, model: Model, key: &str) -> Option<BackupEntry> {
    read(&root.join(model.dir_name()).join(BackupStore::dir_name(key)))
}

/// All v2 backups under a root directory (`backup/`), oldest first.
pub fn scan(root: &Path) -> Vec<BackupEntry> {
    let mut out = Vec::new();
    for model in [Model::Capsula, Model::CapsulaMiniAmlogic] {
        let Ok(rd) = std::fs::read_dir(root.join(model.dir_name())) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir()
                && let Some(entry) = read(&p)
            {
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| a.manifest.created_utc.cmp(&b.manifest.created_utc));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jb-catalog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn manifest(key: &str, images: Vec<ImageEntry>) -> BackupManifest {
        BackupManifest {
            format: FORMAT,
            model: Model::Capsula,
            key: key.into(),
            created_utc: "2026-11-10T00:00:00Z".into(),
            tool_version: "test".into(),
            status: Status::Complete,
            notes: vec![],
            images,
            extras: vec![],
        }
    }

    #[test]
    fn publish_read_verify_roundtrip() {
        let dir = tmp("rt");
        std::fs::write(dir.join("userdata.img"), b"hello").unwrap();
        let m = manifest(
            "SN1",
            vec![ImageEntry {
                partition: "userdata".into(),
                file: "userdata.img".into(),
                sha256: file_sha256(&dir.join("userdata.img")).unwrap(),
                size: 5,
                complete: true,
            }],
        );
        let entry = publish(&dir, m).unwrap();
        assert_eq!(entry.total_bytes(), 5);

        let back = read(&dir).unwrap();
        assert_eq!(back.manifest.key, "SN1");
        assert_eq!(back.image("userdata").unwrap().1, 5);
        assert!(back.verify().is_ok());

        // a corrupted file fails verification
        std::fs::write(dir.join("userdata.img"), b"HELLO").unwrap();
        assert!(back.verify().is_err());
    }

    #[test]
    fn verify_catches_missing_file_and_wrong_size() {
        let dir = tmp("miss");
        std::fs::write(dir.join("a.img"), b"12345").unwrap();
        let m = manifest(
            "SN2",
            vec![
                ImageEntry {
                    partition: "a".into(),
                    file: "a.img".into(),
                    sha256: file_sha256(&dir.join("a.img")).unwrap(),
                    size: 4, // wrong on purpose
                    complete: true,
                },
                ImageEntry {
                    partition: "b".into(),
                    file: "b.img".into(),
                    sha256: "00".repeat(32),
                    size: 1,
                    complete: true,
                },
            ],
        );
        let entry = publish(&dir, m).unwrap();
        assert!(entry.verify().is_err()); // wrong size on a, missing b
    }

    #[test]
    fn pre_v2_directories_are_ignored() {
        let root = tmp("legacy");
        let dir = root.join("capsule").join("OLDSERIAL");
        std::fs::create_dir_all(&dir).unwrap();
        // a complete old-layout backup — not readable, not migrated
        std::fs::write(dir.join("00_uboot.img"), b"uboot!").unwrap();
        std::fs::write(
            dir.join("SHA256SUMS.txt"),
            format!(
                "{}  00_uboot.img\n",
                file_sha256(&dir.join("00_uboot.img")).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(dir.join("SUCCESS.txt"), "SUCCESS\n").unwrap();
        assert!(read(&dir).is_none());
        assert!(open(&root, Model::Capsula, "OLDSERIAL").is_none());
        assert!(scan(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_lists_entries_and_open_finds_by_model_and_key() {
        let root = tmp("scan");
        // same key under two models — no collision
        for (model, file) in [(Model::Capsula, "a.img"), (Model::CapsulaMiniAmlogic, "a.bin")] {
            let dir = root.join(model.dir_name()).join("SAMEKEY");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), b"x").unwrap();
            let mut m = manifest("SAMEKEY", vec![]);
            m.model = model;
            m.images = vec![ImageEntry {
                partition: "data".into(),
                file: file.into(),
                sha256: file_sha256(&dir.join(file)).unwrap(),
                size: 1,
                complete: true,
            }];
            publish(&dir, m).unwrap();
        }
        assert_eq!(scan(&root).len(), 2);
        assert!(open(&root, Model::Capsula, "SAMEKEY").is_some());
        assert!(open(&root, Model::CapsulaMiniAmlogic, "SAMEKEY").is_some());
        assert!(open(&root, Model::Capsula, "OTHER").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
