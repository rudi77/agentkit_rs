//! Blob Storage (Spec §7): content-addressed (key = sha256), immutable, Dedup.

pub mod fs;
pub mod memory;

pub use fs::FileSystemBlobStore;
pub use memory::InMemoryBlobStore;

use crate::domain::BlobRef;
use crate::error::CtxmanError;

/// Metadaten eines gespeicherten Blobs (Port von `BlobInfo.cs`); `last_modified` in Unix-Millis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    pub key: String,
    pub size_bytes: u64,
    pub last_modified: i64,
}

/// Adapter-Trait auf den Blob Store (Spec §7). Content-addressed (key = sha256) und immutable.
/// Synchroner Port von `IBlobStore` ohne Tenant-Parameter (in-process, single-tenant).
pub trait BlobStore: Send + Sync {
    /// Speichert `content` content-adressiert und liefert einen `BlobRef`.
    /// Gleicher Inhalt ⇒ gleicher Key ⇒ kein erneutes Schreiben (Dedup, Spec §7.1).
    fn put(&self, content: &[u8], content_type: &str) -> Result<BlobRef, CtxmanError>;

    /// Liest den Inhalt für den gegebenen `key` (sha256).
    fn get(&self, key: &str) -> Result<Vec<u8>, CtxmanError>;

    /// Prüft, ob für `key` ein Inhalt existiert.
    fn exists(&self, key: &str) -> Result<bool, CtxmanError>;

    /// Listet alle gespeicherten Blobs. Fehlende Ablage ⇒ leere Liste.
    fn list(&self) -> Result<Vec<BlobInfo>, CtxmanError>;

    /// Löscht den Blob mit `key` idempotent (kein Fehler, wenn nicht vorhanden).
    fn delete(&self, key: &str) -> Result<(), CtxmanError>;
}

/// Spec §2.6 / §10: Blob-Keys sind content-addressed (sha256, 64 lowercase-hex). Verhindert,
/// dass ein gelieferter Key als Path-Traversal in einen Store durchschlägt.
pub fn is_blob_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// SHA-256 (lowercase hex) eines Inhalts — der content-addressed Key (Spec §2.6).
pub(crate) fn content_key(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        write!(hex, "{byte:02x}").expect("write! auf String schlägt nie fehl");
    }
    hex
}
