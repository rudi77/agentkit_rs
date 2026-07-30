# agentkit-work – Persistente Runtime für langfristige Agentenarbeit

## 1. Ziel

`agentkit-work` ist die Runtime für Aufgaben, die nicht zuverlässig innerhalb eines einzelnen Agent-Laufs oder eines einzelnen Swarm-Laufs abgeschlossen werden können.

Das Crate verwaltet langlebige Arbeitsvorhaben wie:

* mehrstündige Repository-Analysen,
* größere Feature-Implementierungen,
* Refactorings über mehrere Module,
* Migrationen,
* Test- und Review-Kampagnen,
* Fehleranalysen mit mehreren Hypothesen,
* wiederaufnehmbare Agenten-Workflows,
* Arbeiten mit mehreren Agenten, Rollen und Freigaben.

Der zentrale Grundsatz lautet:

> Nicht ein Agent muss stundenlang leben. Der Arbeitszustand muss stundenlang leben.

Agenten und Swarms dürfen kurzlebig sein. `agentkit-work` bewahrt den Projektfortschritt, offene Aufgaben, Artefakte, Entscheidungen und Prüfungen dauerhaft auf.

---

# 2. Motivation

Ein normaler Agent-Lauf besitzt typischerweise einen begrenzten Lebenszyklus:

```text
Auftrag
  → Modellaufruf
  → Tool-Aufruf
  → weitere Modellaufrufe
  → Ergebnis
```

Ein Swarm erweitert diesen Ablauf:

```text
Auftrag
  → mehrere Agenten
  → Nachrichten
  → Delegation
  → Vorschlag
  → Konsens
  → Ergebnis
```

Für große Aufgaben reicht das nicht aus. Mehrstündige Arbeit benötigt zusätzlich:

* persistente Aufgaben,
* Wiederaufnahme nach einem Prozessabbruch,
* verlässliche Fortschrittsmessung,
* Abhängigkeiten zwischen Aufgaben,
* Wiederholungsversuche,
* Reviews und Verifikation,
* begrenzte Agenten-Laufzeiten,
* Budgetkontrolle,
* Versionierung der Arbeitsergebnisse,
* nachvollziehbare Entscheidungen,
* dauerhafte Verbindung zum Wissensgraphen.

`agentkit-work` bildet diese langlebige Ebene.

---

# 3. Position innerhalb von agentkit

Die Komponenten erhalten klar getrennte Verantwortlichkeiten.

```text
agentkit
    Einzelner Agent
    Agent-Loop
    Tools
    Skills
    MCP
    Modellzugriff

ctxman
    Kontext eines einzelnen Agenten
    Segmente
    Frames
    Externalisierung
    Komprimierung
    Context-Resume

agentkit-swarm
    Zusammenarbeit mehrerer Agenten
    Actor-Modell
    Mailboxen
    Topologien
    Peer-Kommunikation
    Vorschläge und Abstimmungen

agentkit-graph
    Langzeitwissen
    Working Graph
    Canonical Graph
    Claims
    Provenance
    Evidenz
    Wissens-Promotion

agentkit-work
    Langlebige Arbeitsvorhaben
    Work Items
    Abhängigkeiten
    Leases
    Checkpoints
    Verifikation
    Retry
    Budgets
    Run-Lifecycle

agentkit-app
    CLI
    TUI
    Konfiguration
    Verdrahtung der Komponenten
```

## Abgrenzung

### agentkit-work ist kein Agent

Es trifft keine fachlichen Entscheidungen und löst selbst keine Aufgaben.

### agentkit-work ist kein semantischer Orchestrator

Es schreibt den Agenten nicht vor, wie sie ein Problem lösen müssen.

### agentkit-work ist kein Langzeitgedächtnis

Wissen, Behauptungen und Entscheidungen gehören in `agentkit-graph`.

### agentkit-work ist kein Context Manager

Der Kontext einzelner Agenten bleibt Aufgabe von `ctxman`.

### agentkit-work ist eine Arbeitszustands-Runtime

Es beantwortet Fragen wie:

* Welche Aufgabe ist offen?
* Welche Aufgabe wird gerade bearbeitet?
* Von welchem Agenten?
* Welche Aufgaben hängen voneinander ab?
* Welche Aufgabe ist blockiert?
* Welche Ergebnisse wurden erzeugt?
* Wurde das Ergebnis geprüft?
* Darf die Aufgabe abgeschlossen werden?
* Kann eine abgebrochene Arbeit wieder aufgenommen werden?

---

# 4. Wann agentkit-work verwendet werden soll

`agentkit-work` sollte verwendet werden, wenn mindestens eines der folgenden Kriterien erfüllt ist.

## 4.1 Lange Laufzeit

Die erwartete Arbeit dauert länger als einen normalen Agent-Lauf, beispielsweise länger als 20 bis 30 Minuten.

## 4.2 Mehrere Teilaufgaben

Der Auftrag lässt sich in mehrere weitgehend getrennte Aufgaben zerlegen.

