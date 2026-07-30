//! Das Domänenmodell: Projekt, Lauf, Work Item, Lease, Versuch, Artefakt.
//!
//! Zwei Festlegungen prägen alles Weitere (siehe Plan „Entschiedene Abweichungen“):
//!
//! 1. **`Ready` und `Blocked` sind abgeleitete Sichten, kein gespeicherter Status.**
//!    Gespeichert wird nur [`WorkItemStatus`]; `state::ready_items` und
//!    `state::blocked_by` rechnen bei jeder Abfrage neu — ein gespeichertes
//!    `Ready` könnte vom Abhängigkeitsgraphen abdriften, ein abgeleitetes nicht.
//! 2. **Zeit ist ein Parameter, keine Systemuhr im Domänencode.** `now_ms()` liefert
//!    die aktuelle Zeit für Aufrufer (Tools, Runner), aber Leases und Übergänge
//!    nehmen `_ms`-Werte entgegen, damit sie ohne Sleep deterministisch testbar sind.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub type ProjectId = String;
pub type RunId = String;
pub type WorkItemId = String;
pub type AttemptId = String;
pub type ArtifactId = String;

/// Unix-Zeit in Millisekunden. Reine Anzeige-/Erzeugungsquelle — Domänenfunktionen
/// nehmen Zeit immer als Parameter entgegen (siehe Moduldoku), damit Leases und
/// Statusübergänge ohne Sleep testbar bleiben.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sortierschlüssel für IDs der Form `W-17`: numerisch, nicht lexikografisch
/// (sonst stünde `W-10` vor `W-9`). Fällt auf 0 zurück, wenn kein Suffix da ist.
pub fn id_order(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Kebab-Slug eines Titels für die Projekt-ID (`"Graceful Swarm Shutdown"` →
/// `"graceful-swarm-shutdown"`). Kollisionen (`-2`, `-3`, …) löst der Aufrufer,
/// der die Zielverzeichnisse kennt — hier gibt es nur die reine Textfunktion.
pub fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "projekt".to_string()
    } else {
        out
    }
}

/// Wie belastbar ein Projekt insgesamt ist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Completed,
    Canceled,
}

/// Zustand eines Laufs (`WorkRun`). Ein Lauf zerfällt in Work Items; pausiert er,
/// bleibt der Fortschritt aller Items stehen — deshalb ist Pause reine
/// Buchführung ohne Kaskade auf die Items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Paused,
    Completed,
    Canceled,
}

/// Gespeicherter Status eines Work Items. `Ready` gibt es bewusst nicht (siehe
/// Moduldoku) — nur diese sechs werden je journalt.
///
/// Es gibt bewusst KEIN `Claimed` zwischen `Pending` und `Running`: bei einem
/// synchronen Ein-Prozess-Worker sind Claim und Start derselbe Moment, ein
/// eigener Zwischenzustand hätte keine beobachtbare Dauer. Ein früherer Entwurf
/// hatte `Claimed`, gekoppelt an die erste `LeaseRenewed` als Startsignal — das
/// war ein echter Bug: ein kurzer Versuch ohne jede Lease-Verlängerung (z. B.
/// ein Agent, der in einem einzigen Schritt antwortet) blieb auf `Claimed`
/// stehen, und `WorkItemCompleted` lief dann in den verbotenen Übergang
/// `Claimed → Completed`.
///
/// Aus derselben Überlegung gibt es `AwaitingVerification`, aber bewusst KEIN
/// `Verified`: `AwaitingVerification` hat echte Dauer — ein Human-Gate oder
/// ein automatisiertes Prüfkommando kann Sekunden bis Tage offen bleiben. Ein
/// `Verified`-Zwischenzustand hätte dagegen keine eigene Dauer: die Prüfung
/// schlägt entweder fehl (zurück nach `Pending`, ein neuer Versuch) oder das
/// Item ist fertig (`Completed`) — es gibt keinen dritten, sichtbar
/// verstreichenden Moment "verifiziert, aber noch nicht abgeschlossen".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    Pending,
    Running,
    /// Der Versuch selbst war erfolgreich; eine `VerificationPolicy` prüft ihn
    /// noch, bevor das Item als `Completed` gilt (§10 des Konzepts, Phase 5a).
    AwaitingVerification,
    Completed,
    Failed,
    Canceled,
}

