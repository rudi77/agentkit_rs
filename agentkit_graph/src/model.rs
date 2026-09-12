//! Das Domänenmodell: Entity, Claim, Source, Episode — plus Ebene, Scope und Sicht.
//!
//! Zwei Festlegungen prägen alles Weitere:
//!
//! 1. **Ein Fakt ist ein Claim mit Quelle.** Es gibt keinen Weg, einen Claim ohne
//!    mindestens eine [`GraphSource`] anzulegen (siehe [`crate::write::ClaimDraft`])
//!    — Provenance ist damit strukturell erzwungen, nicht per Konvention.
//! 2. **Ebene und Scope sind zwei getrennte Achsen.** Die Ebene sagt, wie belastbar
//!    ein Datensatz ist (`working` = vorläufig, `canonical` = konsolidiert), der
//!    Scope sagt, wem er gehört (Session, Swarm-Lauf, Workspace). Sichtbarkeit wird
//!    ausschließlich über den Scope geregelt — deshalb reichen zwei Ebenen.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub type EntityId = String;
pub type ClaimId = String;
pub type SourceId = String;
pub type EpisodeId = String;

/// Monoton steigende Version des gesamten Stores. Jede committete Mutation
/// erhöht sie um 1; sie ist zugleich Mutations-ID, Audit-Marke und
/// Aktualitäts-Maß beim Ranking (deterministisch — anders als eine Uhrzeit).
pub type GraphRevision = u64;

/// Wie belastbar ein Datensatz ist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphLayer {
    /// Vorläufig: Beobachtungen und Hypothesen eines laufenden Runs.
    Working,
    /// Konsolidiert: dauerhaftes Wissen, nur über Promotion erreichbar.
    Canonical,
}

impl GraphLayer {
    pub fn wire_name(&self) -> &'static str {
        match self {
            GraphLayer::Working => "working",
            GraphLayer::Canonical => "canonical",
        }
    }
}

impl fmt::Display for GraphLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Wem ein Datensatz gehört — ein freies Paar aus Art und ID.
///
/// Bewusst kein Enum mit acht Varianten (Mandant, Workspace, Session, Swarm-Lauf,
/// …): jede Variante wäre eine Fallunterscheidung in Store, Sicht und Tests, und
/// heute gibt es genau drei reale Arten (`workspace`, `session`, `swarm`). Neue
/// Arten kosten so keinen Code, nur eine Konvention.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphScope {
    pub kind: String,
    pub id: String,
}

impl GraphScope {
    pub fn new(kind: &str, id: &str) -> Self {
        GraphScope {
            kind: kind.to_string(),
            id: id.to_string(),
        }
    }
    /// Workspace/Projekt — der übliche Heimat-Scope des kanonischen Wissens.
    pub fn workspace(id: &str) -> Self {
        GraphScope::new("workspace", id)
    }
    /// Ein einzelner Agent-Lauf bzw. eine Session.
    pub fn session(id: &str) -> Self {
        GraphScope::new("session", id)
    }
    /// Ein Schwarm-Lauf — der gemeinsame Arbeitsstand mehrerer Agenten.
    pub fn swarm(id: &str) -> Self {
        GraphScope::new("swarm", id)
    }
    /// Privater Scope eines Agenten innerhalb eines Laufs.
    pub fn agent(agent_id: &str, run_id: &str) -> Self {
        GraphScope::new("agent", &format!("{agent_id}/{run_id}"))
    }
}

impl fmt::Display for GraphScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}

/// Ebene + Scope: die Adresse, unter der gelesen oder geschrieben wird.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphTarget {
    pub layer: GraphLayer,
    pub scope: GraphScope,
}

impl GraphTarget {
    pub fn new(layer: GraphLayer, scope: GraphScope) -> Self {
        GraphTarget { layer, scope }
    }
    pub fn canonical(scope: GraphScope) -> Self {
        GraphTarget::new(GraphLayer::Canonical, scope)
    }
    pub fn working(scope: GraphScope) -> Self {
        GraphTarget::new(GraphLayer::Working, scope)
    }
}

impl fmt::Display for GraphTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.layer, self.scope)
    }
}

