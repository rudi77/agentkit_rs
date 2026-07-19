use std::path::{Path, PathBuf};

use super::{content_key, is_blob_key, BlobInfo, BlobStore};
use crate::domain::BlobRef;
use crate::error::CtxmanError;

/// Content-adressierter Filesystem-Store (Port von `FileSystemBlobStore.cs`, ohne
/// Tenant-Präfix): Key = sha256 (lowercase hex) des Inhalts, Ablage unter `{root}/{key}`.
/// Nur 64-stellige lowercase-hex-Keys sind adressierbar — schützt gegen Path-Traversal über
/// einen manipulierten Key (Spec §10). Dedup: gleicher Inhalt ⇒ gleicher Key ⇒ kein erneutes
/// Schreiben (Spec §7.1, immutable).
pub struct FileSystemBlobStore {
    root: PathBuf,
}

impl FileSystemBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FileSystemBlobStore { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

impl BlobStore for FileSystemBlobStore {
    fn put(&self, content: &[u8], content_type: &str) -> Result<BlobRef, CtxmanError> {
        std::fs::create_dir_all(&self.root)?;
        let key = content_key(content);
        let target = self.path_for(&key);

        // Dedup: vorhandenes Blob nicht erneut schreiben (§7.1, immutable). Schreiben über
        // eine Temp-Datei + Rename, damit nie ein halb geschriebener Blob adressierbar ist.
        if !target.exists() {
            let temp = self.root.join(format!("{key}.tmp-{}", std::process::id()));
            std::fs::write(&temp, content)?;
            match std::fs::rename(&temp, &target) {
                Ok(()) => {}
                Err(err) => {
                    let _ = std::fs::remove_file(&temp);
                    return Err(err.into());
                }
            }
        }

        Ok(BlobRef {
            store: "fs".to_string(),
            key,
            size_bytes: content.len() as u64,
            content_type: content_type.to_string(),
        })
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, CtxmanError> {
        // Spec §10: nur content-addressed Keys (sha256) sind adressierbar.
        if !is_blob_key(key) {
            return Err(CtxmanError::BlobNotFound { key: key.to_string() });
        }
        match std::fs::read(self.path_for(key)) {
            Ok(content) => Ok(content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(CtxmanError::BlobNotFound { key: key.to_string() })
            }
            Err(err) => Err(err.into()),
        }
    }

    fn exists(&self, key: &str) -> Result<bool, CtxmanError> {
        // Spec §10: ein Nicht-sha256-Key adressiert per Definition keinen Blob ⇒ false statt
        // Pfad-Auflösung (Path-Traversal-Schutz).
        if !is_blob_key(key) {
            return Ok(false);
        }
        Ok(self.path_for(key).exists())
    }

    fn list(&self) -> Result<Vec<BlobInfo>, CtxmanError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut infos = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Temp-Dateien des Put-Pfads überspringen — ein Blob-Key ist immer der
            // 64-stellige lowercase-hex sha256.
            if !is_blob_key(name) {
                continue;
            }
            let meta = entry.metadata()?;
            infos.push(BlobInfo {
                key: name.to_string(),
                size_bytes: meta.len(),
                last_modified: modified_millis(&meta, entry.path().as_path()),
            });
        }
        infos.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(infos)
    }

    fn delete(&self, key: &str) -> Result<(), CtxmanError> {
        // Spec §10: nur content-addressed Keys löschen — niemals einen traversierenden Pfad.
        if !is_blob_key(key) {
            return Ok(());
        }
        // Idempotent: kein Fehler, wenn die Datei nicht existiert.
        match std::fs::remove_file(self.path_for(key)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

fn modified_millis(meta: &std::fs::Metadata, _path: &Path) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