impl WorkItemStatus {
    /// Die erlaubte Übergangsmatrix. `Completed` ist immer endgültig; `Failed`
    /// erlaubt den Rücksprung nach `Pending` (Retry) oder den Übergang nach
    /// `Canceled` (Lauf abgebrochen, bevor ein neuer Versuch startet).
    ///
    /// `AwaitingVerification` erreicht man nur aus `Running` (ein erfolgreicher
    /// Versuch, der noch geprüft werden muss) und verlässt es auf drei Wegen:
    /// `Completed` (Prüfung bestanden), `Pending` (Absturz mitten in der
    /// Prüfung — kein fachlicher Fehlschlag, siehe `recovery`) oder `Canceled`
    /// (Lauf abgebrochen, während ein Item auf Freigabe wartet). Zusätzlich
    /// erlaubt `(AwaitingVerification, Failed)`, obwohl das im Plan nicht als
    /// eigener Pfeil auftaucht: eine ABGELEHNTE Prüfung ist fachlich derselbe
    /// Fehlschlag-Mechanismus wie ein regulärer `WorkItemFailed`
    /// (`attempt_count` steigt, `recovery::finish_failed_attempt` entscheidet
    /// einheitlich, ob noch ein Versuch übrig ist) — nur der Ausgangszustand
    /// unterscheidet sich. Ohne diesen Übergang bräuchte es eine zweite,
    /// unabhängig gepflegte Kopie derselben "ist `max_attempts` erschöpft?"-
    /// Entscheidung.
    pub fn can_transition_to(self, next: WorkItemStatus) -> bool {
        use WorkItemStatus::*;
        matches!(
            (self, next),
            (Pending, Running)
                | (Pending, Canceled)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Pending)
                | (Running, Canceled)
                | (Running, AwaitingVerification)
                | (AwaitingVerification, Completed)
                | (AwaitingVerification, Failed)
                | (AwaitingVerification, Pending)
                | (AwaitingVerification, Canceled)
                | (Failed, Pending)
                | (Failed, Canceled)
        )
    }
}

/// Art eines Work Items — bestimmt u. a. die Rollenwahl beim Ausführen.
///
/// `Integration` (Phase 7, §19/§26 Phase 7) ist ein Sonderfall: die Laufzeit
/// legt es SELBST an, wenn Git-Isolation an ist und alle anderen Items eines
/// Laufs terminal sind — nie das Modell (`work_add_item` lehnt diesen Wert
/// ab, siehe `tools::register_work_tools`) und nie eine `--items`-Datei
/// (siehe `cli::load_items_file`). Es mergt die Item-Branches deterministisch
/// in den Ausgangsbranch (§31: „Deterministische Runtime, agentische
/// Problemlösung" — ein Merge ist Mechanik, keine fachliche Entscheidung).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    Discovery,
    Analysis,
    Planning,
    Implementation,
    Test,
    Review,
    Documentation,
    Integration,
}

impl WorkItemKind {
    /// Ob Versuche DIESES Items unter Git-Isolation (Phase 7, §19) einen
    /// eigenen Item-Branch bekommen — siehe `agentkit_work/README.md`
    /// Abschnitt „Git-Isolation" für die volle Begründung. Ausgenommen sind
    /// die rein lesenden Arten (`Review` bewertet nur, `Planning` zerlegt
    /// nur) sowie `Integration` selbst: es MERGT Branches, statt etwas zu
    /// produzieren, das seinerseits gemergt werden müsste.
    pub fn is_git_isolated(self) -> bool {
        !matches!(
            self,
            WorkItemKind::Review | WorkItemKind::Planning | WorkItemKind::Integration
        )
    }
}

/// Ergebnis eines einzelnen Versuchs, ein Item zu bearbeiten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