/// Welche Ziele ein Leser sieht — **die Reihenfolge ist die Priorität**: das erste
/// Ziel gewinnt bei widersprüchlichen Angaben die Darstellung. Beide Claims bleiben
/// erhalten, priorisiert wird nur die Reihenfolge im gerenderten Ausschnitt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphView {
    pub targets: Vec<GraphTarget>,
}

impl GraphView {
    pub fn new(targets: Vec<GraphTarget>) -> Self {
        GraphView { targets }
    }

    pub fn with(mut self, target: GraphTarget) -> Self {
        self.targets.push(target);
        self
    }

    /// Position des Ziels in der Sicht (0 = höchste Priorität), `None` = nicht sichtbar.
    pub fn position(&self, layer: GraphLayer, scope: &GraphScope) -> Option<usize> {
        self.targets
            .iter()
            .position(|t| t.layer == layer && &t.scope == scope)
    }

    pub fn sees(&self, layer: GraphLayer, scope: &GraphScope) -> bool {
        self.position(layer, scope).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

/// Belastbarkeit eines Claims. Vier Werte, nicht neun: mehr Zustände heißt mehr
/// Übergänge, und `candidate`/`supported`/`contested` haben im MVP keinen Erzeuger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// Beobachtet — direkt aus einem Tool-Ergebnis, Dokument oder Testlauf.
    Observation,
    /// Vermutet — noch nicht belegt.
    Hypothesis,
    /// Bestätigt — durch Promotion in den kanonischen Graphen übernommen.
    Confirmed,
    /// Ersetzt — ein neuerer Claim widerspricht ihm (siehe `superseded_by`).
    Superseded,
}

impl ClaimStatus {
    pub fn wire_name(&self) -> &'static str {
        match self {
            ClaimStatus::Observation => "observation",
            ClaimStatus::Hypothesis => "hypothesis",
            ClaimStatus::Confirmed => "confirmed",
            ClaimStatus::Superseded => "superseded",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "observation" => Some(ClaimStatus::Observation),
            "hypothesis" => Some(ClaimStatus::Hypothesis),
            "confirmed" => Some(ClaimStatus::Confirmed),
            "superseded" => Some(ClaimStatus::Superseded),
            _ => None,
        }
    }

    /// Ranking-Beitrag: Bestätigtes vor Beobachtetem vor Vermutetem, Ersetztes
    /// wird nach hinten gedrückt (aber nicht ausgeblendet — es ist Evidenz).
    pub fn rank_bonus(&self) -> f32 {
        match self {
            ClaimStatus::Confirmed => 1.0,
            ClaimStatus::Observation => 0.5,
            ClaimStatus::Hypothesis => 0.2,
            ClaimStatus::Superseded => -1.0,
        }
    }
}

impl fmt::Display for ClaimStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Ein Akteur nach der Actor-Konvention des Open Knowledge Format (Spec §7):
/// `<producer>/<version>` für Agenten/Werkzeuge, `human:<id>` für Menschen,
/// `process:<id>` für automatisierte Prozesse. Die interne Zeichenkette ist
/// bereits die validierte, gerenderte Form — ein `Actor` existiert nur, wenn er
/// eine dieser drei Formen trifft (siehe [`Actor::parse`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Actor(String);

impl Actor {
    /// Prüft gegen dieselbe Form wie der Referenz-Validator
    /// (`^(?:[^\s:/]+:\S+|\S+/\S+)$`): entweder `<praefix>:<rest>` ohne `/` im
    /// Präfix, oder `<a>/<b>`. Beides ohne jedes Whitespace — sonst wäre ein
    /// Satz wie "der Agent" ein "gültiger" Actor.
    ///
    /// Lehnt zusätzlich Beinahe-Treffer ab: ein Text, der mit `human`/`process`
    /// beginnt (ohne Rücksicht auf Groß-/Kleinschreibung), aber nicht exakt mit
    /// `human:`/`process:` startet, läse sich sonst stillschweigend als Agent
    /// — genau die Verwechslung, vor der der Referenz-Validator warnt. `parse`
    /// geht einen Schritt weiter als die Warnung und lehnt ab: ein Actor, der
    /// aussieht wie ein Mensch, es aber nicht ist, wäre schlimmer als gar
    /// keiner (er würde unbemerkt als Agent durchgehen).
    pub fn parse(text: &str) -> Option<Actor> {
        let t = text.trim();
        if t.is_empty() || t.chars().any(char::is_whitespace) {
            return None;
        }
        let lower = t.to_lowercase();
        if (lower.starts_with("human") && !t.starts_with("human:"))
            || (lower.starts_with("process") && !t.starts_with("process:"))
        {
            return None;
        }
        if actor_colon_form(t) || actor_slash_form(t) {
            Some(Actor(t.to_string()))
        } else {
            None
        }
    }

