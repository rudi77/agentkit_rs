# Spatiotemporal Composability für agentkit — Komponenten-Runtime mit revertiblen Effekten und reaktiven Coeffects

**Status:** Vorschlag (Konzept + API-Skizzen + Roadmap, keine Implementierung)
**Quelle:** Paper *„A Programming Paradigm for Spatiotemporal Composability"* (Shi/Zhang/Cui, das „Cordis-Paper"), Ist-Stand dieses Repos.
**Verhältnis zum Bestand:** Dieses Dokument **löst Abschnitt 4 von [`observability-extensibility-plan.md`](observability-extensibility-plan.md) ab und vertieft ihn.** Jener Plan hatte volle reaktive Coeffects mit der Klausel „YAGNI, bis langlaufende Prozesse existieren (agentkit_work wäre der Kandidat)" zurückgestellt. agentkit_work existiert; dieses Dokument löst die Klausel ein. Die P1-Punkte des alten Plans (token_usage, deklarative Tool-Metadaten) und sein 3.3 bleiben unberührt gültig; seine P2-Punkte 3.4 (Extension-Schnittstelle) und 4.2 (Disposer-Registrierung) werden hier unverändert zur **Phase 1**; sein 4.3 (`extension_state`-Events) geht hier auf; die Ablehnungen aus seinem 4.4 werden unten neu entschieden — die meisten bleiben bestehen, die Coeffect-Ablehnung fällt für die langlaufenden Oberflächen.

---

## 1. These

Das Paper identifiziert zwei orthogonale Dimensionen dynamischer Komposition: **temporale Komponierbarkeit** (die Effekte einer entfernten Komponente werden vollständig zurückgenommen) und **spatiale Komponierbarkeit** (Komponenten deklarieren Abhängigkeiten, auf deren Kommen und Gehen der Runtime reagiert). Sein zweites motivierendes Beispiel sind wörtlich *selbst-evolvierende Agent-Harnesses* — agentkits Domäne.

agentkit lebt das halbe Paper bereits, ohne es so zu nennen:

- `McpHub::rewire` ist der „coarse-grained workaround" des Papers wörtlich: statt eine Registrierung zurückzunehmen, wird die ganze Registry aus einer Basis neu gebaut.
- „Sub-Agenten bekommen nie `task`, Schwarm-Mitglieder nie `swarm`" ist gelebte Capability-Disziplin (Paper §6.3), nur als Konvention statt als geprüfte Deklaration.
- `dry_run_blocking` und `ApproveFn` sind Interception am Zugriffspunkt — die zwei realen Fälle des Interception-Mechanismus, bereits gelöst.

Der Vorschlag zieht daraus eine einzige architektonische Konsequenz: **Die Fiber-Menge wird die Quelle der Wahrheit, die ToolRegistry ihre abgeleitete Projektion.** Kurz: *rewire ist das Rendern.* Temporale Komponierbarkeit heißt: jede Komponente akkumuliert ihre Rücknahmen (LIFO-Disposer) und ihre deklarativen Tool-Beiträge; Entladen wendet die Disposer an und lässt die Beiträge aus der nächsten Projektion fallen. Spatiale Komponierbarkeit heißt: Komponenten deklarieren `inject`/`provide`-Schlüssel, und eine synchrone Fixpunkt-Schleife aktiviert und deaktiviert sie.

Weil das ganze Repo synchron ist (kein tokio, Komposition läuft auf genau einem Thread), kollabiert die schwerste Maschinerie des Papers — Inertia, Effekt-Iteratoren, Interleaving-Beweise — zu atomaren Funktionsaufrufen. Was übrig bleibt, ist genau die eine tragende Garantie: **Dependents werden entladen, bevor die Inversen ihres Providers laufen** (der Withdrawal-Guard, hier als synchrone Kaskade).

---

## 2. Konzept-Abgleich: Paper ↔ agentkit