/// Warum ein Versuch gescheitert ist. Sechs Varianten, nicht elf (§18 im
/// Originaldokument) — nur diese sechs haben im MVP einen Erzeuger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    ModelFailure,
    MaxSteps,
    InvalidOutput,
    Interrupted,
    BudgetExceeded,
    /// Der Versuch selbst war erfolgreich, aber die `VerificationPolicy` hat
    /// ihn abgelehnt (automatisiertes Prüfkommando mit Exit ≠ 0, oder ein
    /// Mensch über `work reject`). Zählt gegen `max_attempts` wie jeder
    /// andere fachliche Fehlschlag (Phase 5a, §10/§18).
    VerificationFailure,
    /// Das automatische Integrations-Item (Phase 7, §19/§28) konnte einen
    /// Item-Branch nicht konfliktfrei mergen. Kein automatischer
    /// Auflösungsversuch (§28 nennt automatische Merges ausdrücklich nicht im
    /// Umfang) — das Integrations-Item bleibt `Failed`, der Lauf endet
    /// `Blocked` (siehe `scheduler::decide`).
    MergeConflict,
}

/// Ein gescheiterter Versuch trägt seine Ursache in den nächsten Anlauf weiter
/// (§12: „vorherige Fehlerursache im nächsten Arbeitspaket“).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInfo {
    pub kind: FailureKind,
    pub message: String,
}

/// Art eines Artefakts, das ein Versuch ablegt.
///
/// `GitCommit` (Phase 7, §9/§19) ist ein Sonderfall: es entsteht NUR in der
/// Laufzeit selbst (`runner::record_success`), nie über das Tool
/// `work_artifact` (dessen Schema diesen Wert bewusst nicht anbietet, siehe
/// `tools::ARTIFACT_KINDS`) — ein Modell könnte sonst einen Commit
/// behaupten, der nie stattgefunden hat. Siehe [`WorkArtifact`] für die
/// abweichende Feldbedeutung bei diesem Kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Analysis,
    Code,
    Test,
    Documentation,
    Other,
    GitCommit,
}

/// Warum ein Lauf beendet wurde.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    AllItemsDone,
    BudgetExceeded,
    Blocked,
    Canceled,
    /// Nichts ist mehr ausführbar, aber der Lauf ist auch nicht blockiert —
    /// mindestens ein Item wartet in `AwaitingVerification` auf eine
    /// menschliche Freigabe (`work approve`/`work reject`). Bewusst getrennt
    /// von `Blocked`: ein blockierter Lauf kommt nie mehr voran, ein
    /// wartender sehr wohl, sobald die Freigabe erteilt oder abgelehnt wird.
    AwaitingVerification,
}

/// Wie ein Work Item vor dem Abschluss geprüft wird (§10 des Konzepts, Phase
/// 5a/5b). Standard ist `None` — der Versuch selbst schließt das Item ab, wie
/// bisher.
///
/// `Composite` und `PeerReview` aus §10 sind bewusst NICHT enthalten: keine
/// der beiden hätte heute einen Erzeuger (Guidelines §4, YAGNI) —
/// `agentkit_work/README.md` begründet das im Abschnitt „Verifikation" im
/// Detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy {
    /// Der Regelfall: der Versuch selbst schließt das Item ab.
    #[default]
    None,
    /// Ein Kommando im Workspace muss mit Exit 0 durchlaufen.
    AutomatedTests { command: String },
    /// Ein UNABHÄNGIGER Agent prüft den Versuch (Phase 5b, §10/§26 Phase 6):
    /// die Laufzeit legt nach einem erfolgreichen Versuch automatisch ein
    /// eigenes Prüf-Item an (`runner::spawn_review_item`) — Kind `Review`,
    /// Rolle `reviewer`, und selbst `verification_policy: None`. Letzteres ist
    /// zwingend: ein Prüf-Item, das seinerseits geprüft werden müsste, wäre
    /// ein unendlicher Regress. Das Prüf-Item bekommt bewusst KEINE
    /// Abhängigkeit auf das geprüfte Item (siehe [`WorkItem::verifies`]) —
    /// Abhängigkeiten sind FinishToStart auf `Completed`
    /// (`state::ready_items`), das geprüfte Item steht zu diesem Zeitpunkt
    /// aber erst auf `AwaitingVerification`; eine Abhängigkeit würde das
    /// Prüf-Item dauerhaft unbereit machen. Es entsteht ohnehin erst, wenn die
    /// zu prüfende Arbeit schon vorliegt.
    IndependentAgent,
    /// Ein Mensch gibt frei (`agentkit work approve|reject`).
    HumanApproval,
}