    /// `human:<id>` — ein Mensch. Steuert laut Spec §5.3 den Trust-Tier eines
    /// Konsumenten, deshalb gibt es dafür KEINEN automatischen Migrationspfad
    /// (siehe [`Actor::from_principal`]).
    ///
    /// Der Bestandteil läuft durch [`slugify`], damit die im Typ dokumentierte
    /// Invariante auch hier gilt: `Actor::human("")` ergäbe sonst `human:` und
    /// `Actor::agent("zwei worte")` einen Actor mit Leerzeichen — beides
    /// Formen, die der Referenz-Validator bemängelt, und zwar erst beim
    /// Schreiben des Bundles, weit weg von der Ursache.
    pub fn human(id: &str) -> Actor {
        Actor(format!("human:{}", slugify(id)))
    }

    /// `process:<id>` — ein automatisierter Prozess ohne Modell dahinter.
    pub fn process(id: &str) -> Actor {
        Actor(format!("process:{}", slugify(id)))
    }

    /// `<name>/<version>` — ein Agent oder Werkzeug, z. B. `agentkit-graph/0.1.0`.
    pub fn producer(name: &str, version: &str) -> Actor {
        Actor(format!("{}/{}", slugify(name), slugify(version)))
    }

    /// `agent:<principal>` — siehe [`Actor::from_principal`] für die Begründung,
    /// warum dies (und nicht `human:`) der Migrations-Fallback ist.
    pub fn agent(principal: &str) -> Actor {
        Actor(format!("agent:{}", slugify(principal)))
    }

    /// Der zentrale Migrationspfad von einem blanken Principal (`"tester"`,
    /// `"runtime"`, einer Agent-ID) zu einem gültigen Actor: trifft der Text
    /// bereits eine der drei Formen, bleibt er unverändert; sonst wird
    /// `agent:<slug>` daraus.
    ///
    /// Warum `agent:` und nicht `human:`/`process:`: der Graph weiß an dieser
    /// Stelle nicht, ob hinter einem Principal ein Mensch oder ein
    /// automatisierter Prozess steckt — und `human:` steuert laut Spec §5.3
    /// den Trust-Tier, den ein OKF-Konsument daraus ableitet. Einen Principal
    /// als `human:` zu markieren, nur weil er kein Schrägstrich-Format hat,
    /// wäre eine erfundene Vertrauensaussage. `agent:` behauptet nur das, was
    /// wir wirklich wissen: irgendetwas hat geschrieben.
    pub fn from_principal(text: &str) -> Actor {
        if let Some(actor) = Actor::parse(text) {
            return actor;
        }
        Actor(format!("agent:{}", slugify(text)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_human(&self) -> bool {
        self.0.starts_with("human:")
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Serialisiert als BLANKER String, nicht als `{"0": "..."}` — damit die
// JSON-Form von `crate::export::GraphExport` unverändert bleibt
// (`agentkit_viz/src/assets/app.js` liest `created_by`/`agent_id` als
// Strings, kein Frontend-Umbau nötig). Deserialisiert wird über
// `from_principal`, NICHT über einen direkten String-Constructor: so
// normalisiert das Einlesen einer alten `graph.jsonl`-Zeile mit einem
// blanken Legacy-Principal (aus der Zeit vor dieser Migration) ihn beim
// Laden automatisch zu einem gültigen Actor, statt beim Start mit einem
// Journal-Fehler abzubrechen.
impl Serialize for Actor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Actor::from_principal(&text))
    }
}

fn actor_colon_form(t: &str) -> bool {
    match t.find(':') {
        Some(idx) if idx > 0 => {
            let prefix = &t[..idx];
            let suffix = &t[idx + 1..];
            !prefix.contains('/') && !suffix.is_empty()
        }
        _ => false,
    }
}

fn actor_slash_form(t: &str) -> bool {
    match t.find('/') {
        Some(idx) => idx > 0 && idx + 1 < t.len(),
        None => false,
    }
}

/// Whitespace zu `-`, nie leer (Fallback `"unbekannt"`) — die Grundlage für
/// den `agent:<slug>`-Fallback in [`Actor::from_principal`]. Andere
/// Sonderzeichen (`:`, `/`) bleiben unangetastet: die Actor-Form erlaubt sie
/// im Suffix nach dem ersten `:`, ein Escapen wäre also unnötige Arbeit.
fn slugify(text: &str) -> String {
    let trimmed = text.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut pending_dash = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            pending_dash = !out.is_empty();
        } else {
            if pending_dash {
                out.push('-');
                pending_dash = false;
            }
            out.push(ch);
        }
    }
    if out.is_empty() {
        "unbekannt".to_string()
    } else {
        out
    }
}