| Paper-Konzept | agentkit-Umsetzung | Einordnung |
|---|---|---|
| Einheitlicher Kontext Γ∞ (Zustand × Accumulator × Coeffect-Store) | `ComposeCtx` im neuen Crate `agentkit_compose`: Fiber-Liste (Ladereihenfolge), je Fiber Disposer-Accumulator + Tool-Beiträge, Capability-Bindungen | neu (Phase 2) |
| Revertibler Effekt (Mutation liefert Inverse) | Zwei Effektklassen: (a) **Tool-Beiträge** — deklarative Datensätze, „Inverse" = Wegfall aus der Projektion beim nächsten Rendern; (b) **Ressourcen-Disposer** — `Box<dyn FnOnce() + Send>`, LIFO je Fiber (MCP-Flag aus, Thread-Join, Index fallen lassen) | neu (Phase 1 light, Phase 2 voll) |
| Locality of Concern (kein getrennter deactivate-Hook) | `Component::apply` ist die **einzige** Lifecycle-Methode; Entladen wird vollständig aus Accumulator + Projektion abgeleitet. Es gibt kein `on_unload` im Trait | als Design-Zwang übernommen |
| Reaktive Coeffects (`inject`/`provide`, Satisfaction-Prädikat) | `inject()`/`provide()` liefern `CapKey`-Listen; `ComposeCtx::settle()` ist die Klassifikations- und Benachrichtigungsschleife, nach jeder Änderung bis zum Fixpunkt | neu (Phase 2) |
| Committed View speichert Provider-**uid**, nicht Wert | `Fiber::committed: HashMap<CapKey, u64>` — jeder Providerwechsel lädt den Dependent neu, auch bei gleichem Wert. Es gibt bewusst keinen Wertvergleich | wörtlich übernommen |
| Withdrawal-Guard / Drain-Ordnung | Rekursive Kaskade in `unload`: Dependents (per committed-uid) deaktivieren in umgekehrter Abhängigkeitsreihenfolge, **bevor** die Disposer des Providers laufen; ein Dependent kann im eigenen Teardown die scheidende Abhängigkeit noch lesen | übernommen — die tragende Garantie |
| Fiber-Lifecycle Inactive/Loading/Active/Unloading/Failed | `FiberState`-Enum; jeder Übergang als `Structured{kind:"extension_state"}` → Trace → viz | übernommen; Loading/Unloading sind *beobachtbare Zustände um echte Arbeit* (Prozess-Start, Thread-Join), keine schedulbaren Phasen |
| Inertia (laufende Transition läuft zu Ende, dann Kette) | **entfällt.** Komposition läuft auf genau einem Thread (REPL/TUI-Thread bzw. Work-Runner-Thread); Transitionen sind atomare Aufrufe. Ein `busy`-Reentranz-Guard (→ `Err`) ersetzt die Warteschlange | bewusste Auslassung |
| Effekt-Iterator (Abbruch an Iterationsgrenzen → partieller Rollback) | Reduziert auf: `apply` liefert `Result`; bei `Err` laufen die *bisher* akkumulierten Disposer sofort, Fiber → Failed. In agentkit_work ist das ehrliche Analog der Iterationsgrenze die **Attempt-Grenze**: rekonziliert wird nie mitten im Attempt | reduziert |
| Hierarchische Kontexte (Eltern-Unload kaskadiert zu Kindern) | **entfällt.** Ein flacher Kontext je Agent-Session bzw. je Work-Runner. Helfer (Sub-Agenten, Schwarm-Mitglieder) sind genau eine Ebene tief und kurzlebig; die Member-Menge einer Schwarm-Komponente lebt *in* der Komponente und wird von deren Disposern abgeräumt | bewusste Auslassung |
| Isolation (Realms) | **entfällt.** Sub-Agent-/Schwarm-Registries sind durch Frisch-Bau je Spawn bereits isoliert | bewusste Auslassung |
| Interception (Metadaten-Monoid am Zugriffspunkt) | **kein neuer Mechanismus.** Der Render-Schritt *ist* der Zugriffspunkt: `dry_run_blocking` und `ApproveFn` werden auf die Projektion angewandt. Zwei reale Fälle, beide gelöst | als vorhanden erkannt, nichts gebaut |
| Deklarativer Loader + Rekonziliation | `--profile` bekommt eine `components`-Sektion; `/compose apply` rekonziliert je Eintrags-id (entfernt → entladen, neu → einfügen, geändert → ersetzen). Quiescence-Gesetz `apply(A→B) ≡ fresh(B)` wird ein Test | neu (Phase 3); Granularität ganzer Einträge, nicht Feld-Diff |
| HMR (Hot Module Replacement) | **entfällt für Code** (statisches musl-Binary). Das ehrliche Analog: Skills/Rollen/`.mcp.json`/Profile sind heiß nachladbare *Daten* — genau das liefert die Rekonziliation | auf Daten-Reload reduziert |
| System Boundary (§6.1: nur exklusiv Kontrolliertes ist revertibel) | Tabelle in Abschnitt 3 | als Dokumentationsdisziplin übernommen |
| `inject` = prüfbare Capability-Anforderung (§6.3) | Die `components`-Liste des Profils ist die menschlich reviewte Capability-Menge; agenten-seitige `compose_*`-Tools können nur innerhalb davon schalten, gated durch `ApproveFn` | übernommen (Phase 3) |