/// Wer ein Work Item bearbeitet (§13 des Konzepts, Phase 6). Der Schwarm ist
/// eine kurzlebige Arbeitsphase für GENAU einen Versuch, kein Dauerzustand —
/// dieses Crate kennt `agentkit_swarm` deshalb weiterhin NICHT
/// (CLAUDE.md, Einbahnrichtung): dieses Feld ist nur die ENTSCHEIDUNG, wer
/// einen Versuch ausführt. Welche Vorlagen es gibt und wie aus einem Namen
/// tatsächlich ein Schwarm entsteht, weiß ausschließlich der komponierende
/// Executor in `agentkit_app` (`DispatchingExecutor`/`SwarmWorkExecutor`) —
/// dieses Crate validiert den Vorlagennamen nicht und braucht am Runner dafür
/// nichts zu ändern (`AgentExecutor` ist der Port, der das schon trägt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    /// Der Regelfall: ein einzelner `agentkit`-Agent bearbeitet den Versuch.
    #[default]
    SingleAgent,
    /// Name einer Schwarm-Vorlage (z. B. `"discovery"`, `"review"`); welche
    /// es gibt, weiß nur das Frontend.
    Swarm { template: String },
}

/// Ausgang einer Prüfung, wie sie an einem [`WorkAttempt`] hängen bleibt —
/// das Gegenstück zu `WorkAttempt::failure` für die Verifikationsebene: der
/// Versuch selbst kann `Succeeded` gewesen sein und trotzdem hier `Rejected`
/// tragen (die Prüfung ist eine ZWEITE, unabhängige Instanz, kein Teil des
/// Versuchs). `reason` ist bei `Approved` optional (CLI `approve --reason` ist
/// eine freiwillige Notiz des Bedieners), bei `Rejected` verpflichtend — ohne
/// Grund gäbe es nichts, das im nächsten Arbeitspaket unter den vorherigen
/// Fehlversuchen auftauchen könnte (§12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptVerification {
    Approved { by: String, reason: Option<String> },
    Rejected { by: String, reason: String },
}

/// Budget eines Projekts. `max_parallel_agents` ist im MVP fix 1 (ein
/// synchroner Vordergrund-Worker) — das Feld bleibt trotzdem Teil des Modells,
/// damit ein künftiger Scheduler nicht das Datenmodell brechen muss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkBudget {
    pub max_wall_time_secs: Option<u64>,
    pub max_work_items: Option<u32>,
    pub max_attempts_per_item: u32,
    pub max_steps_per_attempt: u32,
    pub max_parallel_agents: u32,
}

impl Default for WorkBudget {
    fn default() -> Self {
        WorkBudget {
            max_wall_time_secs: None,
            max_work_items: None,
            max_attempts_per_item: 3,
            max_steps_per_attempt: 40,
            max_parallel_agents: 1,
        }
    }
}

/// Ein Vorhaben — die Wurzel, unter der Läufe und Items liegen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkProject {
    pub id: ProjectId,
    pub title: String,
    pub objective: String,
    pub workspace: String,
    pub status: ProjectStatus,
    pub created_at_ms: u64,
    pub budget: WorkBudget,
    /// Ob jedes schreibende Item (`WorkItemKind::is_git_isolated`) einen
    /// eigenen Git-Branch bekommt (Phase 7, §19) — gesetzt über `agentkit
    /// work create --git-isolation`, NIE nachträglich änderbar (ein
    /// laufendes Vorhaben mitten im Lauf umzustellen hätte kein wohldefiniertes
    /// Verhalten). Default `false`: ein Vorhaben, das nicht in einem
    /// Git-Repository liegt, darf davon nichts merken — deshalb
    /// `#[serde(default)]`, damit ein Journal aus der Zeit vor Phase 7 weiter
    /// lesbar bleibt und sich unverändert verhält. Siehe
    /// `agentkit_work/README.md` Abschnitt „Git-Isolation" für den vollen
    /// Zuschnitt (ein Branch je Item statt eines echten Worktrees) und die
    /// Begründung.
    #[serde(default)]
    pub git_isolation: bool,
}