```text
Feature analysieren
Implementierung planen
Code ändern
Tests ergänzen
Review durchführen
Dokumentation aktualisieren
```

## 4.3 Abhängigkeiten

Einzelne Aufgaben können erst beginnen, nachdem andere abgeschlossen wurden.

```text
Architektur analysieren
    ↓
Implementierungsplan erstellen
    ↓
Code ändern
    ↓
Tests und Review
```

## 4.4 Wiederaufnahme

Die Arbeit muss nach einem Neustart, Fehler oder manuellen Abbruch fortgesetzt werden können.

## 4.5 Mehrere Agenten oder Swarms

Verschiedene Rollen arbeiten gemeinsam oder nacheinander an einem Problem.

## 4.6 Verifikation

Das Ergebnis eines Agenten muss von einem anderen Agenten, einem Test oder einem Menschen geprüft werden.

## 4.7 Große Anzahl von Artefakten

Der Auftrag erzeugt mehrere Dateien, Patches, Reports, Commits oder Testresultate.

## 4.8 Kontrollierte Budgets

Die Arbeit benötigt Grenzen für:

* Laufzeit,
* Modellaufrufe,
* Token,
* Kosten,
* Tool-Aufrufe,
* Wiederholungen,
* parallele Agenten.

---

# 5. Wann agentkit-work nicht verwendet werden soll

Für kurze, direkte Aufgaben wäre `agentkit-work` unnötiger Overhead.

Beispiele:

* eine einzelne Datei erklären,
* eine kleine Funktion schreiben,
* einen Fehler anhand eines Logs analysieren,
* eine kurze Repository-Zusammenfassung,
* einen einzelnen Test ergänzen,
* eine Frage beantworten,
* eine kleine Änderung mit klarer Lösung durchführen.

Für solche Fälle bleibt der normale `agentkit`-Agent beziehungsweise ein einzelner Swarm ausreichend.

Eine mögliche Auswahlregel:

```text
Ein klarer Auftrag, ein Agent, wenige Minuten
    → agentkit

Mehrere Perspektiven, kurze Zusammenarbeit
    → agentkit-swarm

Mehrere Arbeitsschritte, Resume, Verifikation oder lange Laufzeit
    → agentkit-work
```

---

# 6. Zentrale Domänenobjekte

## 6.1 Work Project

Ein `WorkProject` beschreibt das langfristige Vorhaben.

```rust
pub struct WorkProject {
    pub id: ProjectId,
    pub title: String,
    pub objective: String,
    pub workspace: WorkspaceRef,
    pub status: ProjectStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: PrincipalId,
    pub policy: ProjectPolicy,
    pub budget: WorkBudget,
}
```

Beispiele:

```text
agentkit_swarm Graceful Shutdown implementieren
agentkit_app CLI modularisieren
OpenAPI-Client auf neue API-Version migrieren
Repository vollständig dokumentieren
```

## 6.2 Work Run

Ein Projekt kann mehrere Ausführungen besitzen.

```rust
pub struct WorkRun {
    pub id: RunId,
    pub project_id: ProjectId,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub base_revision: Option<String>,
    pub current_revision: Option<String>,
    pub completion_reason: Option<CompletionReason>,
    pub checkpoint_id: Option<CheckpointId>,
}
```

Ein neuer Run kann entstehen durch:

* erstmaligen Start,
* Wiederaufnahme,
* erneuten Versuch,
* neue Projektphase,
* Ausführung auf einem anderen Branch.

## 6.3 Work Item

Das `WorkItem` ist die zentrale ausführbare Einheit.

```rust
pub struct WorkItem {
    pub id: WorkItemId,
    pub run_id: RunId,
    pub title: String,
    pub description: String,

    pub kind: WorkItemKind,
    pub status: WorkItemStatus,
    pub priority: Priority,

    pub required_role: Option<AgentRole>,
    pub assigned_agent: Option<AgentId>,

    pub dependencies: Vec<WorkItemId>,
    pub attempt_count: u32,
    pub max_attempts: u32,

    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    pub verification_policy: VerificationPolicy,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

Mögliche Arten:

```rust
pub enum WorkItemKind {
    Discovery,
    Analysis,
    Planning,
    Implementation,
    Test,
    Review,
    Documentation,
    Integration,
    Decision,
    HumanApproval,
}
```

## 6.4 Work-Item-Zustände

```rust
pub enum WorkItemStatus {
    Draft,
    Ready,
    Claimed,
    Running,
    AwaitingVerification,
    Verified,
    Completed,
    Blocked,
    Failed,
    Canceled,
}
```

Typischer Ablauf:

```text
Draft
  → Ready
  → Claimed
  → Running
  → AwaitingVerification
  → Verified
  → Completed
```

Fehlerpfade:

```text
Running
  → Failed
  → Ready

Running
  → Blocked

AwaitingVerification
  → Ready