Lesart wie im Vorgänger-Dokument: agentkit ist dem Paper näher, als es aussieht. Das Paper liefert die Begriffe — und diesmal auch die Struktur der nächsten Ausbaustufe.

---

## 3. System-Grenze: was revertibel ist und was Emission

Das Paper (§6.1) zieht die Grenze pro *Ort*: revertibel ist nur, was das System exklusiv modifizieren und wiederherstellen kann; alles andere ist Emission (wirkt als Identität auf den Kontext) und kennt nur Zurückhalten oder Kompensation. Für agentkit:

| Ort | Klasse | Inverse / Kompensation |
|---|---|---|
| `ToolRegistry`-Einträge | revertibel | Wegfall aus der Projektion / `remove(name)` |
| MCP-`enabled`-Flag + stdio-Session | revertibel | Flag aus (Session bleibt, wie heute); Prozess-Ende nur bei Fiber-*Entfernung* |
| Skill-/Rollen-Indizes im Speicher | revertibel | fallen lassen |
| Schwarm-Member-Threads + Mailboxen | revertibel | Shutdown-Nachricht + `join` im Disposer der Schwarm-Komponente |
| Graph-**Working**-Layer (vor Promotion) | revertibel | Working-Layer verwerfen |
| Graph-Canonical (nach Promotion) | **Emission** | Kompensation: Deprecation-Einträge mit Provenance — nie Löschung |
| Work-Leases | revertibel | Lease freigeben |
| Work-Journal / NDJSON-Trace-Zeilen | **Identitätseffekt, bewusst** | keine — append-only-Audit ist Feature, kein Leck |
| LLM-Aufrufe (Tokens), `run_shell`/`write_file`-Wirkungen, gesendete Schwarm-Nachrichten, stdout | **Emission** | Zurückhalten (`--dry-run`), Freigabe (`ApproveFn`), git für Workspace-Dateien |

Bemerkenswert: **Workspace-Dateien sind Emissionen**, obwohl Tools sie schreiben — der Workspace wird mit dem Nutzer geteilt, nicht exklusiv kontrolliert; das ist exakt das Kriterium des Papers. Wer Workspace-Rollback will, hat ihn schon: git (`agentkit_work` nutzt Branch-je-Item mit Drop-Guard).

---

## 4. Das Komponentenmodell: Crate `agentkit_compose`

Neues Sibling-Crate nach dem Muster von agentkit_graph: Path-Dependency auf `agent_framework_rs` mit `default-features = false`, **null weitere Dependencies** außer serde_json. Es sitzt eine Stufe *unter* den Siblings: `agentkit_work → agentkit_compose → agentkit` ist eine legale Einweg-Kette; der Kern erfährt nichts davon. Konkrete Komponenten für Kern-Capabilities (MCP, Skills, Rollen) leben im Crate selbst; Komponenten, die Schwarm/Graph/Work wrappen, leben in `agentkit_app` (dem einzigen Crate, das alle kennt) — dasselbe Adapter-Muster wie `WorkGraphAdapter`. Ein neuer Port-Trait ist nicht nötig.

### 4.1 Schlüssel und Werte

```rust
/// Capability-Schlüssel. String mit dokumentierter Konvention statt Enum:
/// "mcp:<server>", "skills", "role:<name>", "graph", "work", "swarm",
/// sowie "tool:<name>" (beantwortet der Kontext selbst, siehe settle()).
pub type CapKey = String;
pub type CapValue = Arc<dyn std::any::Any + Send + Sync>;
```

**Entscheidung:** String-Schlüssel + `Any`-Werte, kein typisiertes Enum. Verworfen: ein geschlossenes Enum der Capability-Arten — MCP-Server sind dynamisch-per-Name, und Graph-/Work-Store-Typen kann dieses Crate nicht benennen, ohne die Abhängigkeitsrichtung umzukehren. `Any`-Downcasts bleiben auf die Adapter-Komponenten in agentkit_app beschränkt; die kleine Schlüsselmenge ist eine Doc-Comment-Konvention, genau wie die `kind`-Strings der `Structured`-Events heute. Sonderregel: Schlüssel der Form `"tool:<name>"` beantwortet der Kontext selbst gegen die aktuelle Projektion (Basis + aktive Beiträge) — das liefert Rollen und Skills ihre Satisfaktionsprüfung (Punkt 4.4 des alten Plans) ohne einen zweiten Mechanismus.

### 4.2 Component und Effects