/// Eine Bestätigung eines Claims (OKF §5.2 „verified"): wer hat wann bestätigt.
/// Mehrere Einträge sind ausdrücklich vorgesehen — unabhängige Bestätigungen
/// nebeneinander sind der Sinn der Liste, keine Fehlerkorrektur.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub by: Actor,
    /// Unix-Millisekunden, dieselbe Zeitbasis wie `created_at`.
    pub at: u64,
}

/// Ein Knoten: eine Sache, über die etwas ausgesagt wird.
///
/// Aliase liegen als Liste **im** Datensatz statt in einer eigenen Tabelle — im
/// MVP hat kein Alias eigene Provenance oder Konfidenz, eine zweite Datensatzart
/// wäre reine Buchhaltung.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEntity {
    pub id: EntityId,
    pub canonical_name: String,
    pub entity_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Normalisierte Schreibweisen (siehe [`normalize`]), inklusive des kanonischen
    /// Namens. Der Auflöser trifft ausschließlich auf dieser Liste.
    #[serde(default)]
    pub aliases: Vec<String>,
    pub layer: GraphLayer,
    pub scope: GraphScope,
    /// Scope, aus dem die Entity bei einer Promotion mitgewandert ist.
    ///
    /// Dieselbe Begründung wie bei [`GraphClaim::promoted_from`]: eine
    /// kanonische Entity, die einmal in einem Session-Scope entstanden ist,
    /// soll das nach der Kompaktierung noch von sich sagen können.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<GraphScope>,
    pub created_revision: GraphRevision,
    pub updated_revision: GraphRevision,
    pub created_at: u64,
    /// Frontmatter-Schlüssel, die nicht aus diesem Crate stammen — von Hand
    /// ergänzte OKF-Felder (`resource`, `stale_after`) oder Felder eines
    /// Profils (`owner`, `domain`). Werden unverändert durchgereicht: die Spec
    /// verlangt in §4.1 ausdrücklich, dass ein Konsument unbekannte Schlüssel
    /// beim Round-Trip erhält.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, crate::okf::yaml::YamlValue>,
}

/// Eine Kante: Subjekt–Prädikat–Objekt plus Belastbarkeit und Herkunft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphClaim {
    pub id: ClaimId,
    pub subject: EntityId,
    pub predicate: String,
    pub object: EntityId,
    pub layer: GraphLayer,
    pub scope: GraphScope,
    pub status: ClaimStatus,
    pub confidence: f32,
    /// Mindestens ein Eintrag — ein Claim ohne Quelle kann gar nicht entstehen.
    pub source_ids: Vec<SourceId>,
    /// Wer den Claim erzeugt hat. Setzt IMMER die Laufzeit aus dem
    /// [`crate::GraphAccess`], nie ein Modellargument.
    pub created_by: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<ClaimId>,
    /// Scope, aus dem der Claim promotet wurde (Audit-Spur der Promotion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<GraphScope>,
    /// Status, den er VOR der Promotion hatte.
    ///
    /// Ohne ihn ist an einem kanonischen Claim nicht mehr abzulesen, ob hier
    /// eine Beobachtung dauerhaft wurde oder eine bloße Vermutung — der
    /// Unterschied, auf dem die ganze Zweiteilung in `working`/`canonical`
    /// beruht. Am Datensatz und nicht im Journal, weil die Kompaktierung nur
    /// den aktuellen Stand schreibt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from_status: Option<ClaimStatus>,
    /// Bestätigungen des Claims (OKF §5.2) — angehängt, nie ersetzt. Heute der
    /// einzige Erzeuger: [`crate::write::promote_claim`] hängt beim Promoten
    /// einen Eintrag an (siehe dort für die Begründung).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified: Vec<Verification>,
    pub created_revision: GraphRevision,
    pub updated_revision: GraphRevision,
    pub created_at: u64,
}