```

Eine abgelehnte Prüfung führt die Aufgabe normalerweise zurück nach `Ready` oder `Running`.

## 6.5 Abhängigkeiten

Abhängigkeiten werden explizit modelliert.

```rust
pub struct WorkDependency {
    pub predecessor: WorkItemId,
    pub successor: WorkItemId,
    pub dependency_type: DependencyType,
}
```

Mögliche Typen:

```rust
pub enum DependencyType {
    FinishToStart,
    RequiresArtifact,
    RequiresDecision,
    RequiresVerification,
}
```

Eine Aufgabe wird erst `Ready`, wenn alle notwendigen Abhängigkeiten erfüllt sind.

---

# 7. Agenten-Zuweisung und Leases

Ein Agent soll ein Work Item nicht dauerhaft besitzen. Er erhält ein zeitlich begrenztes Lease.

```rust
pub struct WorkLease {
    pub id: LeaseId,
    pub work_item_id: WorkItemId,
    pub agent_id: AgentId,
    pub claimed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
}
```

## Warum Leases benötigt werden

Ohne Lease bleibt eine Aufgabe möglicherweise dauerhaft blockiert, wenn:

* ein Agent abstürzt,
* ein Modellaufruf hängen bleibt,
* der Prozess beendet wird,
* eine Netzwerkverbindung ausfällt,
* ein Worker nicht mehr erreichbar ist.

Bei abgelaufenem Lease kann die Runtime:

1. den Versuch als unterbrochen markieren,
2. Diagnoseinformationen speichern,
3. die Aufgabe wieder auf `Ready` setzen,
4. einen neuen Agenten starten.

## Heartbeats

Länger laufende Agenten aktualisieren regelmäßig ihr Lease.

```text
Agent übernimmt W17
Lease: 10 Minuten
Heartbeat: alle 30 Sekunden
```

Der Heartbeat bedeutet nur, dass der Worker lebt. Er ist kein Beweis für fachlichen Fortschritt.

---

# 8. Work Attempts

Jeder Bearbeitungsversuch wird separat protokolliert.

```rust
pub struct WorkAttempt {
    pub id: AttemptId,
    pub work_item_id: WorkItemId,
    pub agent_id: AgentId,
    pub swarm_id: Option<SwarmId>,

    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,

    pub status: AttemptStatus,
    pub input_snapshot: ContextSnapshotRef,
    pub result: Option<WorkResult>,
    pub failure: Option<FailureInfo>,

    pub model_usage: ModelUsage,
    pub tool_usage: ToolUsage,
}
```

Dadurch bleibt nachvollziehbar:

* welcher Agent etwas versucht hat,
* welche Eingaben er erhielt,
* welches Modell verwendet wurde,
* welche Tools ausgeführt wurden,
* warum der Versuch scheiterte,
* welche Artefakte entstanden.

---

# 9. Ergebnisse und Artefakte

Ein Work Item sollte nicht nur eine Textantwort zurückgeben.

```rust
pub struct WorkResult {
    pub summary: String,
    pub artifacts: Vec<ArtifactRef>,
    pub claims: Vec<GraphClaimRef>,
    pub created_work_items: Vec<WorkItemId>,
    pub decisions: Vec<DecisionRef>,
    pub metrics: ResultMetrics,
}
```

Mögliche Artefakte:

```rust
pub enum ArtifactKind {
    File,
    Patch,
    GitCommit,
    TestReport,
    AnalysisReport,
    Plan,
    Review,
    Log,
    BuildOutput,
    DecisionRecord,
}
```

Beispiele:

```text
artifacts/W17/analysis.md
artifacts/W18/change.patch
artifacts/W18/test-results.json
artifacts/W19/review.json
git commit abc123
```

## Artefakte statt langer Nachrichten

Agent-to-Agent-Nachrichten sollen nicht die vollständige Arbeit enthalten.

Stattdessen:

```text
Agent A:
Analyse abgeschlossen.
Artefakt: artifact://run-123/W17/architecture-report
Claims: C41, C42, C43
```

Das reduziert Kontextverbrauch und erleichtert die Wiederaufnahme.

---

# 10. Verifikation

Ein Agent darf eine risikoreiche Aufgabe nicht allein als abgeschlossen erklären.

## Verifikationsstatus

```rust
pub enum VerificationStatus {
    NotRequired,
    Pending,
    Running,
    Approved,
    Rejected,
}
```

## Verifikationsrichtlinien

```rust
pub enum VerificationPolicy {
    None,
    AutomatedTests,
    PeerReview,
    IndependentAgent,
    HumanApproval,
    Composite(Vec<VerificationRequirement>),
}
```

Beispiele:

```text
Dokumentation:
    keine zusätzliche Prüfung

Codeänderung:
    Tests + Peer Review

Architekturänderung:
    unabhängiger Architektur-Review

Sicherheitsrelevante Änderung:
    Tests + zwei Reviews + Human Approval
