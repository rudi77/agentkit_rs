use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::blob_ref::BlobRef;
use super::enums::{Region, Role, SegmentState};
use crate::error::CtxmanError;

/// Das atomare Element des Contexts (Spec §2.2). Die Message-Liste der Provider-API wird
/// ausschließlich aus Segmenten gerendert; das Segment ist Source of Truth, die Message-Liste
/// ein Render-Artefakt (Spec §1.1).
///
/// Bewusst ein Struct mit privaten Feldern: Segmente durchlaufen einen Lebenszyklus
/// (live → externalized | compacted | evicted) und tragen die Invarianten I1–I3 als Verhalten.
/// Zustandsübergänge laufen ausschließlich über die Guard-Methoden, damit die Invarianten an
/// einer Stelle erzwungen werden. (Port von `Segment.cs`; Deserialisierung für Snapshots wird
/// beim Laden erneut gegen I2 validiert.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    id: Ulid,
    session_id: Ulid,
    region: Region,
    kind: String,
    source: Option<String>,
    role: Option<Role>,
    content: Option<String>,
    blob_ref: Option<BlobRef>,
    refetchable: bool,
    origin: Option<String>,
    summary: Option<String>,
    tool_call_id: Option<String>,
    frame_id: Option<Ulid>,
    pinned: bool,
    created_turn: u32,
    last_referenced_turn: u32,
    tokens: u32,
    seq: i64,
    state: SegmentState,
}

/// Vorlage für ein neues Segment: Pflichtfelder als `new`-Parameter, optionale Felder danach
/// per Feldzugriff setzen (Rust-Ersatz für die benannten Default-Argumente von
/// `Segment.CreateLive`/`CreateExternalized`).
#[derive(Debug, Clone)]
pub struct SegmentDraft {
    pub id: Ulid,
    pub session_id: Ulid,
    pub region: Region,
    pub kind: String,
    pub role: Option<Role>,
    pub seq: i64,
    pub created_turn: u32,
    pub source: Option<String>,
    pub refetchable: bool,
    pub origin: Option<String>,
    pub tool_call_id: Option<String>,
    pub frame_id: Option<Ulid>,
    pub pinned: bool,
    pub tokens: u32,
    pub summary: Option<String>,
}

impl SegmentDraft {
    pub fn new(
        id: Ulid,
        session_id: Ulid,
        region: Region,
        kind: &str,
        role: Option<Role>,
        seq: i64,
        created_turn: u32,
    ) -> Self {
        SegmentDraft {
            id,
            session_id,
            region,
            kind: kind.to_string(),
            role,
            seq,
            created_turn,
            source: None,
            refetchable: false,
            origin: None,
            tool_call_id: None,
            frame_id: None,
            pinned: false,
            tokens: 0,
            summary: None,
        }
    }
}