```rust
/// Ein Werkzeug-Beitrag: deklarativ, damit die Registry als Projektion
/// jederzeit neu gerendert werden kann. Trägt die deklarierten
/// Tool-Metadaten (destruktiv ja/nein) aus dem Observability-Plan §3.2 mit.
pub struct Contribution {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub f: ToolFn,                 // Arc — Re-Registrierung ist billig
    pub destructive: bool,         // fürs dry-run beim Rendern
}

pub type Dispose = Box<dyn FnOnce() + Send>;

/// Sammler, den apply() befüllt — der Effekt-Accumulator des Papers.
pub struct Effects {
    tools: Vec<Contribution>,
    disposers: Vec<Dispose>,                    // LIFO beim Entladen
    values: HashMap<CapKey, CapValue>,          // Werte zu provide()-Schlüsseln
}
impl Effects {
    pub fn add_tool(&mut self, c: Contribution);
    pub fn on_unload(&mut self, d: Dispose);
    pub fn provide_value(&mut self, key: &str, v: CapValue);
}

/// Aufgelöste inject-Schlüssel zum Zeitpunkt der Aktivierung.
pub struct Deps<'a> { /* key -> (provider_uid, Option<&CapValue>) */ }
impl Deps<'_> {
    pub fn value<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>>;
}

pub trait Component: Send {
    fn id(&self) -> &str;
    fn inject(&self) -> Vec<CapKey>;    // leer = immer erfüllt
    fn provide(&self) -> Vec<CapKey>;
    /// Der EINZIGE Lifecycle-Hook (Locality of Concern): wirkt Effekte in
    /// `fx`, liest Abhängigkeiten aus `deps`. Err => Fiber Failed; bereits
    /// akkumulierte Inverse laufen sofort (partieller Rollback).
    fn apply(&mut self, fx: &mut Effects, deps: &Deps) -> Result<(), String>;
}
```

Es gibt bewusst **keine** `deactivate`-/`teardown`-Methode im Trait — das ist das zentrale Argument des Papers, und es hält Komponenten ehrlich: alles, was sie tun, geht durch `Effects` und ist damit rücknehmbar.

### 4.3 Fiber und Kontext

```rust
pub enum FiberState { Inactive, Loading, Active, Unloading, Failed }

pub struct Fiber {
    uid: u64,                              // Identität, nie wiederverwendet
    component: Box<dyn Component>,
    target_active: bool,                   // Wunsch (bleibt bei Deaktivierungs-Kaskade true)
    state: FiberState,
    tools: Vec<Contribution>,              // aus apply(); Teil der Projektion
    disposers: Vec<Dispose>,               // aus apply(); LIFO
    values: HashMap<CapKey, CapValue>,
    committed: HashMap<CapKey, u64>,       // Schlüssel -> Provider-uid bei Aktivierung
    error: Option<String>,
}

pub struct ComposeCtx {
    fibers: Vec<Fiber>,                    // Ladereihenfolge = Render-Reihenfolge
    next_uid: u64,
    busy: bool,                            // Reentranz-Schutz statt Inertia-Warteschlange
    base: ToolRegistry,                    // eingebaute Coding-Tools etc.
    dry_run: bool,
    events: Option<Box<dyn FnMut(AgentEvent) + Send>>,  // extension_state -> Bus/Trace
}

impl ComposeCtx {
    pub fn insert(&mut self, c: Box<dyn Component>, active: bool) -> Result<u64, String>;
    pub fn set_target(&mut self, id: &str, active: bool) -> Result<(), String>;
    pub fn remove(&mut self, id: &str) -> Result<(), String>;   // entladen + Fiber weg
    pub fn states(&self) -> Vec<FiberInfo>;                     // für /compose, TUI, Tools
    /// DIE Projektion: base.clone() + Beiträge aller Active-Fibers in
    /// Ladereihenfolge + dry-run-Hülle. „rewire ist das Rendern."
    pub fn render(&self) -> ToolRegistry;
    fn settle(&mut self);   // Fixpunkt: aktiviert Erfülltes
}
```

**Die settle-Schleife** (spatiale Komponierbarkeit, synchron): nach jedem `insert`/`set_target`/`remove` wiederholen, bis nichts sich mehr ändert: für jeden Fiber mit `target_active && Inactive` die `inject()`-Schlüssel auflösen — jeder Schlüssel muss von einem Active-Fiber bereitgestellt oder ein erfüllbarer `"tool:"`-Schlüssel sein. Erfüllt: Zustand Loading, Event, `apply` ausführen, Beiträge/Disposer/committed-uids festhalten, Zustand Active (bei `Err`: Failed + partieller Rollback). Die Schleife terminiert, weil jede Iteration die Active-Menge strikt vergrößert, beschränkt durch die Fiber-Zahl; Deaktivierung geschieht nur über explizites Entladen (unten) und läuft dort als Kaskade.