```

## Acceptance Criteria

Jedes Work Item sollte überprüfbare Kriterien besitzen.

```text
- cargo test läuft erfolgreich
- keine öffentliche API wurde verändert
- neuer Quiescing-Zustand ist getestet
- RecipientGone tritt beim normalen Shutdown nicht mehr auf
- Dokumentation beschreibt das Lifecycle-Modell
```

Die Verifikation prüft diese Kriterien und nicht nur eine freie Selbsteinschätzung.

---

# 11. Verbindung zu agentkit-graph

`agentkit-work` und `agentkit-graph` ergänzen sich.

## agentkit-work speichert operative Wahrheit

```text
Work Item W17 ist offen.
Agent developer-2 bearbeitet W17.
W17 wartet auf Review.
W18 ist durch W17 blockiert.
```

## agentkit-graph speichert fachliches Wissen

```text
CompletionPolicy kann laufende Streams abbrechen.
Ein Quiescing-Zustand verhindert verlorene Abschlussmeldungen.
Die Stern-Topologie begrenzt Peer-Capabilities.
```

## Verknüpfung

Jeder Claim sollte auf seinen Arbeitskontext verweisen können:

```rust
pub struct WorkProvenance {
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub work_item_id: WorkItemId,
    pub attempt_id: AttemptId,
    pub agent_id: AgentId,
    pub artifact_ids: Vec<ArtifactId>,
    pub repository_revision: Option<String>,
}
```

## Working Graph

Während eines Projekts entstehen vorläufige Claims:

* Hypothesen,
* Beobachtungen,
* offene Fragen,
* mögliche Risiken,
* fehlgeschlagene Ansätze.

Diese gelangen in den Working Graph.

## Canonical Graph

Nach Review oder Verifikation können relevante Claims in den Canonical Graph promotet werden.

```text
Work Item abgeschlossen
    ↓
Claims geprüft
    ↓
akzeptierte Claims promoten
    ↓
spätere Agenten können Wissen wiederverwenden
```

---

# 12. Verbindung zu ctxman

`ctxman` verwaltet den Kontext eines einzelnen Agenten. `agentkit-work` entscheidet, welcher Auftrag und welche Artefakte für den Agenten relevant sind.

Vor dem Start eines Agenten erstellt `agentkit-work` ein Input Package:

```rust
pub struct AgentWorkPackage {
    pub work_item: WorkItem,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    pub relevant_artifacts: Vec<ArtifactRef>,
    pub relevant_claims: Vec<GraphClaimRef>,
    pub relevant_decisions: Vec<DecisionRef>,
    pub workspace: WorkspaceRef,
    pub budget: AttemptBudget,
}
```

`ctxman` rendert daraus einen kontrollierten Agentenkontext.

Der Agent muss nicht die gesamte Projektgeschichte erhalten, sondern nur:

* den aktuellen Auftrag,
* relevante Entscheidungen,
* bekannte Constraints,
* vorherige fehlgeschlagene Ansätze,
* betroffene Artefakte,
* notwendige Graph-Claims.

---

# 13. Verbindung zu agentkit-swarm

Ein Work Item kann entweder von einem einzelnen Agenten oder einem Swarm bearbeitet werden.

```rust
pub enum ExecutorKind {
    SingleAgent,
    Swarm(SwarmTemplateId),
}
```

Beispiele:

```text
W1 Repository analysieren
    → Architektur-Swarm

W2 kleine Funktion implementieren
    → einzelner Coding-Agent

W3 Sicherheitsreview
    → Review-Swarm

W4 Konflikt auflösen
    → Diskussions- oder Konsens-Swarm
```

## Wichtige Abgrenzung

Der Swarm bearbeitet eine begrenzte Arbeitsphase.

`agentkit-work` verwaltet das gesamte langfristige Vorhaben.

```text
agentkit-work
    startet Swarm A für Discovery
    speichert Ergebnis
    beendet Swarm A

agentkit-work
    startet Agent B für Implementierung
    speichert Patch
    beendet Agent B

agentkit-work
    startet Swarm C für Review
    speichert Abstimmung
    beendet Swarm C
```

Swarms sollten nicht über mehrere Stunden permanent aktiv bleiben müssen.

---

# 14. Event-Modell

Alle wichtigen Zustandsänderungen sollten als Events persistiert werden.

```rust
pub enum WorkEvent {
    ProjectCreated,
    RunStarted,
    WorkItemCreated,
    WorkItemReady,
    WorkItemClaimed,
    WorkItemStarted,
    HeartbeatReceived,
    ArtifactCreated,
    ClaimRecorded,
    WorkItemSubmittedForVerification,
    VerificationStarted,
    VerificationApproved,
    VerificationRejected,
    WorkItemBlocked,
    WorkItemFailed,
    WorkItemRetried,
    WorkItemCompleted,
    CheckpointCreated,
    RunPaused,
    RunResumed,
    RunCompleted,
    RunCanceled,
}
```

## Vorteile

* vollständige Auditierbarkeit,
* Wiederherstellung des Zustands,
* Debugging,
* Timeline in der TUI,
* Metriken,
* spätere Analyse von Agentenverhalten,
* Replay von Fehlerfällen.

Der aktuelle Zustand kann zusätzlich materialisiert gespeichert werden. Das Event Log bleibt die nachvollziehbare Historie.

---

# 15. Checkpoints und Resume

Ein Checkpoint speichert den wiederaufnehmbaren Projektzustand.

```rust
pub struct WorkCheckpoint {
    pub id: CheckpointId,
    pub run_id: RunId,
    pub created_at: DateTime<Utc>,
    pub sequence_number: u64,