impl Segment {
    /// Erzeugt ein Segment aus vollständigem Zustand und erzwingt I2 (Spec §2.2): bei
    /// `state = live | externalized` dürfen `content` und `blob_ref` nicht beide `None` sein.
    /// Für die übliche Konstruktion stehen `create_live` und `create_externalized` bereit.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        draft: SegmentDraft,
        content: Option<String>,
        blob_ref: Option<BlobRef>,
        last_referenced_turn: u32,
        state: SegmentState,
    ) -> Result<Self, CtxmanError> {
        let segment = Segment {
            id: draft.id,
            session_id: draft.session_id,
            region: draft.region,
            kind: draft.kind,
            source: draft.source,
            role: draft.role,
            content,
            blob_ref,
            refetchable: draft.refetchable,
            origin: draft.origin,
            summary: draft.summary,
            tool_call_id: draft.tool_call_id,
            frame_id: draft.frame_id,
            pinned: draft.pinned,
            created_turn: draft.created_turn,
            last_referenced_turn,
            tokens: draft.tokens,
            seq: draft.seq,
            state,
        };
        segment.guard_content_invariant()?; // Spec §2.2 I2
        Ok(segment)
    }

    /// Factory für ein reguläres, inline gehaltenes Segment (state = live). I2 ist durch den
    /// nicht-optionalen `content` konstruktionsbedingt erfüllt.
    pub fn create_live(draft: SegmentDraft, content: &str) -> Self {
        let created_turn = draft.created_turn;
        Segment::new(
            draft,
            Some(content.to_string()),
            None,
            created_turn,
            SegmentState::Live,
        )
        .expect("I2 ist mit content != None immer erfüllt")
    }

    /// Factory für ein von Anfang an externalisiertes Segment (state = externalized) — der
    /// Upload-Pfad referenziert großen Inhalt direkt per blob_ref + summary (Spec §4.3).
    /// I2 ist durch den nicht-optionalen `blob_ref` konstruktionsbedingt erfüllt.
    pub fn create_externalized(draft: SegmentDraft, blob_ref: BlobRef) -> Self {
        let created_turn = draft.created_turn;
        Segment::new(
            draft,
            None,
            Some(blob_ref),
            created_turn,
            SegmentState::Externalized,
        )
        .expect("I2 ist mit blob_ref != None immer erfüllt")
    }

    /// Validiert I2 nach einer Deserialisierung (Snapshot-Load), da serde die Guard-
    /// Konstruktion umgeht.
    pub(crate) fn validate_invariants(&self) -> Result<(), CtxmanError> {
        self.guard_content_invariant()
    }

    // ---- Getter (Spec §2.2) ----

    pub fn id(&self) -> Ulid {
        self.id
    }

    pub fn session_id(&self) -> Ulid {
        self.session_id
    }

    /// Static | Working (Spec §2.2).
    pub fn region(&self) -> Region {
        self.region
    }

    /// Offenes Vokabular (Spec §2.2 / §2.3).
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Logische Herkunft, z. B. "core", "mcp:github" (Spec §2.2); Basis für Epoch-Diffs.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// system | user | assistant | tool (Spec §2.2).
    pub fn role(&self) -> Option<Role> {
        self.role
    }

    /// Inhalt; `None` ⇔ externalisiert (Spec §2.2). Nur via Guard-Methoden veränderbar.
    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// Pointer in den Blob Store (Spec §2.2 / §2.6). Nur via Guard-Methoden veränderbar.
    pub fn blob_ref(&self) -> Option<&BlobRef> {
        self.blob_ref.as_ref()
    }

    /// Inhalt verlustfrei aus externer Quelle neu beziehbar (Spec §2.2).
    pub fn refetchable(&self) -> bool {
        self.refetchable
    }

    /// Quell-URI bei refetchable, z. B. skill://… (Spec §2.2).
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    /// Typ-Signatur bei Externalisierung; Kurzfassung nach Compaction (Spec §2.2).
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Korrelation tool_use ↔ tool_result (Spec §2.2 / §2.4).
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    /// `None` = Root-Frame (Spec §2.2 / §2.5).
    pub fn frame_id(&self) -> Option<Ulid> {
        self.frame_id
    }

    /// Für GC unantastbar; rendert an chronologischer Position (Spec §2.2 / I4).
    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// Turn, in dem das Segment angelegt wurde (Spec §2.2).
    pub fn created_turn(&self) -> u32 {
        self.created_turn
    }

    /// Zuletzt referenzierter Turn; Basis für TTL-Liveness (Spec §2.2 / §3.2).
    pub fn last_referenced_turn(&self) -> u32 {
        self.last_referenced_turn
    }

    /// Gezählt beim Append, Tokenizer konfigurierbar (Spec §2.2).
    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    /// Globale, stabile Render-Reihenfolge innerhalb der Session (Spec §2.2 / I4).
    pub fn seq(&self) -> i64 {
        self.seq
    }

    /// live | externalized | compacted | evicted (Spec §2.2).
    pub fn state(&self) -> SegmentState {
        self.state
    }

    // ---- Guard-Methoden (Zustandsübergänge, Spec §2.2) ----

    /// Externalisiert das Segment (Minor GC, Spec §3.2 Schritt 2): Inhalt wandert atomar in den
    /// Blob Store, `content := None`, `blob_ref := ref`, `summary := summary`,
    /// `state := externalized`. Da blob_ref gesetzt wird, bleibt I2 (§2.2) gewahrt.
    /// I1 (§2.2): auf Static-Segmenten verboten.
    pub fn externalize(
        &mut self,
        blob_ref: BlobRef,
        summary: Option<String>,
        tokens: u32,
    ) -> Result<(), CtxmanError> {
        self.guard_not_static()?; // Spec §2.2 I1
        self.content = None;
        self.blob_ref = Some(blob_ref);
        self.summary = summary;
        // Spec §2.6/§4.3-Spiegel des Blob-Append-Pfads: die summary ist Render-Ersatz UND
        // Token-Basis. Ohne die Neuzählung bliebe der volle Original-Zählwert stehen und
        // die Watermark würde durch Externalisierung nie sinken.
        self.tokens = tokens;
        self.state = SegmentState::Externalized;
        // I2 bleibt gewahrt: blob_ref != None.
        Ok(())
    }

    /// Soft-Delete via Eviction (Spec §2.2 I3 / §3.2 Schritt 1+3): nur `state := evicted`.
    /// Daten (content/blob_ref/summary/origin) bleiben für den Audit-Trail erhalten — das
    /// Segment bleibt abfragbar, erscheint aber nie wieder im Render-Output.
    /// I1 (§2.2): auf Static verboten.
    pub fn evict(&mut self) -> Result<(), CtxmanError> {
        self.guard_not_static()?; // Spec §2.2 I1
        self.state = SegmentState::Evicted;
        // I3: keine Daten entfernen — reiner Zustandsübergang.
        Ok(())
    }

    /// Soft-Delete via Compaction (Spec §2.2 I3 / §3.3 Schritt 2): nur `state := compacted`,
    /// optional summary aktualisieren. Daten bleiben für den Audit-Trail erhalten.
    /// I1 (§2.2): auf Static verboten.
    pub fn compact(&mut self, summary: Option<String>) -> Result<(), CtxmanError> {
        self.guard_not_static()?; // Spec §2.2 I1
        if let Some(summary) = summary {
            self.summary = Some(summary);
        }
        self.state = SegmentState::Compacted;
        // I3: content/blob_ref bleiben als Audit-Metadatum erhalten — kein Datenverlust.
        Ok(())
    }

    /// Aktualisiert `last_referenced_turn` (Spec §3.4: Page Fault setzt approximierte Liveness).
    /// I1 (§2.2): Static-Segmente sind immutable — Update verboten.
    pub fn mark_referenced(&mut self, turn: u32) -> Result<(), CtxmanError> {
        self.guard_not_static()?; // Spec §2.2 I1
        self.last_referenced_turn = turn;
        Ok(())
    }

    /// Pinnt das Segment (Spec §4.3). I1 (§2.2): auf Static verboten.
    pub fn pin(&mut self) -> Result<(), CtxmanError> {
        self.guard_not_static()?; // Spec §2.2 I1
        self.pinned = true;
        Ok(())
    }

    /// Entfernt den Pin (Spec §4.3). I1 (§2.2): auf Static verboten.
    pub fn unpin(&mut self) -> Result<(), CtxmanError> {
        self.guard_not_static()?; // Spec §2.2 I1
        self.pinned = false;
        Ok(())
    }

    fn guard_not_static(&self) -> Result<(), CtxmanError> {
        if self.region == Region::Static {
            return Err(CtxmanError::StaticRegionImmutable {
                segment_id: self.id,
            });
        }
        Ok(())
    }

    fn guard_content_invariant(&self) -> Result<(), CtxmanError> {
        // Spec §2.2 I2: content und blob_ref sind nie beide None bei state = live | externalized.
        if matches!(self.state, SegmentState::Live | SegmentState::Externalized)
            && self.content.is_none()
            && self.blob_ref.is_none()
        {
            return Err(CtxmanError::SegmentContentInvariant {
                state: self.state.wire_name(),
            });
        }
        Ok(())
    }
}