**Entladen mit Withdrawal-Guard** (temporale Komponierbarkeit): Um Fiber P zu deaktivieren: alle Active-Fibers sammeln, deren `committed` `P.uid` enthält; diese rekursiv zuerst deaktivieren (ihr `target_active` bleibt `true` → sie werden *pending* und reaktivieren automatisch, wenn ein Ersatz-Provider erscheint). Dann P: Zustand Unloading, Event, Disposer LIFO ausführen, `tools`/`values` fallen lassen, Zustand Inactive, Event. Weil die Disposer der Dependents laufen, während P materiell noch lebt (seine Disposer sind noch nicht gelaufen), kann ein Dependent im eigenen Teardown die scheidende Abhängigkeit noch benutzen — die Garantie des Papers, gratis, weil alles ein Call-Stack ist. Provider-Ersatz (P1 entladen, P2 mit demselben Schlüssel einfügen) geschieht automatisch: Dependents deaktivieren mit P1, `settle` reaktiviert sie gegen P2 — und weil `committed` uids speichert, passiert das **auch bei gleichem Wert**.

**Was Loading/Unloading ohne Nebenläufigkeit noch bedeuten:** (a) die Zustände, in denen echte Arbeit passiert — MCP-Handshake, Thread-Joins —, sodass ein Hänger im Trace attributierbar ist; (b) die Payload der `extension_state`-Events, sodass viz/TUI *Übergänge* zeigen, nicht nur Vorher/Nachher. Kein anderer Thread beobachtet sie. Failed ist real und bleibt stehen, bis ein explizites `set_target(true)` neu versucht.

### 4.4 Modul-Layout

```
agentkit_compose/
  Cargo.toml            # agentkit-Path-Dependency, default-features = false
  README.md             # deutsch; „Bewusste Design-Entscheidungen" (Abschnitt 6)
  src/
    lib.rs
    component.rs        # Component, Effects, Deps, Contribution, CapKey
    fiber.rs            # Fiber, FiberState, FiberInfo
    context.rs          # ComposeCtx: insert/set_target/remove/settle/render
    components.rs       # McpServerComponent, SkillsComponent, RoleComponent
    reconcile.rs        # Phase 3: deklaratives Profil -> Soll/Ist-Abgleich
  tests/integration.rs  # FakeLlm-Szenarien, siehe Abschnitt 7
```

---

## 5. Integrationspunkte, konkret

### (a) `ToolRegistry::remove` — die fehlende Primitive (Kern, Phase 1)

```rust
impl ToolRegistry {
    /// Entfernt Schema und Funktion. false, wenn unbekannt.
    pub fn remove(&mut self, name: &str) -> bool;
    /// Namen aller Tools mit Präfix — für gezieltes Entfernen je MCP-Server.
    pub fn names_with_prefix(&self, prefix: &str) -> Vec<String>;
}
```

~30 Zeilen (`schemas` ist ein `Vec<Value>`, `fns` eine `HashMap`). Zwei sofortige Nutzer — inkrementeller MCP-Toggle und das Phase-1-Teardown der Extension-Schnittstelle — die Zwei-Nutzer-Regel der Guidelines ist erfüllt.

### (b) MCP: vom Voll-Rewire zur Komponente

Phase 1 behält die öffentliche `apply`/`rewire`-API, macht aber den Toggle inkrementell (`remove` per `mcp__<server>__`-Präfix bzw. Registrieren eines Servers) — Verhaltensäquivalenz gegen den Voll-Neuaufbau wird getestet. Phase 2 wrappt jeden Server als `McpServerComponent { hub: Arc<McpHub>, name: String }`: `provide = ["mcp:<name>"]`, `apply` schaltet `enabled` an und registriert die Server-Tools als Beiträge, der Disposer schaltet aus. Sessions bleiben über Deaktivierungen hinweg verbunden (heutiges, bewusstes Verhalten — Toggle ohne Reconnect); erst `ComposeCtx::remove` des Fibers darf den Kindprozess beenden. `rewire` überlebt *als Commit-Schritt*: `agent.tools = ctx.render()` — die elegante Auflösung der Klon-beim-Build-Falle (die Registry wird bei `build()` in den Agenten geklont; ein Live-Umbau muss also ohnehin über eine Ersetzung von `agent.tools` laufen). `register_enabled` bleibt für die Spawn-Pfade von Sub-Agenten und Schwarm-Mitgliedern (die lesen den Hub live, wie heute).