    pub active_work_items: Vec<WorkItemSnapshot>,
    pub active_leases: Vec<LeaseSnapshot>,
    pub budget_state: BudgetState,
    pub workspace_revision: Option<String>,
    pub graph_revision: Option<String>,
    pub artifact_index_revision: Option<String>,
}
```

## Checkpoint-Auslöser

* periodisch,
* nach Abschluss eines Work Items,
* vor größeren Schreiboperationen,
* nach einem Git-Commit,
* vor dem Pausieren,
* bei Annäherung an ein Budgetlimit,
* vor einem Runtime-Upgrade.

## Resume-Ablauf

```text
Runtime startet
    ↓
letzten gültigen Checkpoint laden
    ↓
Event Log nach Checkpoint wiedergeben
    ↓
abgelaufene Leases erkennen
    ↓
unterbrochene Attempts markieren
    ↓
Work Items erneut freigeben
    ↓
Arbeit fortsetzen
```

---

# 16. Scheduler

Der Scheduler entscheidet mechanisch, welche bereitstehenden Aufgaben ausgeführt werden können.

Er trifft keine fachlichen Entscheidungen.

## Verantwortlichkeiten

* `Ready` Work Items finden,
* Abhängigkeiten prüfen,
* Prioritäten berücksichtigen,
* Rollenanforderungen beachten,
* Parallelitätslimits einhalten,
* Budgets prüfen,
* Agent oder Swarm starten,
* Leases vergeben,
* abgelaufene Leases behandeln.

## Mögliche Auswahlstrategie

```text
1. höchste Priorität
2. kritischer Pfad
3. ältestes Work Item
4. verfügbare Rolle
5. geringstes erwartetes Budget
```

Für Version 1 reicht zunächst:

```text
Priorität → Erstellungszeit
```

---

# 17. Budgets

Budgets müssen auf mehreren Ebenen existieren.

```rust
pub struct WorkBudget {
    pub max_wall_time: Option<Duration>,
    pub max_model_calls: Option<u64>,
    pub max_tokens: Option<u64>,
    pub max_tool_calls: Option<u64>,
    pub max_cost: Option<Decimal>,
    pub max_parallel_agents: usize,
    pub max_work_items: Option<u32>,
    pub max_attempts_per_item: u32,
}
```

Zusätzlich:

* Projektbudget,
* Run-Budget,
* Work-Item-Budget,
* Attempt-Budget,
* Swarm-Budget.

## Verhalten bei Budgetgrenzen

```text
Soft Limit:
    keine neuen optionalen Aufgaben erzeugen

Hard Limit:
    keine neuen Attempts starten

Emergency Limit:
    aktive Arbeit kontrolliert stoppen
    Checkpoint schreiben
    Run pausieren
```

---

# 18. Fehlerbehandlung

Fehler müssen klassifiziert werden.

```rust
pub enum FailureKind {
    ModelFailure,
    RateLimit,
    ToolFailure,
    Timeout,
    AgentCrash,
    ProcessCrash,
    InvalidOutput,
    VerificationFailure,
    MergeConflict,
    BudgetExceeded,
    HumanRejected,
}
```

## Retry-Policy

```rust
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub retryable_failures: Vec<FailureKind>,
    pub backoff: BackoffPolicy,
    pub change_agent_on_retry: bool,
    pub change_model_on_retry: bool,
}
```

Ein fachlich falsches Ergebnis sollte anders behandelt werden als ein temporäres Rate Limit.

---

# 19. Schreibzugriffe auf Repositories

Für parallele Codearbeit benötigt `agentkit-work` eine sichere Workspace-Strategie.

## Empfohlene Variante: Git Worktrees

Jedes schreibende Work Item erhält einen eigenen Worktree.

```text
.work/trees/W17
.work/trees/W18
.work/trees/W19
```

Der Agent erzeugt einen Commit:

```text
work/W17 → commit abc123
```

Nach Verifikation wird der Commit durch einen Integrationsschritt übernommen.

## Vorteile

* isolierte Schreibzugriffe,
* weniger Race Conditions,
* einfacher Rollback,
* klare Provenance,
* parallele Agenten,
* Reviews auf Commit-Ebene.

## Alternative für Version 1

Zunächst kann die Runtime nur einen schreibenden Agenten gleichzeitig erlauben.

```text
max_parallel_writers = 1
```

Lesende Agenten dürfen weiterhin parallel arbeiten.

---

# 20. Human-in-the-Loop

Bestimmte Work Items können menschliche Entscheidungen benötigen.

```rust
pub struct HumanGate {
    pub id: GateId,
    pub work_item_id: WorkItemId,
    pub question: String,
    pub options: Vec<String>,
    pub status: HumanGateStatus,
}
```

Beispiele:

* Architekturentscheidung freigeben,
* riskanten Patch akzeptieren,
* Kostenlimit erhöhen,
* widersprüchliche Anforderungen klären,
* Deployment erlauben.

Währenddessen wird das Work Item auf `Blocked` oder `AwaitingHumanApproval` gesetzt.

Andere unabhängige Work Items können weiterlaufen.

---

# 21. CLI-Konzept

## Projekt erstellen

```bash
agentkit work create \
  --title "Graceful Swarm Shutdown" \
  --objective "Implementiere einen kontrollierten Swarm-Shutdown mit Tests"
