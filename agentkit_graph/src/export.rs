//! Der vollständige Graph als serde-Projektion — für Betrachter und Werkzeuge
//! außerhalb dieses Crates.
//!
//! **Warum neben `Subgraph`/`render()`?** Die sind FÜRS MODELL: eine kleine,
//! bereits gefilterte und als Text gerenderte Auswahl, die in einen Prompt
//! passt. Ein Betrachter braucht das Gegenteil — alles, unverdichtet und
//! strukturiert, damit er selbst filtern und zeichnen kann. Zwei verschiedene
//! Zwecke, deshalb zwei Ausgaben; `render()` bleibt unverändert.

use serde::{Deserialize, Serialize};

use crate::model::{GraphClaim, GraphEntity, GraphEpisode, GraphRevision, GraphSource};
use crate::store::GraphIndex;

/// Alle Datensätze eines Stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphExport {
    pub revision: GraphRevision,
    pub entities: Vec<GraphEntity>,
    pub claims: Vec<GraphClaim>,
    pub sources: Vec<GraphSource>,
    pub episodes: Vec<GraphEpisode>,
}

/// Projiziert einen [`GraphIndex`] in eine serialisierbare Form.
///
/// Kopiert die Datensätze aus ihren `Arc`s heraus — das ist der Preis dafür,
/// dass `GraphExport` ein eigenständiger Wert ist, den ein Aufrufer
/// serialisieren kann, ohne die Lebensdauer des Snapshots zu halten. Bei den
/// dokumentierten Größen (≤ ~10⁵ Claims) ist das eine einmalige Kopie, kein
/// laufender Aufwand.
pub fn export(index: &GraphIndex) -> GraphExport {
    GraphExport {
        revision: index.revision(),
        entities: index.entities().map(|e| (**e).clone()).collect(),
        claims: index.claims().map(|c| (**c).clone()).collect(),
        sources: index.sources().map(|s| (**s).clone()).collect(),
        episodes: index.episodes().map(|e| (**e).clone()).collect(),
    }
}
