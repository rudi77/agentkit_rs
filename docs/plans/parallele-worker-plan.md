# Implementierungsplan: Parallele Worker in `agentkit_work`

## Stand der Umsetzung

Der Plan ist vollständig umgesetzt (Branch `ccr-0a9fb589-3rkpps`). Die ursprünglich in Phase 4
vorgesehene Handprobe mit echtem Modell ist gestrichen: sie brauchte einen Modell-Schlüssel, den es
in der Umsetzungsumgebung nicht gab, und wurde auf Entscheidung des Auftraggebers aus dem Umfang
genommen. Geprüft ist die Parallelität damit offline (`agentkit_work/tests/parallel.rs`) und über
die Executable mit dem Demo-Modell — nicht mit einem echten Modell. Wanduhrzeit, Rate-Limits und
Integrationskonflikte unter echter Last sind ungemessen.

Vier Stellen sind anders gebaut, als unten beschrieben — die Begründung steht jeweils im README von
`agentkit_work` (Abschnitte „Git-Isolation" und „Parallele Worker"):

| Plan | Gebaut | Grund |
|---|---|---|
| Rückhol-Logik für liegengebliebene Item-Branches entfernen | `recovery::recover_git_branch` und der Hinweis in `status`/`watch` bleiben | Ein Vorhaben, das vor der Umstellung hart abgebrochen ist, steht noch auf einem Item-Branch; ohne Rückholung ließe sich dessen Worktree nicht anlegen. |
| Die Prüfung „Planungsversuch hat nichts angelegt" zählt nur eigene Items | Eine laufende Zerlegung läuft exklusiv (`scheduler::decide`); der vorhandene Vergleich bleibt | Die Items eines Planers hängen nicht von ihm ab. Ohne Exklusivität finge ein zweiter Worker an, einen halben Plan auszuführen. Das ist der eigentliche Fehler; die Zählung war nur sein Symptom. |
| Fortschritt läuft über einen `Sender<WorkProgress>` als Parameter | Die `record_*`-Funktionen behalten `&mut dyn FnMut(WorkProgress)`; der Worker reicht eine Closure hinein, die in den Kanal sendet | Gleiche Wirkung, keine geänderten Signaturen. |
| `prune` beim Start in der CLI | `git::ensure_worktree` räumt vor jedem Anlegen auf | Eine Stelle statt zweier; ein von Hand gelöschter Worktree fällt genau dort auf, wo er stört. |

Dazugekommen ist, was der Plan nicht vorsah: Anlegen und Entfernen von Worktrees laufen unter
einer Prozess-Sperre (mehrere Worker tun es gleichzeitig), `status`/`watch` zeigen alle laufenden
Items, und nicht verfolgte Dateien des Workspace (etwa eine lokale `AGENTS.md`) fehlen im Worktree
— das steht als Grenze im README.

## Ziel

`agentkit work run --parallel N` arbeitet bis zu N Work Items gleichzeitig ab. Jedes schreibende
Item läuft in einem eigenen Git-Worktree, die Integration am Ende bleibt deterministisch. Mit
`N = 1` (Default) verhält sich ein Lauf exakt wie heute.