### (c) Skills und Rollen als Komponenten

`SkillsComponent` stellt `"skills"` bereit (Wert: `Arc<Skills>` für den task-Tool-Builder) und trägt die Skill-Tools bei. `RoleComponent` je Custom-Rolle: `inject` = `["tool:<t>"]` für jedes Tool, das die Rolle nennt. Eine Rolle, deren Tools nicht baubar sind (z. B. `read_pdf` ohne `pdf`-Feature), bleibt sichtbar **pending mit Warnung**, statt zur Laufzeit soft zu erroren — genau die Satisfaktionsprüfung, die der alte Plan wollte, hier als Nebenprodukt. Eingebaute Rollen bleiben in der Basis (per Konstruktion immer erfüllbar; sie zu wrappen wäre Zeremonie).

### (d) REPL/TUI und deklaratives Profil

`/mcp on|off` (REPL) und F2 (TUI) werden zu `ctx.set_target("mcp:<name>", …)` gefolgt vom Render-Commit — nutzersichtbares Verhalten identisch, zusätzlich erscheinen jetzt `extension_state`-Events im Trace. Neues `/compose` listet die Fiber-Zustände. Phase 3: das `--profile`-JSON bekommt eine Sektion `components: [{id, kind, config, disabled}]`; `reconcile(&mut ctx, &profile)` difft je id — entfernt → `remove`, neu → `insert`, geändert → `remove`+`insert` (**ganze Einträge, kein Feld-Diff**: Einträge sind klein; Feld-Rekonziliation ist eine HMR-Ära-Optimierung, die wir nicht brauchen — dokumentierte Abweichung). Das Quiescence-Theorem des Papers wird ein Test: `apply(A→B)` liefert denselben beobachtbaren Zustand (Fiber-Menge, gerenderte Tool-Namen) wie ein Frischstart mit B.

### (e) Selbst-Evolution: `compose_*`-Tools (Phase 3, standardmäßig aus)

Hinter `--compose-tools`: `compose_list` (read-only), `compose_enable`/`compose_disable` (als destruktiv deklariert, also dry-run-geblockt und `ApproveFn`-gated). **Das Modell kann nur Komponenten schalten, die im menschlich reviewten Profil vordeklariert sind — nie eine neue Komponentenart oder einen neuen MCP-Server einführen.** Das operationalisiert §6.3 des Papers: das Profil ist die prüfbare Capability-Anforderung, das Modell wählt innerhalb davon, der Rollback ist per Konstruktion garantiert. Verworfen: ein volles `compose_load` mit beliebigen MCP-Specs — das wäre ein nicht reviewbarer Eskalationskanal.

**Mid-Run-Commit (die RunHandle-artige Falle):** `agent.tools` kann nicht aus einer Tool-Closure heraus getauscht werden (`drive` hält den Agenten). Die Lösung spiegelt `RunHandle`: ein geteiltes `Arc<ComposeHandle>` mit `Mutex<ComposeCtx>` + atomarem Dirty-Flag. `compose_*`-Tools mutieren den Kontext und setzen dirty; `Agent::drive` prüft das Flag **an jeder Step-Grenze** und tauscht `ctx.render()` ein — ein ~5-Zeilen-Hook, `None` = null Kosten, die einzige Kernberührung von Phase 3. Da die Schemas je LLM-Request gesendet werden, sieht das Modell seinen neuen Werkzeugkasten im unmittelbar nächsten Schritt. Verworfen: `ToolRegistry` selbst als `Arc<RwLock<…>>` — das zöge einen Lock durch die parallele Tool-Ausführung und jede Klon-Stelle, für einen einzigen Konsumenten. RunHandle-Timing und `dry_run` überleben automatisch: Beiträge halten `Arc`-Closures, die dieselbe RunHandle-Zelle einfangen (Re-Registrierung beim Rendern ist Identität), und `dry_run` wird *beim Rendern* angewandt — jede Projektion, auch die Mid-Run-Swaps, ist umhüllt.

### (f) Beobachtbarkeit

Jeder Übergang emittiert `EventData::Structured { kind: "extension_state", payload: {id, uid, from, to, fehler?} }` (Zustands-Token englisch als Identifier, Meldungstexte deutsch). Landet ohne neues Plumbing im NDJSON-Trace; agentkit-viz kann später Markierungen ergänzen (optional, nicht tragend). Der `kind`-Name bleibt der aus §4.3 des alten Plans, damit die Dokumente übereinstimmen.

### (g) agentkit_work als *der* Loader