```

## Projekt starten

```bash
agentkit work run <project-id>
```

## Status anzeigen

```bash
agentkit work status <project-id>
```

## Work Items anzeigen

```bash
agentkit work items <project-id>
```

## Projekt fortsetzen

```bash
agentkit work resume <project-id>
```

## Projekt pausieren

```bash
agentkit work pause <project-id>
```

## Einzelnes Work Item wiederholen

```bash
agentkit work retry <work-item-id>
```

## Timeline anzeigen

```bash
agentkit work events <project-id>
```

## JSON-Ausgabe

```bash
agentkit work status <project-id> --format json
```

Bei `--format json` muss exakt ein gültiges JSON-Dokument auf `stdout` ausgegeben werden. Logs und Fortschrittsereignisse gehören nach `stderr` oder in einen separaten Event-Stream.

---

# 22. TUI-Konzept

Die TUI könnte folgende Bereiche besitzen:

```text
┌──────────────────────────────────────────────────────────┐
│ Project: Graceful Swarm Shutdown              RUNNING    │
├──────────────────┬───────────────────────────────────────┤
│ Work Items       │ Selected Work Item                    │
│                  │                                       │
│ ✓ Discovery      │ W17 Implement Quiescing State         │
│ ✓ Architecture   │ Status: Running                       │
│ ● Implementation │ Agent: developer-2                    │
│ ○ Tests          │ Attempt: 2/3                          │
│ ○ Review         │ Lease: 08:41 remaining                │
│ ○ Documentation  │ Artifacts: 2                          │
├──────────────────┴───────────────────────────────────────┤
│ Timeline                                                  │
│ 14:02 W17 claimed by developer-2                         │
│ 14:04 artifact patch.diff created                        │
│ 14:06 heartbeat received                                 │
└──────────────────────────────────────────────────────────┘
```

Weitere Ansichten:

* Dependency Graph,
* Agenten und Leases,
* Artefakte,
* Graph Claims,
* Budget,
* Events,
* Human Approvals.

---

# 23. Persistenz

Für die erste Version bietet sich SQLite an.

## Warum SQLite

* lokal einfach nutzbar,
* keine zusätzliche Infrastruktur,
* transaktional,
* gut testbar,
* geeignet für CLI und lokale Projekte,
* später durch PostgreSQL ersetzbar.

## Tabellen

```text
projects
runs
work_items
work_item_dependencies
work_attempts
leases
artifacts
verifications
human_gates
checkpoints
events
budgets
```

Graphwissen bleibt im Storage von `agentkit-graph`.

Artefakte können zunächst im Dateisystem gespeichert werden.

```text
.agentkit/
  work.db
  projects/
    <project-id>/
      artifacts/
      checkpoints/
      logs/
      worktrees/
```

---

# 24. Interne Module des Crates

```text
agentkit_work/
  src/
    lib.rs

    domain/
      project.rs
      run.rs
      work_item.rs
      attempt.rs
      lease.rs
      artifact.rs
      verification.rs
      budget.rs
      event.rs
      checkpoint.rs

    application/
      project_service.rs
      run_service.rs
      scheduler.rs
      dispatcher.rs
      verification_service.rs
      checkpoint_service.rs
      recovery_service.rs

    ports/
      work_store.rs
      event_store.rs
      artifact_store.rs
      agent_executor.rs
      swarm_executor.rs
      graph_gateway.rs
      context_gateway.rs
      workspace_manager.rs

    infrastructure/
      sqlite/
      filesystem/
      git/
      clock/
```

Diese Struktur hält Domänenlogik und technische Adapter getrennt.

---

# 25. Ports und Adapter

## Work Store

```rust
pub trait WorkStore {
    fn create_project(&self, project: &WorkProject) -> Result<()>;
    fn save_work_item(&self, item: &WorkItem) -> Result<()>;
    fn ready_items(&self, run_id: &RunId) -> Result<Vec<WorkItem>>;
    fn claim_item(
        &self,
        item_id: &WorkItemId,
        agent_id: &AgentId,
        lease: Duration,
    ) -> Result<WorkLease>;
}
```

## Agent Executor

```rust
pub trait AgentExecutor {
    fn execute(
        &self,
        package: AgentWorkPackage,
        events: WorkEventSink,
    ) -> Result<WorkResult>;
}
```

## Swarm Executor

```rust
pub trait SwarmExecutor {
    fn execute_swarm(
        &self,
        template: &SwarmTemplateId,
        package: AgentWorkPackage,
        events: WorkEventSink,
    ) -> Result<WorkResult>;
}
```

## Graph Gateway

```rust
pub trait GraphGateway {
    fn retrieve_context(
        &self,
        query: WorkGraphQuery,
    ) -> Result<Vec<GraphClaimRef>>;