impl GraphClaim {
    /// Ein Claim ist widersprüchlich zu einem anderen, wenn Subjekt und Prädikat
    /// gleich sind, das Objekt aber nicht. Genau darauf greift die Promotion zu.
    pub fn contradicts(&self, other: &GraphClaim) -> bool {
        self.subject == other.subject
            && normalize(&self.predicate) == normalize(&other.predicate)
            && self.object != other.object
    }
}

/// Woher ein Claim kommt. Der `content_hash` erlaubt Wiedererkennung derselben
/// Quelle ohne Volltextvergleich.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphSource {
    pub id: SourceId,
    /// Freier Typ: `agent_turn`, `tool_result`, `document`, `test_run`, …
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub content_hash: String,
    pub created_revision: GraphRevision,
    pub created_at: u64,
}

/// Ein Ereignis statt einer Aussage: „Agent X hat Y getan."
///
/// Episoden liegen bewusst neben den Claims und nicht in ihnen — sie werden nie
/// promotet und nie traversiert, sie sind der Verlauf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEpisode {
    pub id: EpisodeId,
    pub actor: Actor,
    pub summary: String,
    pub scope: GraphScope,
    pub source_ids: Vec<SourceId>,
    pub created_revision: GraphRevision,
    pub created_at: u64,
}

/// Normalisierte Schreibweise für Alias-Vergleiche und Token-Overlap:
/// Kleinschreibung, alles Nicht-Alphanumerische wird zu einem Trenner.
///
/// `"MCP-Client (stdio)"` und `"mcp client stdio"` sind damit dasselbe Alias.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            pending_space = true;
        }
    }
    out
}

