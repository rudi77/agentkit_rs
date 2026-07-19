use ulid::Ulid;

/// Typisierte Fehler der Bibliothek. Ersetzt die Domain-Exceptions und HTTP-Statuscodes des
/// C#-Originals (`DomainExceptions.cs`, 409/413/422-Antworten der Endpoints) — der Host
/// entscheidet selbst, wie er die Fälle abbildet.
#[derive(Debug)]
pub enum CtxmanError {
    /// Spec §2.2 I1: Static-Segmente sind innerhalb einer Epoche immutable (C#: 409).
    StaticRegionImmutable { segment_id: Ulid },
    /// Spec §2.2 I2: content und blob_ref dürfen bei live|externalized nie beide fehlen.
    SegmentContentInvariant { state: &'static str },
    /// Spec §3.1: Budget auch nach Emergency-GC über der Schwelle (C#: 413, retrybar).
    BudgetExceeded { tokens_total: i64, budget: u32 },
    /// Spec §2 I5: Render mit unvollständigen Units — tool_calls ohne tool_result (C#: 422).
    IncompleteUnits { open_tool_call_ids: Vec<String> },
    /// Spec §4.3 / §7.1: Ref nicht mehr expandierbar (evicted/compacted/gesweept; C#: 410).
    /// Trägt die bestmögliche Restinformation.
    RefGone { summary: Option<String>, origin: Option<String> },
    /// Spec §4.2 / I1: Append in die Static-Region ist nur via Epoch-Bump zulässig (C#: 409).
    StaticAppendForbidden,
    /// Spec §4.3: Inline-Content über 1 MiB — großer Inhalt läuft über den Blob-Pfad (C#: 413).
    ContentTooLarge { bytes: usize },
    /// Spec §2.5: Verletzung der Stack-Disziplin (LIFO) bei Frame-Push/Pop (C#: 409).
    FrameDiscipline(String),
    /// Unbekannter Provider-Name im Render (C#: 400 mit Liste der registrierten Adapter).
    UnknownProvider(String),
    /// Blob-Key entspricht nicht dem content-addressed Format (64 Hex-Zeichen, lowercase).
    InvalidBlobKey(String),
    /// Blob existiert nicht im Store.
    BlobNotFound { key: String },
    /// Segment-ID nicht in der Session vorhanden.
    SegmentNotFound { id: Ulid },
    /// Frame-ID nicht in der Session vorhanden.
    FrameNotFound { id: Ulid },
    /// Ungültige Policy-Konfiguration (Validierung beim Anlegen, Spec §5).
    InvalidPolicy(String),
    /// Ungültiges Dauer-Format in der Konfiguration (Spec §7.1, z. B. "24h").
    InvalidDuration(String),
    /// Fehler des Compaction-Modells oder fehlende Konfiguration (Spec §3.3).
    Compaction(String),
    /// Fehler der Promotion-Senke (Spec §3.3 Schritt 1; C#: retrybarer 503).
    Promotion(String),
    /// Snapshot-Serialisierung/-Deserialisierung fehlgeschlagen.
    Snapshot(String),
    /// I/O-Fehler (Blob Store, Snapshot-Dateien).
    Io(std::io::Error),
}

impl std::fmt::Display for CtxmanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtxmanError::StaticRegionImmutable { segment_id } => write!(
                f,
                "Static-Segment {segment_id} ist innerhalb der Epoche immutable (Spec §2.2 I1)"
            ),
            CtxmanError::SegmentContentInvariant { state } => write!(
                f,
                "content und blob_ref dürfen bei state={state} nicht beide fehlen (Spec §2.2 I2)"
            ),
            CtxmanError::BudgetExceeded { tokens_total, budget } => write!(
                f,
                "Token-Budget überschritten: {tokens_total} von {budget} — asynchrone GC abwarten und wiederholen (Spec §3.1)"
            ),
            CtxmanError::IncompleteUnits { open_tool_call_ids } => write!(
                f,
                "Render mit unvollständigen Units — offene tool_calls: {} (Spec §2 I5)",
                open_tool_call_ids.join(", ")
            ),
            CtxmanError::RefGone { .. } => write!(
                f,
                "Segment ist nicht mehr expandierbar (evicted/compacted oder Blob gesweept, Spec §4.3/§7.1)"
            ),
            CtxmanError::StaticAppendForbidden => write!(
                f,
                "Append in die Static-Region ist nur über den Epoch-Bump zulässig (Spec §4.2 / I1)"
            ),
            CtxmanError::ContentTooLarge { bytes } => write!(
                f,
                "Inline-Content ({bytes} Bytes) überschreitet 1 MiB — großen Inhalt als Blob anhängen (Spec §4.3)"
            ),
            CtxmanError::FrameDiscipline(msg) => {
                write!(f, "Frame-Stack-Disziplin verletzt: {msg} (Spec §2.5)")
            }
            CtxmanError::UnknownProvider(name) => {
                write!(f, "Unbekannter Provider '{name}' (verfügbar: anthropic, openai)")
            }
            CtxmanError::InvalidBlobKey(key) => write!(
                f,
                "Ungültiger Blob-Key '{key}' — erwartet werden 64 Hex-Zeichen (sha256, lowercase)"
            ),
            CtxmanError::BlobNotFound { key } => write!(f, "Blob '{key}' nicht gefunden"),
            CtxmanError::SegmentNotFound { id } => write!(f, "Segment {id} nicht gefunden"),
            CtxmanError::FrameNotFound { id } => write!(f, "Frame {id} nicht gefunden"),
            CtxmanError::InvalidPolicy(msg) => write!(f, "Ungültige Policy: {msg}"),
            CtxmanError::InvalidDuration(value) => write!(f, "Ungültige Dauer '{value}'"),
            CtxmanError::Compaction(msg) => write!(f, "Compaction fehlgeschlagen: {msg}"),
            CtxmanError::Promotion(msg) => write!(f, "Promotion fehlgeschlagen: {msg}"),
            CtxmanError::Snapshot(msg) => write!(f, "Snapshot-Fehler: {msg}"),
            CtxmanError::Io(err) => write!(f, "I/O-Fehler: {err}"),
        }
    }
}

impl std::error::Error for CtxmanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CtxmanError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CtxmanError {
    fn from(err: std::io::Error) -> Self {
        CtxmanError::Io(err)
    }
}
