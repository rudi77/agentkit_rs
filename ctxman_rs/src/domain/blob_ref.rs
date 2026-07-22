use serde::{Deserialize, Serialize};

/// Pointer auf einen externalisierten, content-adressierten Inhalt im Blob Store (Spec §2.6).
/// Blob-Inhalte sind immutable (key = sha256 des Inhalts); BlobRefs haben keine eigene Identität.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub store: String,
    pub key: String,
    pub size_bytes: u64,
    pub content_type: String,
}