/// Ein Lauf eines Projekts. `base_revision` hält fest, auf welchem Stand des
/// Workspace (z. B. Git-Revision) der Lauf gestartet wurde — reine Anzeige im
/// MVP, ohne Worktree-Isolation (Phase 7, nicht im Umfang).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRun {
    pub id: RunId,
    pub project_id: ProjectId,
    pub status: RunStatus,
    pub started_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub base_revision: Option<String>,
    pub completion_reason: Option<CompletionReason>,
}

/// Eine Teilaufgabe. `seq` ist die Erzeugungsreihenfolge (numerisch, für
/// stabile Sortierung) — unabhängig von `id`, die als Anzeige-String dient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: WorkItemId,
    pub run_id: RunId,
    pub title: String,
    pub description: String,
    pub kind: WorkItemKind,
    pub status: WorkItemStatus,
    pub priority: u8,
    pub seq: u64,
    /// Name einer agentkit `AgentRole` — welche Rolle diesen Versuch ausführen soll.
    pub required_role: Option<String>,
    pub dependencies: Vec<WorkItemId>,
    pub acceptance_criteria: Vec<String>,
    /// Wie ein erfolgreicher Versuch dieses Items geprüft wird, bevor es
    /// `Completed` gilt (Phase 5a, §10). `#[serde(default)]`, damit ein
    /// Journal aus der Zeit vor Phase 5a (ohne dieses Feld) weiter lesbar
    /// bleibt — fehlt es, gilt `VerificationPolicy::None`, exakt das
    /// Verhalten von vorher.
    #[serde(default)]
    pub verification_policy: VerificationPolicy,
    /// NUR bei einem Prüf-Item gesetzt (Kind `Review`, aus
    /// `VerificationPolicy::IndependentAgent` erzeugt, siehe dort): die ID
    /// des Items, das dieses Prüf-Item begutachtet. Bestimmt zur Laufzeit, ob
    /// `tools::register_work_tools` das Tool `work_verdict` registriert
    /// (Fähigkeit entscheidet bei der Registrierung, nicht im Tool-Körper —
    /// Muster `work_claim`/`ctx.gateway`). `#[serde(default)]`, damit ein
    /// Journal aus der Zeit vor Phase 5b weiter lesbar bleibt.
    #[serde(default)]
    pub verifies: Option<WorkItemId>,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub updated_at_ms: u64,
    /// Ob die Claim-IDs aller Versuche dieses Items schon in den Canonical
    /// Graph promotet wurden (§11, Phase 5b) — nur relevant, wenn
    /// `verification_policy != None` (nur dann promotet die Laufzeit
    /// überhaupt, siehe `graph::promote_after_completion`). Verhindert, dass
    /// `recovery::recover_pending_promotions` bei JEDEM Resume erneut
    /// promotet, obwohl das beim vorherigen Lauf schon gelungen ist.
    /// `#[serde(default)]` aus demselben Grund wie `verifies`.
    #[serde(default)]
    pub claims_promoted: bool,
    /// Wer diesen Versuch ausführt (Phase 6, §13) — gesetzt beim Anlegen
    /// (`--items`-Datei/CLI), NIE über `work_add_item` (siehe
    /// `tools::register_work_tools`-Doku: ein Modell, das sich selbst einen
    /// Schwarm verordnet, wäre die Eskalation, die diese Laufzeit gerade
    /// deterministisch halten soll). `#[serde(default)]`, damit ein Journal
    /// aus der Zeit vor Phase 6 (ohne dieses Feld) weiter lesbar bleibt —
    /// fehlt es, gilt `ExecutorKind::SingleAgent`, exakt das Verhalten von
    /// vorher.
    #[serde(default)]
    pub executor: ExecutorKind,
}