/// Tokens einer normalisierten Zeichenkette — die Grundlage des Overlap-Scores.
pub fn tokens(text: &str) -> Vec<String> {
    normalize(text)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// FNV-1a (64 bit) als Hex — reicht für Quellen-Wiedererkennung und spart die
/// `sha2`-Dependency. Kein kryptografischer Hash und nirgends als solcher benutzt.
pub fn content_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Unix-Zeit in Millisekunden. Nur Anzeige/Audit — Sortierung und Ranking laufen
/// über die Revision, damit Tests deterministisch bleiben.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sortierschlüssel für IDs der Form `C-17`: numerisch, nicht lexikografisch
/// (sonst stünde `C-10` vor `C-9`). Fällt auf 0 zurück, wenn kein Suffix da ist —
/// die ID selbst bleibt zweiter Schlüssel, damit die Ordnung total ist.
pub fn id_order(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_vereinheitlicht_schreibweisen() {
        assert_eq!(normalize("MCP-Client (stdio)"), "mcp client stdio");
        assert_eq!(normalize("  Session   Mutex "), "session mutex");
        assert_eq!(normalize("Größe"), "größe");
        assert_eq!(normalize("---"), "");
    }

    #[test]
    fn id_order_sortiert_numerisch() {
        let mut ids = vec!["C-10".to_string(), "C-9".to_string(), "C-100".to_string()];
        ids.sort_by_key(|i| id_order(i));
        assert_eq!(ids, vec!["C-9", "C-10", "C-100"]);
    }

    #[test]
    fn view_position_ist_die_prioritaet() {
        let view = GraphView::new(vec![
            GraphTarget::working(GraphScope::session("s1")),
            GraphTarget::canonical(GraphScope::workspace("w")),
        ]);
        assert_eq!(
            view.position(GraphLayer::Working, &GraphScope::session("s1")),
            Some(0)
        );
        assert_eq!(
            view.position(GraphLayer::Canonical, &GraphScope::workspace("w")),
            Some(1)
        );
        assert!(!view.sees(GraphLayer::Working, &GraphScope::session("andere")));
    }

    #[test]
    fn content_hash_ist_stabil_und_unterscheidet() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
        assert_eq!(content_hash("abc").len(), 16);
    }

    #[test]
    fn actor_erkennt_alle_drei_okf_formen() {
        assert_eq!(
            Actor::parse("agentkit-graph/0.1.0")
                .as_ref()
                .map(Actor::as_str),
            Some("agentkit-graph/0.1.0")
        );
        assert_eq!(
            Actor::parse("human:dana").as_ref().map(Actor::as_str),
            Some("human:dana")
        );
        assert_eq!(
            Actor::parse("process:cron-42").as_ref().map(Actor::as_str),
            Some("process:cron-42")
        );
        assert!(Actor::human("dana").is_human());
        assert!(!Actor::process("cron-42").is_human());
        assert_eq!(
            Actor::producer("agentkit-graph", "0.1.0").as_str(),
            "agentkit-graph/0.1.0"
        );
        assert_eq!(Actor::agent("tester").as_str(), "agent:tester");
    }

    /// Die bequemen Konstruktoren dürfen die Typ-Invariante nicht unterlaufen:
    /// was hier herauskommt, muss `parse` wieder akzeptieren — sonst schreibt
    /// der Bundle-Adapter später ein `generated.by`, das der Referenz-Validator
    /// bemängelt, und die Ursache liegt an ganz anderer Stelle.
    #[test]
    fn die_konstruktoren_liefern_immer_eine_gueltige_form() {
        for actor in [
            Actor::human(""),
            Actor::process("  "),
            Actor::agent("zwei worte"),
            Actor::producer("agentkit-graph", ""),
            Actor::producer("", "0.1.0"),
        ] {
            assert_eq!(
                Actor::parse(actor.as_str()).as_ref().map(Actor::as_str),
                Some(actor.as_str()),
                "{actor} trifft keine Actor-Form"
            );
        }
        assert_eq!(Actor::agent("zwei worte").as_str(), "agent:zwei-worte");
        assert_eq!(Actor::human("").as_str(), "human:unbekannt");
    }

    #[test]
    fn from_principal_laesst_gueltige_actors_unveraendert() {
        assert_eq!(Actor::from_principal("human:dana").as_str(), "human:dana");
        assert_eq!(
            Actor::from_principal("agentkit-graph/0.1.0").as_str(),
            "agentkit-graph/0.1.0"
        );
    }

    #[test]
    fn from_principal_macht_aus_blanken_principals_einen_agent_actor() {
        assert_eq!(Actor::from_principal("tester").as_str(), "agent:tester");
        assert_eq!(Actor::from_principal("runtime").as_str(), "agent:runtime");
    }

    #[test]
    fn from_principal_ersetzt_whitespace_und_faengt_leere_texte_auf() {
        assert_eq!(
            Actor::from_principal("  Rudi Dittrich  ").as_str(),
            "agent:Rudi-Dittrich"
        );
        assert_eq!(Actor::from_principal("").as_str(), "agent:unbekannt");
        assert_eq!(Actor::from_principal("   ").as_str(), "agent:unbekannt");
    }

    #[test]
    fn parse_lehnt_beinahe_treffer_auf_human_und_process_ab() {
        assert!(
            Actor::parse("Human:dana").is_none(),
            "falsche Groß-/Kleinschreibung"
        );
        assert!(
            Actor::parse("processfoo").is_none(),
            "kein Doppelpunkt nach 'process'"
        );
    }

    #[test]
    fn actor_serialisiert_als_blanker_string() {
        let actor = Actor::human("dana");
        let json = serde_json::to_string(&actor).unwrap();
        assert_eq!(json, "\"human:dana\"");
    }

    #[test]
    fn actor_deserialisiert_einen_blanken_legacy_string_normalisiert() {
        let actor: Actor = serde_json::from_str("\"tester\"").unwrap();
        assert_eq!(actor.as_str(), "agent:tester");

        // Ein bereits gültiger Actor bleibt beim Einlesen unverändert.
        let gueltig: Actor = serde_json::from_str("\"human:dana\"").unwrap();
        assert_eq!(gueltig.as_str(), "human:dana");
    }
}