Der Work-Runner ist der eine echte langlaufende Prozess — er hält einen `ComposeCtx` über die Lebensdauer eines Runs (Fibers: MCP-Server, Graph-Store, Skills, projektspezifische Erweiterungen). Je Attempt injiziert der bestehende `AgentSetup`-/`extra_tools`-Seam die aus `ctx.render()` abgeleiteten Beiträge in den frischen Agenten. **Rekonziliert wird nur an Attempt-Grenzen** — der Attempt ist die Transaktion; Leases, Journal und `scheduler::decide` bleiben unberührt. Perspektivisch kann die Work-Projekt-Konfiguration die `components`-Sektion tragen, sodass verschiedene Projekte unter einem Runner mit verschiedenen Capability-Mengen laufen. agentkit_work bekommt eine direkte Path-Dependency auf agentkit_compose (legale Einweg-Kette; kein Port-Trait nötig, da compose unterhalb der Siblings liegt).

### (h) Schwarm: statische Topologie bleibt

Die Invariante „eine laufende Schwarm-Topologie ändert sich nicht" (README agentkit_swarm, „Nicht im Umfang") **bleibt bestehen**. Komponenten wrappen Member-*Capabilities* (was ein Mitglied beim Spawn in seine Registry bekommt — heute `extra_member_tools`), nie ein Live-Re-Wiring des PeerDirectory: Konsens-Abschluss und die deterministischen Idle-/Limit-Policies setzen feste Mitgliedschaft voraus, und dynamische Schwärme entstehen ohnehin pro Auftrag — Neu-Erzeugen ist das billige, korrekte „Reload". Ein Schwarm-Lauf *als Komponente* (Phase 3, in agentkit_app) stellt `"swarm"` bereit, und sein Disposer leistet den Drain, den das Paper verlangt: Shutdown-Nachrichten, dann `join` auf jeden Member-Thread — die Inverse des Spawns, LIFO.

---

## 6. Bewusst nicht übernommen

Diese Liste wandert später in die „Bewusste Design-Entscheidungen"-Sektion des Crate-READMEs; das Paper spielt dabei die Rolle, die das PRD für agentkit_graph spielt.

1. **Effekt-Kontext-Algebra, Unabhängigkeits-Zeugen, Interleaving-Garantien** (Paper §3.1.3, §4.4): Komposition läuft auf einem Thread mit einer Handvoll Registranten in definierter Reihenfolge; LIFO je Fiber plus Ladereihenfolge beim Rendern ist die gesamte Beweislast.
2. **Inertia / Transitions-Warteschlange** (§4.3.3): es kann keine nebenläufigen Kompositionswünsche geben; ein `busy`-Reentranz-Guard mit `Err` ersetzt sie. Daraus folgt eine geprüfte (nicht nur dokumentierte) Regel: `apply` einer Komponente darf nicht in den Kontext zurückrufen.
3. **Isolation-Realms** (§3.2.3): ein Kontext je Session; die Isolation von Sub-Agenten/Schwarm-Mitgliedern existiert durch Frisch-Bau bereits.
4. **Interception-Monoid** (§3.2.3): die zwei realen Fälle (Approval, dry-run) werden am Render-Schritt angewandt, der *der* Zugriffspunkt des Papers ist — erkannt und benannt, kein Framework gebaut.
5. **Hierarchische Kontexte** (§3.3.1): Helfer sind eine Ebene tief und kurzlebig; Kapselung in Komponenten-Disposern ersetzt den Baum.
6. **HMR für Code** (§5.2.2): statisches musl-Binary. Das ehrliche Analog — Heiß-Nachladen von Skills/Rollen/`.mcp.json`/Profil-*Daten* — liefert genau die Rekonziliation.
7. **Wertbasierte Änderungserkennung**: nur committed-uids, wie im Paper; einfacher *und* korrekter (ein Provider, der denselben Wert liefert, ist trotzdem ein anderer Provider).
8. **Live-Re-Wiring laufender Schwärme**: siehe 5 (h).

---

## 7. Roadmap und Teststrategie

Jede Phase ist unabhängig auslieferbar, berührt keinen Verhaltenskontrakt (Event-Typ-Strings, Exit-Codes, Failure-Sentinels, `extra_tools`-Signatur für bestehende Aufrufer) und bleibt offline-by-default (keinerlei neue Dependencies).

### Phase 1 — Kern-Primitive („revertible effects light"; = P2-Punkte 3.4 + 4.2 + 4.3 des alten Plans)