/// Der exklusive Anspruch eines Agenten auf ein Item, solange das Lease läuft.
/// Läuft es ab, gilt der Agent als tot — `state::expired_leases` findet genau das.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkLease {
    pub work_item_id: WorkItemId,
    pub agent_id: String,
    pub attempt_id: AttemptId,
    pub claimed_at_ms: u64,
    pub expires_at_ms: u64,
    pub last_heartbeat_ms: u64,
}

/// Ein einzelner Anlauf, ein Item zu bearbeiten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAttempt {
    pub id: AttemptId,
    pub work_item_id: WorkItemId,
    pub agent_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: AttemptStatus,
    pub summary: Option<String>,
    pub failure: Option<FailureInfo>,
    pub steps: u32,
    pub tool_calls: u32,
    /// IDs der über `work_claim` festgehaltenen Aussagen (Phase 4, §11/§14).
    /// Wächst nur — `state::apply` HÄNGT bei `ClaimsRecorded` an, weil ein
    /// Versuch `work_claim` mehrfach aufrufen darf, jeder Aufruf journalt
    /// aber ein eigenes Ereignis mit genau seinen neuen IDs.
    #[serde(default)]
    pub claim_ids: Vec<String>,
    /// Ausgang der Verifikation dieses Versuchs (Phase 5a) — `None`, solange
    /// keine `VerificationPolicy` etwas anderes als `None` verlangt, oder
    /// solange die Prüfung noch aussteht. `#[serde(default)]` aus demselben
    /// Grund wie bei `WorkItem::verification_policy`.
    #[serde(default)]
    pub verification: Option<AttemptVerification>,
}

/// Ein Artefakt, das ein Versuch abgelegt hat. Liegt bewusst im Workspace
/// (`<workspace>/.agentkit/work/<project-id>/artifacts/<item>/<versuch>/<datei>`),
/// damit der nächste Agent es mit dem vorhandenen `read_file`-Tool erreicht.
/// Der Versuch steckt mit im Pfad, nicht nur das Item — sonst würde ein
/// Wiederholungsversuch beim erneuten Ablegen desselben Dateinamens auf die
/// Datei seines Vorgängers treffen (siehe `tools::resolve_artifact_path`).
///
/// Ausnahme `ArtifactKind::GitCommit` (Phase 7, §9/§19): dieses Artefakt
/// entsteht NICHT über `work_artifact`, also gibt es dafür keine Datei. Statt
/// `rel_path` still auf einen Dateipfad umzudeuten, trägt es dort den Namen
/// des Item-Branches (`work/<projekt>/<item>`) — informativ, aber KEIN Pfad,
/// den `read_file` je auflösen könnte — und die tatsächliche Commit-ID steht
/// im eigenen Feld [`WorkArtifact::commit_id`]. `AgentWorkPackage::build`
/// schließt `GitCommit`-Artefakte deshalb explizit aus der Liste der
/// Vorgänger-Artefakte aus (die dort als „mit 'read_file' lesen" angekündigt
/// werden) — ein Branchname wäre dort irreführend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkArtifact {
    pub id: ArtifactId,
    pub work_item_id: WorkItemId,
    pub attempt_id: AttemptId,
    pub kind: ArtifactKind,
    pub rel_path: String,
    pub summary: String,
    pub created_at_ms: u64,
    /// NUR bei `kind == GitCommit` gesetzt: die tatsächliche Commit-ID (siehe
    /// Typdoku oben). `#[serde(default)]`, damit ein Journal aus der Zeit vor
    /// Phase 7 weiter lesbar bleibt.
    #[serde(default)]
    pub commit_id: Option<String>,
}

