use std::collections::HashMap;
use std::sync::Mutex;

use super::{content_key, is_blob_key, BlobInfo, BlobStore};
use crate::domain::BlobRef;
use crate::error::CtxmanError;

/// In-Memory-Blob-Store (Default + Tests): content-addressed HashMap hinter einem Mutex.
/// Externalisierung wirkt nur innerhalb des Prozesses — für Persistenz den
/// [`super::FileSystemBlobStore`] verwenden.
#[derive(Default)]
pub struct InMemoryBlobStore {
    blobs: Mutex<HashMap<String, StoredBlob>>,
}

struct StoredBlob {
    content: Vec<u8>,
    stored_at: i64,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BlobStore for InMemoryBlobStore {
    fn put(&self, content: &[u8], content_type: &str) -> Result<BlobRef, CtxmanError> {
        let key = content_key(content);
        let mut blobs = self.blobs.lock().expect("Blob-Mutex nicht poisoned");
        // Dedup: gleicher Inhalt ⇒ gleicher Key ⇒ kein erneutes Schreiben (Spec §7.1, immutable).
        blobs.entry(key.clone()).or_insert_with(|| StoredBlob {
            content: content.to_vec(),
            stored_at: 0,
        });
        Ok(BlobRef {
            store: "memory".to_string(),
            key,
            size_bytes: content.len() as u64,
            content_type: content_type.to_string(),
        })
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, CtxmanError> {
        if !is_blob_key(key) {
            return Err(CtxmanError::BlobNotFound { key: key.to_string() });
        }
        let blobs = self.blobs.lock().expect("Blob-Mutex nicht poisoned");
        blobs
            .get(key)
            .map(|b| b.content.clone())
            .ok_or_else(|| CtxmanError::BlobNotFound { key: key.to_string() })
    }

    fn exists(&self, key: &str) -> Result<bool, CtxmanError> {
        if !is_blob_key(key) {
            return Ok(false);
        }
        let blobs = self.blobs.lock().expect("Blob-Mutex nicht poisoned");
        Ok(blobs.contains_key(key))
    }

    fn list(&self) -> Result<Vec<BlobInfo>, CtxmanError> {
        let blobs = self.blobs.lock().expect("Blob-Mutex nicht poisoned");
        let mut infos: Vec<BlobInfo> = blobs
            .iter()
            .map(|(key, blob)| BlobInfo {
                key: key.clone(),
                size_bytes: blob.content.len() as u64,
                last_modified: blob.stored_at,
            })
            .collect();
        infos.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(infos)
    }

    fn delete(&self, key: &str) -> Result<(), CtxmanError> {
        if !is_blob_key(key) {
            return Ok(());
        }
        let mut blobs = self.blobs.lock().expect("Blob-Mutex nicht poisoned");
        blobs.remove(key);
        Ok(())
    }
}