    fn record_claims(
        &self,
        provenance: WorkProvenance,
        claims: Vec<ClaimDraft>,
    ) -> Result<Vec<GraphClaimRef>>;

    fn promote_verified(
        &self,
        claims: &[GraphClaimRef],
    ) -> Result<()>;
}
```

---

# 26. Typischer Ablauf

## Beispielauftrag

```text
Analysiere agentkit_swarm, implementiere einen Graceful Shutdown,
ergänze Tests und lasse die Änderung unabhängig reviewen.
```

## Phase 1: Projekt anlegen

```text
Project P1
Run R1
```

## Phase 2: Initiale Planung

Ein Planungsagent oder Discovery-Swarm erzeugt:

```text
W1 Lifecycle analysieren
W2 gewünschtes Zustandsmodell spezifizieren
W3 Quiescing-Zustand implementieren
W4 Integrationstests ergänzen
W5 unabhängigen Review durchführen
W6 Dokumentation aktualisieren
```

## Phase 3: Abhängigkeiten

```text
W1 → W2 → W3 → W4 → W5
                    ↘ W6
```

## Phase 4: Bearbeitung

`W1` wird an einen Architektur-Agenten vergeben.

Er erzeugt:

* Analyseartefakt,
* Graph Claims,
* mögliche Folgeaufgaben.

## Phase 5: Implementierung

`W3` erhält:

* akzeptierte Architekturentscheidungen,
* relevante Claims,
* Artefakte aus `W1` und `W2`,
* eigenen Git Worktree.

## Phase 6: Verifikation

`W4` führt Tests aus.

`W5` erhält den Patch und die Acceptance Criteria, aber nicht notwendigerweise den vollständigen Gesprächsverlauf des Implementierungsagenten.

## Phase 7: Promotion

Verifizierte Erkenntnisse werden in den Canonical Graph übernommen.

## Phase 8: Abschluss

Das Projekt gilt erst als abgeschlossen, wenn:

* alle notwendigen Work Items abgeschlossen sind,
* keine kritischen Items blockiert sind,
* erforderliche Verifikationen vorliegen,
* das Budget nicht überschritten wurde,
* ein finaler Checkpoint geschrieben wurde.

---

# 27. Completion Policy

Ein Work Run benötigt klare Abschlussbedingungen.

```rust
pub enum RunCompletionPolicy {
    AllRequiredItemsCompleted,
    RequiredMilestoneReached(MilestoneId),
    ObjectiveVerified,
    HumanApproved,
}
```

Ein LLM sollte den gesamten Run nicht allein durch eine Textaussage wie „fertig“ beenden können.

Die Runtime prüft deterministisch:

```text
Alle Pflichtaufgaben abgeschlossen?
Alle notwendigen Reviews akzeptiert?
Alle erforderlichen Tests erfolgreich?
Keine kritischen Blocker?
Abschlussartefakte vorhanden?
```

---

# 28. Mindestversion – MVP

Die erste Version sollte bewusst klein bleiben.

## MVP-Funktionen

1. `WorkProject` und `WorkRun`
2. persistente `WorkItems`
3. einfache Abhängigkeiten
4. SQLite Storage
5. Work-Item-Statusmaschine
6. einzelner lokaler Worker
7. Start eines normalen `agentkit`-Agenten
8. Work Attempts
9. Artefaktreferenzen
10. Event Log
11. Checkpoint und Resume
12. einfache Retry-Policy
13. CLI für Create, Run, Status und Resume
14. Verbindung zu `agentkit-graph`
15. maximal ein schreibender Agent gleichzeitig

## Noch nicht im MVP

* verteilte Worker,
* mehrere Prozesse,
* PostgreSQL,
* komplexe Scheduling-Algorithmen,
* dynamische Skalierung,
* Remote Execution,
* automatische Git-Merges,
* komplexe Human-Approval-Oberflächen.

---

# 29. Umsetzung in Phasen

## Phase 1 – Domänenmodell

Implementieren:

* Project
* Run
* WorkItem
* Dependency
* Attempt
* Statusmaschinen
* Events

Deliverable:

> Vollständig getesteter Domain Core ohne Datenbank oder Agenten.

## Phase 2 – Persistenz

Implementieren:

* SQLite Adapter
* Event Store
* atomisches Claiming
* Checkpoints
* Recovery

Deliverable:

> Projekte und Work Items überleben einen Prozessneustart.

## Phase 3 – Einzelner Agent Worker

Implementieren:

* `AgentExecutor`
* AgentWorkPackage
* Work-Item-Ausführung
* Result- und Failure-Mapping
* Leases und Heartbeats

Deliverable:

> Ein persistentes Work Item kann von einem normalen agentkit-Agenten bearbeitet und wiederholt werden.

## Phase 4 – Graph-Integration

Implementieren:

* relevante Claims abrufen,
* Claims mit Work-Provenance speichern,
* Working-Graph-Claims erzeugen,
* verifizierte Claims promoten.

Deliverable:

> Spätere Agenten können Erkenntnisse früherer Work Items nutzen.

## Phase 5 – Verifikation

Implementieren:

* Acceptance Criteria
* Verification Work Items
* Peer Review
* automatisierte Testprüfung
* Reopen bei Ablehnung

Deliverable:

> Implementierungen werden nicht allein durch Selbstaussage abgeschlossen.

## Phase 6 – Swarm-Integration

Implementieren:

* Work Item durch Swarm bearbeiten,
* Swarm Templates,
* Swarm-Ergebnis in WorkResult überführen,
* Konsens als Verifikationsergebnis speichern.

Deliverable:

> Ein Work Item kann kontrolliert von einem kurzlebigen Swarm bearbeitet werden.

## Phase 7 – Workspace-Isolation

Implementieren:

* Git Worktrees,
* Commit-Artefakte,
* Integrations-Work-Items,
* Merge-Konfliktbehandlung.

Deliverable:

> Mehrere Agenten können sicher parallel an getrennten Änderungen arbeiten.

## Phase 8 – TUI und Observability

Implementieren:

* Projektstatus,
* Work-Item-Board,
* Timeline,
* Budgetanzeige,
* Leases,
* Graph Claims,
* Checkpoints.

Deliverable:

> Mehrstündige Runs sind für den Benutzer nachvollziehbar und steuerbar.

---

# 30. Erster End-to-End-Test

Der erste echte Systemtest sollte absichtlich mehrere Laufphasen und einen Neustart enthalten.

## Auftrag

```text
Analysiere agentkit_swarm und identifiziere die drei wichtigsten
Lifecycle-Risiken. Implementiere die wichtigste Verbesserung,
ergänze Tests, führe ein unabhängiges Review durch und dokumentiere
die Entscheidung.
```

## Testbedingungen

* mindestens fünf Work Items,
* mindestens zwei verschiedene Agentenrollen,
* mindestens ein Swarm,
* Claims im Working Graph,
* Promotion in den Canonical Graph,
* mindestens ein Git-Artefakt,
* mindestens ein Review,
* Prozess während der Arbeit absichtlich beenden,
* Arbeit nach Neustart fortsetzen,
* abgeschlossene Analyse nicht vollständig wiederholen,
* finalen Evidence Trail erzeugen.

## Erfolgskriterien

```text
- kein Work Item geht verloren
- kein abgeschlossenes Work Item wird unnötig wiederholt
- abgelaufene Leases werden freigegeben
- der Run wird korrekt fortgesetzt
- Claims besitzen vollständige Provenance
- nur verifizierte Claims werden promotet
- das Endergebnis enthält Artefakte, Tests und Review
```

---

# 31. Leitprinzipien

## Persistenter Zustand vor langlebigen Prozessen

Jeder Prozess darf ausfallen. Der Arbeitszustand darf nicht verloren gehen.

## Kleine ausführbare Einheiten

Work Items sollen begrenzt und überprüfbar sein.

## Artefakte vor Chat-Verläufen

Das eigentliche Ergebnis ist ein Artefakt, nicht eine lange Nachricht.

## Verifikation vor Abschluss

Ein Agent ist Autor, nicht automatisch Prüfer seiner eigenen Arbeit.

## Wissen und Arbeit getrennt halten

`agentkit-graph` speichert Wissen. `agentkit-work` speichert Arbeitszustand.

## Agenten sind austauschbare Worker

Ein neuer Agent muss eine Aufgabe anhand persistenter Informationen übernehmen können.

## Deterministische Runtime, agentische Problemlösung

Die Runtime verwaltet Status, Budgets und Abhängigkeiten deterministisch. Agenten lösen die fachlichen Probleme.

---

# 32. Zusammenfassung

`agentkit-work` wird die langlebige Projektebene von agentkit.

Es soll verwendet werden, wenn Aufgaben:

* mehrere Schritte besitzen,
* länger laufen,
* nach einem Abbruch fortgesetzt werden müssen,
* mehrere Agenten oder Swarms benötigen,
* Artefakte erzeugen,
* überprüft werden müssen,
* Abhängigkeiten und Budgets besitzen.

Die Kernkomponenten sind:

```text
Projects
Runs
Work Items
Dependencies
Attempts
Leases
Artifacts
Verification
Events
Checkpoints
Budgets
Recovery
Scheduler
```

Die Kombination der bestehenden Komponenten ergibt anschließend:

```text
agentkit-work
    hält die Arbeit am Leben

agentkit-swarm
    ermöglicht Zusammenarbeit

agentkit-graph
    hält Wissen am Leben

ctxman
    hält Agentenkontexte beherrschbar

agentkit
    führt die eigentliche Agentenarbeit aus
```

Damit kann agentkit von einem leistungsfähigen Agenten-Framework zu einer Runtime für langfristige, nachvollziehbare und wiederaufnehmbare autonome Arbeit wachsen.