Das ist Baustein 1 auf dem Weg zu „viele Agenten arbeiten über Stunden oder Tage an einem
Vorhaben" (Leitszenario: Software-Fabrik). Alles Weitere — Modell-Scheduler, Token-Budget,
Auffächern, Schwarm als Planer und Prüfer — setzt darauf auf und ist hier ausdrücklich nicht
enthalten (siehe „Nicht im Umfang").

## Ausgangslage

Fast alles für lange Laufzeiten steht schon: Journal, Leases, Versuche, Retry, Budgets,
Verifikation, Resume. Parallelität fehlt, und zwar an genau vier Stellen.

| Stelle | Heute | Was Parallelität daran bricht |
|---|---|---|
| `runner::run_to_completion` | eine Schleife: `decide` → `run_attempt` (blockiert) → Checkpoint | Es gibt keinen Ort, an dem ein zweiter Versuch laufen könnte. `Decision::AtCapacity` wird als „Lauf steht" gedeutet und pausiert den Lauf. |
| `runner::GitAttemptCtx` | wechselt den Branch im EINEN Arbeitsbaum, `Drop` wechselt zurück | Zwei Versuche können nicht gleichzeitig zwei Branches auschecken. Rollback ist `git reset --hard` auf den ganzen Baum. |
| `AgentExecutor` / `on_progress` | `&dyn AgentExecutor` ohne `Sync`, `&mut dyn FnMut(WorkProgress)` | Beides lässt sich nicht über Threads teilen. `WorkProgress::Agent(ev)` trägt kein Item — die CLI rät es aus dem letzten `ItemStarted`. |
| `scheduler::decide` | zählt `Running` gegen `max_parallel_agents` | Kennt keinen Unterschied zwischen lesenden und schreibenden Items. Ohne Git-Isolation dürften zwei Schreiber nie gleichzeitig laufen. |

Was dagegen schon trägt und unverändert bleibt:

- **Der Store ist threadsicher.** `WorkStore` ist `RwLock<Arc<WorkState>>` plus ein
  serialisierender Schreiber; `submit_with` vergibt IDs im Schreiber-Lock.
- **Der Claim ist atomar.** `state::apply` lehnt `WorkItemClaimed` auf ein Item ab, das schon
  `Running` ist. Zwei Worker können dasselbe Item nicht doppelt bekommen.
- **`max_parallel_agents` existiert** in `WorkBudget` (Default 1), steht im Journal und in der
  JSON-Ausgabe. Ein Scheduler-Test fährt es schon mit 2.
- **Die Nähte des Executors sind `Send + Sync`:** `Arc<dyn Llm>`, `ApproveFn`, `ExtraTools`,
  `AgentSetup`, `Cancel`. `CodingAgentExecutor` braucht nur die Trait-Schranke, keine Umbauten.
- **`work.lock` bleibt.** Alle Worker leben in EINEM Prozess; mehrere Prozesse auf einem Projekt
  bleiben verboten. Deshalb bleibt auch `recovery::recover_all` beim Start richtig: ein Lease, das
  beim Öffnen existiert, gehört zu einem toten Prozess.
- **Versuche zählen erst beim Scheitern** (`attempt_count += 1` in `WorkItemFailed`). Ein Item,
  das gerade seinen letzten Versuch fährt, gilt dem Scheduler also nicht als „endgültig
  blockiert" — die Blockade-Erkennung in `decide` Schritt 5 bleibt mit N > 1 richtig.

## Entschiedene Architektur

**Ein Koordinator, N Worker-Threads, ein Kanal.** Der Koordinator ist der Thread, der
`run_to_completion` aufruft. Nur er befragt den Scheduler, claimt Items und ruft `on_progress`.
Worker führen je einen Versuch aus und melden Fortschritt und Ergebnis über einen
`std::sync::mpsc`-Kanal zurück.

```text
Koordinator (Aufrufer-Thread)                  Worker-Thread (je Versuch)
─────────────────────────────                  ──────────────────────────
loop:
  cancel? → nichts Neues starten
  decide(state)
    Run(id)  → claim(id)  ── spawn ──────────▶ Worktree vorbereiten
               in_flight += 1                  executor.execute(...)   ─┐ Fortschritt
               weiter mit decide               Ergebnis journalen       │ über Kanal
    sonst, in_flight > 0 → recv() ◀────────────────────────────────────┘
               Fortschritt → on_progress
               Fertig      → in_flight -= 1, Checkpoint, weiter mit decide
    sonst, in_flight == 0 → wie heute (fertig / pausiert / blockiert)
```

Vier Festlegungen dazu:

1. **`std::thread::scope`, kein Pool, keine Runtime.** Ein Thread je laufendem Versuch, gestartet
   im Scope von `run_to_completion`. Das ist das Muster, das die Guidelines §4 für Nebenläufigkeit
   nennen, und der Scope garantiert, dass kein Worker den Aufruf überlebt.
2. **Der Claim bleibt beim Koordinator.** Er wird synchron journalt, bevor `decide` erneut gefragt
   wird. Damit sieht die nächste Entscheidung das Item schon als `Running`, und `decide` bleibt
   eine reine Funktion des Zustands — kein „schon vergeben"-Set neben dem Journal.
3. **Eine Regel für alle Nicht-`Run`-Entscheidungen:** Läuft noch etwas, wartet der Koordinator
   auf das nächste Ergebnis und fragt neu. Läuft nichts mehr, gilt die Entscheidung wie heute.
   Das ersetzt die heutige Deutung von `AtCapacity` als Stillstand und deckt Budget, Freigabe-Halt
   und Blockade mit ab: bezahlte Arbeit läuft zu Ende, bevor der Lauf pausiert.
4. **Fehler bremsen, sie brechen nicht ab.** Liefert ein Worker einen harten `WorkError` (heute
   ein `?` aus `run_attempt`), startet der Koordinator nichts Neues, lässt die übrigen Versuche
   zu Ende laufen und gibt dann den ersten Fehler zurück. Ein Abbruch durch den Nutzer (`cancel`)
   wirkt wie heute sofort auf alle Agenten; jeder betroffene Versuch wird als unterbrochen
   journalt und zählt nicht gegen `max_attempts`.

### Git: Worktree statt Branchwechsel

Unter `--git-isolation` bekommt jedes schreibende Item einen eigenen Worktree auf seinem Branch
`work/<projekt-id>/<item-id>`. Der Arbeitsbaum des Nutzers wird bis zur Integration nicht mehr
angefasst.

- **Ort:** `<store-verzeichnis>/worktrees/<item-id>`, also im Regelfall unter
  `<workspace>/.agentkit/work/<projekt-id>/`. `.agentkit` ist schon von `git status` und vom
  Staging ausgenommen. Das Layout steht so in §23 des Ursprungsplans.
- **Workspace des Agenten** ist der Worktree, plus der relative Pfad des Projekt-Workspace im
  Repo (`git rev-parse --show-prefix`), falls der ein Unterordner ist.
- **Lebensdauer:** angelegt beim ersten Versuch, wiederverwendet bei jedem Retry, entfernt, sobald
  das Item terminal ist (`git worktree remove --force`). Der Branch bleibt für die Integration
  und die Nachverfolgung.
- **Vorgänger:** Das Hineinmergen der Vorgänger-Branches (`GitAttemptCtx::prepare`) bleibt
  dieselbe Logik, nur mit dem Worktree als Arbeitsverzeichnis.
- **Rollback** (`reset --hard`, `clean`) trifft nur noch den Worktree des Items.
- **Integration** bleibt unverändert: Sie läuft, wenn alle Items terminal sind, also wenn kein
  Worker mehr arbeitet, und mergt im Arbeitsbaum des Nutzers.

**Ein Pfad statt zwei.** Worktrees ersetzen den Branchwechsel auch bei `N = 1`. Zwei
Isolationsmechanismen nebeneinander wären doppelter Code und doppelte Tests für dasselbe Ergebnis.
Die Branch-Namen bleiben gleich, ein laufendes älteres Vorhaben setzt also nahtlos fort.

Das löst nebenbei drei dokumentierte Schwächen des heutigen Stands:

- Die Prüfung „Repo außerhalb des Workspace sauber" entfällt vor jedem Versuch, weil der Rollback
  fremde Arbeit nicht mehr erreichen kann. Der Arbeitsbaum des Nutzers muss nur noch für die
  Integration sauber sein (geprüft beim Start des Laufs und direkt vor dem Merge).
- Ein hart abgestürzter Prozess lässt den Arbeitsbaum des Nutzers nicht mehr auf einem Item-Branch
  stehen. Die Rückhol-Logik in der CLI (`git_stray_item_branch_note` und Umfeld) wird überflüssig;
  an ihre Stelle tritt `git worktree prune` beim Start.
- Der `Drop`-Guard, der zurückwechselt, entfällt.

Der Grund, aus dem Phase 7 Worktrees zurückgestellt hat, muss dafür gelöst werden: **Artefakte der
Vorgänger liegen außerhalb des Worktrees**, und `read_file` ist auf den Workspace eingehegt. Die
Naht dafür existiert schon — `CodingAgentExecutor::allow_read` (nur lesbare Sandbox-Wurzeln). Der
Runner gibt dem Item-Agenten das Artefaktverzeichnis als Lese-Wurzel mit, und das Arbeitspaket
nennt die Pfade so, dass `read_file` sie auflöst. Das bekommt einen eigenen Test.

### Ohne Git-Isolation: ein Schreiber

Ohne `--git-isolation` teilen sich alle Items einen Arbeitsbaum. Dann gilt die Regel aus §19 des
Ursprungsplans: höchstens EIN schreibendes Item läuft gleichzeitig, lesende (`Review`, `Planning`)
dürfen daneben laufen. Das ist eine reine Eigenschaft des Zustands und gehört deshalb in
`scheduler::decide`: Läuft schon ein schreibendes Item und ist Isolation aus, überspringt Schritt 4
schreibende Kandidaten.

`WorkItemKind::is_git_isolated` beantwortet heute schon „gilt diese Art als schreibend?". Die
Methode bekommt den Namen, der zu beiden Nutzern passt (`writes_workspace`).

### Executor und Fortschritt

- `AgentExecutor` bekommt die Schranke `Sync`, `ExecutorBuilder` liefert
  `Box<dyn AgentExecutor>` entsprechend. `CodingAgentExecutor`, `SwarmWorkExecutor`,
  `DispatchingExecutor` und der `ScriptedExecutor` der Tests (hält `Mutex` und `AtomicUsize`)
  erfüllen sie voraussichtlich ohne Umbau; der Compiler zeigt jede Ausnahme.
- `WorkProgress::Agent(AgentEvent)` wird zu `Agent { item, attempt, event }`. Die CLI baut daraus
  dasselbe Trace-Tag wie heute (`W-1#2`), der Betrachter (`agentkit_viz`) braucht keine Änderung.
  Die Variable `laufendes_item` in `cmd_run` entfällt.
- Die Lease-Verlängerung bleibt im Event-Callback des Versuchs, jetzt im Worker-Thread. Der
  Callback schickt das Ereignis danach in den Kanal.
- Jeder Worker bekommt eine eigene `agent_id` (`worker-1` … `worker-N`), damit `work items` und
  das Journal zeigen, wer was gefahren hat.
- **Freigabe-Rückfragen** (`ApproveFn` ohne `-y`) würden sich mit mehreren Agenten auf dem
  Terminal überlagern. `cmd_run` hüllt den Callback bei N > 1 in einen `Mutex`: eine Frage zur
  Zeit. Welches Item fragt, zeigt die Rückfrage dabei nicht — `ApproveFn` bekommt nur das
  Kommando. Für autonome Läufe ist `-y` oder eine Allowlist ohnehin der Normalfall.

### Bedienung

- `agentkit work run --parallel N` und `agentkit work budget --max-parallel N` setzen
  `max_parallel_agents` über das vorhandene `BudgetUpdated`.
- Der Default bleibt 1. Es gibt keine eingebaute Obergrenze: Die knappe Ressource ist das
  Modell-Kontingent, und das kennt nur der Bediener.
- Die Fortschrittszeilen auf stderr tragen schon die Item-ID; die `--steps`-Zeilen bekommen sie
  als Präfix dazu.

### Der Plan zeigt, wie viele Agenten er beschäftigen kann

Der Bediener muss die Teamgröße nicht vorab kennen. Den Ausführungsplan erstellt das
Planungs-Item (`work run --mode plan`), und aus dessen Abhängigkeiten folgt, wie viele Items
gleichzeitig laufen können. `--parallel N` ist nur die Obergrenze, die sich nach
Modell-Kontingent, Kosten und Plattenplatz richtet; ein zu großes N bleibt ungenutzt.

Damit diese Wahl eine Grundlage hat, nennt die Plananzeige zwei Zahlen:

```text
Plan zur Freigabe (9 Items):
  …
Bis zu 4 Items gleichzeitig, 3 Stufen nacheinander.
```

- **Stufe** eines Items ist die Länge der längsten Abhängigkeitskette, die zu ihm führt (ein Item
  ohne Vorgänger hat Stufe 1). Die Zahl der Stufen ist die längste Kette des Plans.
- **Gleichzeitig** ist die größte Zahl von Items auf einer Stufe.

Das ist bewusst die einfache Rechnung über Stufen und nicht die exakte größte Menge voneinander
unabhängiger Items. Sie stimmt, wenn Items einer Stufe etwa gleich lang laufen, und ist für die
Wahl von N genau genug. Ohne Git-Isolation sagt die Anzeige dazu, dass schreibende Items trotzdem
nacheinander laufen.

Die Rechnung ist eine reine Funktion des Zustands (`WorkState`), ohne I/O, und steht deshalb neben
`ready_items`. Angezeigt wird sie in der Plan-Freigabe (`cli::print_plan_review`) und in
`work status`.

Der Planer soll außerdem auf Parallelität hin zerlegen. Die Planungsvorlage
(`runner::create_plan_item`) bekommt zwei Sätze dazu:

- Arbeit, die nicht aufeinander aufbaut, wird in eigene Items ohne Abhängigkeit geschnitten.
- Items, die dieselben Dateien ändern, bekommen eine Abhängigkeit untereinander, auch wenn sie
  fachlich getrennt sind — sonst kollidieren sie bei der Integration.

## Phasen

Jede Phase ist für sich mergebar und lässt alle bestehenden Tests grün. Umbau und neues Verhalten
landen in getrennten Commits (Guidelines §6).

### Phase 1 — Git-Isolation auf Worktrees (weiter ein Worker)

1. `git.rs`: `ensure_worktree`, `remove_worktree`, `prune_worktrees`, `workspace_prefix`. Tests im
   Wegwerf-Repo wie die vorhandenen.
2. `GitAttemptCtx` hält den Worktree-Pfad statt des vorherigen Branches; `prepare`, `commit` und
   `discard` arbeiten dort. `Drop` und die Sauberkeitsprüfungen je Versuch entfallen.
3. `run_attempt` reicht den Worktree als `pkg.workspace` an den Executor und als
   Arbeitsverzeichnis an `run_verification_command`.
4. Artefaktverzeichnis als Lese-Wurzel für den Item-Agenten; Test: ein abhängiges Item liest das
   Artefakt seines Vorgängers aus dem Worktree heraus.
5. CLI: Sauberkeitsprüfung beim Start und vor der Integration, `prune` beim Start, Rückhol-Logik
   für liegengebliebene Item-Branches entfernen.
6. `tests/git_dependencies.rs` und die Git-Tests in `tests/runner.rs` laufen unverändert in ihren
   Aussagen (Vorgänger sichtbar, Konflikt führt zu `Blocked`, Rollback nach Fehlversuch).

Zu prüfen in dieser Phase, weil der Agent jetzt in einem anderen Verzeichnis arbeitet:

- **Wissensgraph:** Die `graph_*`-Tools und der Recall müssen weiter das Projekt-Bundle unter
  `<workspace>/.agentkit/graph` treffen, nicht ein leeres im Worktree.
- **Kontext-Management:** `agent_setup` legt Zustand je Versuch ab; der Schlüssel `<item>-<n>`
  ist schon eindeutig, das Verzeichnis darf nicht am Workspace hängen.
- **Schwarm-Items:** `SwarmWorkExecutor` nimmt `pkg.workspace`; alle Mitglieder eines Schwarms
  teilen sich damit den Worktree ihres Items.

### Phase 2 — Runner zerlegen (weiter ein Worker, kein neues Verhalten)

1. `run_attempt` in drei benannte Schritte teilen: `claim_attempt` (journalt den Claim, liefert
   die Versuchsdaten), `execute_attempt` (Worktree, Executor, Zählen, Lease) und `record_attempt`
   (die heutige Ergebnis-Auswertung).
2. `AgentExecutor: Sync`; `WorkProgress::Agent { item, attempt, event }`; CLI und Trace anpassen.
3. Fortschritt aus `execute_attempt` und `record_attempt` läuft über einen `Sender<WorkProgress>`
   statt über `&mut dyn FnMut`. Bei einem Worker leert der Koordinator den Kanal im selben
   Ablauf wie heute.

### Phase 3 — Der Worker-Pool

1. Koordinator-Schleife wie oben skizziert, in `std::thread::scope`.
2. `scheduler::decide`: Schreiber-Regel ohne Git-Isolation; `is_git_isolated` umbenennen.
3. `agent_id` je Worker; Kommentare und Doku von `recover_all`, `AtCapacity` und der Moduldoku
   des Runners auf den neuen Stand bringen.
4. CLI: `--parallel`, `--max-parallel`, `Mutex` um die Freigabe.
5. Die Prüfung „Planungsversuch hat nichts angelegt" vergleicht heute die Item-Zahl vor und nach
   dem Versuch. Parallel können andere Agenten währenddessen Items anlegen; die Prüfung zählt
   deshalb nur Items, die dieser Versuch selbst angelegt hat.
6. Stufen-Rechnung in `state.rs`; Anzeige in der Plan-Freigabe und in `work status`.
7. Planungsvorlage um die zwei Sätze zur Parallelität ergänzen.

Tests (alle offline, `ScriptedExecutor`):

- **Plan-Breite:** Zehn unabhängige Items ergeben „10 gleichzeitig, 1 Stufe", eine Kette aus zehn
  „1 gleichzeitig, 10 Stufen", eine Raute (A, dann B und C, dann D) „2 gleichzeitig, 3 Stufen".

- **Echte Gleichzeitigkeit:** Zwei unabhängige Items, der Executor blockiert an einer
  `std::sync::Barrier`, bis beide Versuche gestartet sind. Der Test besteht nur, wenn sie sich
  überlappen, und braucht keinen Sleep.
- **Kapazität:** Mit N = 2 und drei bereiten Items laufen nie mehr als zwei zugleich.
- **Abhängigkeiten:** Ein abhängiges Item startet erst nach Abschluss seines Vorgängers, auch
  wenn ein Worker frei ist.
- **Schreiber-Regel:** Ohne Isolation laufen zwei schreibende Items nacheinander, ein Review
  daneben parallel.
- **Git parallel:** Zwei Items ändern verschiedene Dateien, beide landen nach der Integration im
  Ausgangsbranch. Zwei Items ändern dieselbe Zeile, der Lauf endet `Blocked` mit den
  Konfliktdateien.
- **Abbruch:** `cancel` während zwei Versuche laufen; beide werden als unterbrochen journalt,
  der Lauf pausiert, ein Resume setzt beide fort.
- **Harter Fehler:** Ein Worker liefert `WorkError`; der zweite Versuch läuft zu Ende und wird
  journalt, danach kommt der Fehler zurück.
- **Budget:** Läuft das Wall-Time-Budget ab, während Versuche laufen, pausiert der Lauf erst
  nach deren Ende.
- **N = 1:** Die bestehende Suite bleibt unverändert grün.

### Phase 4 — Doku

Die Handprobe mit echtem Modell, die hier ursprünglich als erster Schritt stand, ist gestrichen
(siehe „Stand der Umsetzung").

1. `agentkit_work/README.md`: „Grenzen des heutigen Stands", „Git-Isolation" und „Bewusste
   Design-Entscheidungen" (Worktrees statt Branchwechsel, Koordinator-Modell, Schreiber-Regel,
   Abweichung vom Ursprungsplan dokumentieren).
2. Wurzel-`CLAUDE.md`: Die Tabellenzeile zu `agentkit_work` nennt heute eine „synchronous
   single-worker loop".
3. `agentkit_viz`: prüfen, dass der Work-Tab mehrere gleichzeitig laufende Items richtig zeigt.

## Nicht im Umfang

- **Modell-Scheduler und Token-Budget.** Jeder Agent wiederholt ein 429 heute für sich mit
  Backoff. Das funktioniert bei kleinem N und wird bei großem N zum Engpass. Das ist der nächste
  Baustein. Ab welchem N er nötig wird, ist nicht gemessen.
- **Mehrere Prozesse oder Maschinen** auf einem Projekt. `work.lock` bleibt.
- **Wachhund für hängende Worker.** Ein Versuch, der keine Ereignisse mehr erzeugt, blockiert wie
  heute seinen Platz; sein Lease läuft ab, aber niemand gibt es im laufenden Prozess frei.
- **Inkrementelle Integration und automatische Konfliktauflösung.** Konflikte zeigen sich weiter
  erst am Ende des Laufs. Mit mehr parallelen Branches wird das wahrscheinlicher; es ist das
  größte bekannte Risiko dieses Plans; gemessen ist es nicht.
- **Prüf-Items sehen den Diff.** Mit Worktrees wird das einfach (ein Prüfer bekommt den Worktree
  des geprüften Items nur lesbar), gehört aber in einen eigenen Schritt.
- **Neuplanung im laufenden Vorhaben.** Der Plan steht nach der Zerlegung fest; Agenten können
  Items nachlegen, aber eine geordnete Überarbeitung des Plans gibt es nicht.
- **Der Planer wählt Schwärme.** Einen Schwarm je Item weist weiter nur der Bediener zu.
- **Auffächern, Daemon-Betrieb, Schwarm als Planer und Prüfer.**

## Risiken

- **Plattenplatz und kalte Builds.** Jeder Worktree ist eine volle Arbeitskopie und baut in sein
  eigenes Build-Verzeichnis. Bei einem Rust-Projekt mit N = 8 sind das acht `target/`-Ordner.
  Worktrees terminaler Items werden entfernt; mehr unternimmt dieser Plan nicht.
- **Konflikte am Ende** (siehe oben).
- **Rate-Limits** (siehe oben).
- **Worktree unter `.agentkit`.** Ein Worktree innerhalb des Haupt-Arbeitsbaums ist für Git ein
  nicht verfolgtes Verzeichnis. Die vorhandenen Ausschlüsse decken das ab; der erste Schritt von
  Phase 1 prüft es mit einem Test, bevor der Rest darauf baut. Fällt er durch, weicht der Ort auf
  ein Verzeichnis neben dem Repo aus.

## Verifikation

```bash
cargo test   --manifest-path agentkit_work/Cargo.toml
cargo clippy --manifest-path agentkit_work/Cargo.toml --all-targets
cargo build  --manifest-path agentkit_work/Cargo.toml --features "openai ctxman"
cargo test   --manifest-path agentkit_app/Cargo.toml --no-default-features --features work
cargo test   --manifest-path agentkit_app/Cargo.toml --no-default-features --features "viz work graph"
cargo test   --manifest-path agentkit_viz/Cargo.toml --features "work graph"
```