Crates: `agent_framework_rs` (`tools.rs`: `remove`/`names_with_prefix`; `mcp.rs`: inkrementeller Toggle; `app.rs`: den Extension-Seam formalisieren — eine Erweiterung registriert über `ExtraToolCtx` und gibt ihr Teardown als festgehaltene Tool-Namen + optionalen Disposer zurück), `agentkit_app/src/lib.rs` (`FrontendTools` = erster Nutzer), `agentkit_work/src/executor.rs` (zweiter Nutzer). `extension_state`-Events für MCP-Toggles. Kein Verhaltensunterschied, solange niemand entlädt.

Tests (alle offline): remove/add-Roundtrip liefert schema-gleiche Registry; Toggle-Zyklus-Äquivalenz `rewire ≡ remove+add` (Assertion auf sortierten Tool-Namen und Schemas); FakeLlm-Lauf mit `extension_state`-Zeilen im Trace; dry-run überlebt einen Toggle.

### Phase 2 — Crate `agentkit_compose` (Fibers, Satisfaction, Projektion)

Neues Crate wie in 4.4; `McpServerComponent`/`SkillsComponent`/`RoleComponent`; REPL-/TUI-Toggles über `ComposeCtx` mit `render()` als Commit (agentkit_app).

Tests: settle-Fixpunkt aktiviert einen wartenden Fiber, wenn sein Provider lädt; Kaskaden-Reihenfolge beim Entladen (Disposer-Ausführungsreihenfolge festhalten, Dependents-vor-Provider prüfen); Provider-Ersatz reaktiviert Dependents trotz gleichem Wert (uid-Semantik); `apply`-Fehler → Failed + partieller Rollback (bereits gewirkte Effekte zurückgenommen); `"tool:"`-Satisfaction für eine Rolle mit fehlendem Tool → pending + Warnung; Render-Äquivalenz: Menge S inkrementell geladen ≡ Frisch-Bau mit S.

### Phase 3 — Loader, Selbst-Evolution, Work-Runtime

`reconcile.rs` + Profil-`components`-Sektion + `/compose apply`; Quiescence-Test `apply(A→B) ≡ fresh(B)`. `compose_list/enable/disable` hinter `--compose-tools` mit ApproveFn-Gating und dem Step-Grenzen-Commit in `agent.rs` (der einzige Kern-Hook). Work-Runner hält den langlebigen Kontext und rekonziliert an Attempt-Grenzen (Test: Attempt N läuft ohne MCP-Server X, Profiländerung, Attempt N+1 läuft mit — geskriptete FakeLlm, Assertion übers Journal). Graph-/Schwarm-Komponenten in agentkit_app (Schwarm-Disposer joint Member-Threads — Test mit 2-Member-FakeLlm-Schwarm, der den Thread-Drain vor dem Provider-Unload prüft). Optional viz-Markierungen.

### Roadmap-Tabelle

| Prio | Vorschlag | Abschnitt | Hängt ab von |
|---|---|---|---|
| **Phase 1** | `ToolRegistry::remove`/`names_with_prefix` | 5 (a) | — |
| **Phase 1** | MCP-Toggle inkrementell, Äquivalenztest | 5 (b) | remove |
| **Phase 1** | Extension-Seam formalisieren (2 Nutzer) | 5 (a) | remove |
| **Phase 1** | `extension_state`-Events | 5 (f) | — |
| **Phase 2** | Crate agentkit_compose (Component/Fiber/Ctx) | 4 | Phase 1 |
| **Phase 2** | MCP-/Skills-/Rollen-Komponenten, REPL/TUI-Umstellung | 5 (b)–(d) | Crate |
| **Phase 3** | Profil-Rekonziliation + Quiescence-Test | 5 (d) | Phase 2 |
| **Phase 3** | `compose_*`-Tools + Step-Grenzen-Commit | 5 (e) | Phase 2 |
| **Phase 3** | Work-Runner als Loader; Schwarm-/Graph-Komponenten | 5 (g)/(h) | Phase 2 |

---

## 8. Offene Punkte

- Die deklarativen Tool-Metadaten (P1 des alten Plans, `destructive`-Feld an `Contribution`) sind hier vorausgesetzt, aber nicht Teil dieses Plans — sie sollten vorher oder parallel landen.
- Ob die `components`-Profilsektion mittelfristig auch die Work-Projekt-Konfiguration erreicht (verschiedene Capability-Mengen je Projekt unter einem Runner), entscheidet sich am ersten realen Bedarf.
- agentkit ist ein Python-Port; `agentkit_compose` ist wie swarm/graph/work **kein** Port, sondern ein Neubau — die Kernberührungen (tools.rs, mcp.rs, agent.rs-Hook) sind bewusste Abweichungen und gehören in die „Bewusste Unterschiede zu Python"-Liste der agentkit-README, sobald implementiert wird.