impl fmt::Display for WorkItemStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WorkItemStatus::Pending => "pending",
            WorkItemStatus::Running => "running",
            WorkItemStatus::AwaitingVerification => "awaiting_verification",
            WorkItemStatus::Completed => "completed",
            WorkItemStatus::Failed => "failed",
            WorkItemStatus::Canceled => "canceled",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalisiert_und_kollabiert_trenner() {
        assert_eq!(slug("Graceful Swarm Shutdown"), "graceful-swarm-shutdown");
        assert_eq!(slug("  A -- B  "), "a-b");
        assert_eq!(slug("Größe & Öko"), "gr-e-ko");
        assert_eq!(slug("---"), "projekt");
        assert_eq!(slug(""), "projekt");
    }

    #[test]
    fn id_order_sortiert_numerisch() {
        let mut ids = vec!["W-10".to_string(), "W-9".to_string(), "W-2".to_string()];
        ids.sort_by_key(|i| id_order(i));
        assert_eq!(ids, vec!["W-2", "W-9", "W-10"]);
    }

    #[test]
    fn erlaubte_und_verbotene_uebergaenge() {
        use WorkItemStatus::*;
        assert!(Pending.can_transition_to(Running));
        assert!(Running.can_transition_to(Completed));
        assert!(Failed.can_transition_to(Pending));
        assert!(Failed.can_transition_to(Canceled));

        assert!(!Completed.can_transition_to(Pending));
        assert!(!Completed.can_transition_to(Running));
        assert!(!Canceled.can_transition_to(Pending));
        assert!(!Pending.can_transition_to(Completed));
    }

    /// Phase 5a: die vier im Konzept genannten Übergänge, plus der fünfte,
    /// deliberate hinzugefügte (`AwaitingVerification -> Failed`, siehe
    /// Doc-Kommentar an `can_transition_to`) für eine abgelehnte Prüfung.
    #[test]
    fn awaiting_verification_uebergaenge() {
        use WorkItemStatus::*;
        assert!(Running.can_transition_to(AwaitingVerification));
        assert!(AwaitingVerification.can_transition_to(Completed));
        assert!(AwaitingVerification.can_transition_to(Pending));
        assert!(AwaitingVerification.can_transition_to(Canceled));
        assert!(AwaitingVerification.can_transition_to(Failed));

        assert!(!Pending.can_transition_to(AwaitingVerification));
        assert!(!AwaitingVerification.can_transition_to(Running));
        assert!(!Completed.can_transition_to(AwaitingVerification));
    }

    #[test]
    fn awaiting_verification_zeigt_sich_im_display_als_snake_case() {
        assert_eq!(
            WorkItemStatus::AwaitingVerification.to_string(),
            "awaiting_verification"
        );
    }

    #[test]
    fn verification_policy_default_ist_none() {
        assert_eq!(VerificationPolicy::default(), VerificationPolicy::None);
    }

    #[test]
    fn executor_kind_default_ist_single_agent() {
        assert_eq!(ExecutorKind::default(), ExecutorKind::SingleAgent);
    }

    /// Journal-Repräsentation von `ExecutorKind` (§13, Phase 6): der
    /// Unit-Variante entspricht ein reiner String, der Struct-Variante ein
    /// verschachteltes Objekt — dieselbe Standard-Serde-Form wie
    /// `VerificationPolicy`. Das `--items`-Wire-Format (`{"swarm": "review"}`,
    /// flach) ist bewusst eine ANDERE, separate Repräsentation (siehe
    /// `cli::ExecutorField`) — Journal und Nutzerschnittstelle dürfen
    /// auseinanderlaufen, wie bei `VerificationField`.
    #[test]
    fn executor_kind_serialisiert_und_deserialisiert_im_journal_format() {
        assert_eq!(
            serde_json::to_value(ExecutorKind::SingleAgent).unwrap(),
            serde_json::json!("single_agent")
        );
        let swarm = ExecutorKind::Swarm {
            template: "review".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&swarm).unwrap(),
            serde_json::json!({"swarm": {"template": "review"}})
        );
        let back: ExecutorKind =
            serde_json::from_value(serde_json::json!({"swarm": {"template": "review"}})).unwrap();
        assert_eq!(back, swarm);
        let back_single: ExecutorKind =
            serde_json::from_value(serde_json::json!("single_agent")).unwrap();
        assert_eq!(back_single, ExecutorKind::SingleAgent);
    }
}
