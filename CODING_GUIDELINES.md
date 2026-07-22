# Coding-Guidelines

Verbindliche Leitlinien für alle Rust-Crates in diesem Repo (`agent_framework_rs`, `ctxman_rs`). Sie ergänzen die technischen Konventionen in `CLAUDE.md` (Sprache, Build, Sync-ohne-Async, Offline-Default) um die **Gestaltungsprinzipien**: Wie soll Code hier aussehen?

Der Grundsatz über allem: **Einfachheit ist ein Feature.** Der beste Code ist der, der gar nicht geschrieben wird; der zweitbeste der, den man in einem Durchgang von oben nach unten lesen und verstehen kann.

## 1. Einfachheit zuerst

- Direkter, prozeduraler Code schlägt Indirektion. Eine Funktion, die ihren Ablauf sichtbar macht, ist besser als drei Schichten, die ihn verstecken.
- Kein Design-Pattern um des Patterns willen. Positivbeispiel im Repo: `Strategy` ist bewusst **nur ein System-Prompt-Preamble** (`src/agent.rs`), kein eigener Execution-Pfad mit Trait-Hierarchie — obwohl man das „sauber abstrahieren" könnte.
- Wer zwischen einer cleveren und einer langweiligen Lösung wählt, nimmt die langweilige.

## 2. Abstraktion nur bei nachgewiesenem Bedarf

- **Rule of Three:** Abstrahiert wird beim dritten konkreten Nutzer, nicht beim ersten geahnten.
- Ein Trait braucht **mindestens zwei reale Implementierungen**, um zu existieren. Vorbilder: `Llm` (OpenAI + `FakeLlm`), ctxmans `CompactionModel`/`BlobStore`. Ein Trait mit einer Implementierung „für später" wird zurückgebaut zu einem konkreten Typ.
- Wenn abstrahiert wird, dann an der **schmalsten Stelle**: kleine, synchrone Traits mit wenigen Methoden — nicht breite Interfaces, die alles durchreichen.
- Generics nur, wenn sie echten Nutzen bringen (z. B. `FnMut(AgentEvent)`-Sinks). Im Zweifel konkrete Typen.

## 3. Single Responsibility

- **Ein Modul = ein Thema, eine Funktion = ein Schritt.** Faustregel: Lässt sich eine Funktion nicht in einem Satz ohne „und" beschreiben, wird sie geteilt.
- Orchestrierende Funktionen (wie `Agent::drive_inner`) rufen benannte Schritte auf, statt Fachlogik inline zu tragen. Die laufende Entzerrung von `agent.rs` (Epic [#6](https://github.com/rudi77/agentkit_rs/issues/6)) ist der Maßstab: Streaming, Tool-Ausführung, Historien-Buchführung und Abbruchprüfung sind je ein eigener, testbarer Schritt.
- SRP heißt hier **nicht** „für jede Verantwortung eine neue Datei/Struct-Hierarchie": Eine private Funktion im selben Modul ist die bevorzugte erste Stufe der Trennung. Erst wenn ein Thema wächst, bekommt es ein eigenes Modul.
- Keine dogmatischen Zeilenzahl-Grenzen. Kriterium ist: Kann man den Block isoliert verstehen und testen?

## 4. Kein Over-Engineering (YAGNI)

- Keine Konfigurierbarkeit ohne konkreten zweiten Anwendungsfall. Jede Builder-Option, jedes Flag muss ein reales Bedürfnis abdecken — nicht ein hypothetisches.
- Keine neuen Dependencies ohne Not. Der Offline-/Zero-TLS-Default beider Crates ist ein Feature; jede Dependency muss ihn respektieren (optionales Feature-Gate) und ihren Platz verdienen.
- Keine async Runtime, keine Channels/Threads „auf Vorrat" — Nebenläufigkeit nur dort, wo sie heute gebraucht wird (`std::thread::scope` für parallele Tools ist das Muster).
- Fehlerbehandlung im richtigen Maß: weiche Fehler als Werte (`Ok("ERROR: …")`), harte als `Err` — keine eigenen Error-Trait-Hierarchien, solange `String`/kleine Enums reichen.

## 5. Lesbarkeit und Dokumentation

- Sprachkonvention gilt: **alles Benutzer-Sichtbare auf Deutsch**, Identifier englisch (siehe `CLAUDE.md`).
- Kommentare erklären das **Warum und die Invariante**, nie das Was. Vorbild aus `agent.rs`: „Wird zu Beginn jedes Laufs überschrieben; ein explizites Zurücksetzen ist unnötig, da …" — das ist ein guter Kommentar, weil der Code allein diese Begründung nicht zeigen kann.
- Doc-Comments an öffentlichen APIs dokumentieren **Verhalten und Kontrakte**: Fehlerkontrakt, Event-Reihenfolge, Randfälle. Nicht die Signatur in Prosa wiederholen.
- Code liest sich wie der umgebende Code: gleiche Kommentardichte, gleiche Idiome, gleiche Namensgebung.

## 6. Wartbarkeit

- **Port-Treue:** agentkit ist ein strukturelles 1:1-Port des Python-Originals, ctxman des C#-Services. Vor jedem Umbau das Gegenstück prüfen; Abweichungen bewusst treffen und im jeweiligen README („Bewusste Unterschiede …") dokumentieren — nie stillschweigend divergieren.
- **Verhaltenskontrakte sind API:** Exit-Codes, Event-Typ-Strings, das Stream-/Stdin-Verhalten der CLI, ctxmans Golden Fixtures. Sie werden nie beiläufig geändert; die Benchmark-Harness und die Frontends hängen daran.
- Änderungen klein und einzeln nachvollziehbar halten: ein Refactoring-Schritt pro Commit, Verhalten und Umbau nie im selben Commit mischen.

## 7. Performance

- **Messen vor Optimieren.** Es gibt ein `bench`-Binary (`cargo run --bin bench --release --no-default-features`) — spekulative Optimierungen ohne Messung werden nicht gemerged.
- Aber Grundhygiene kostet nichts: keine unnötigen Klone in heißen Pfaden, `Arc` statt Kopie wo geteilt wird (`ToolFn` ist das Muster), Allokationen aus Schleifen ziehen, wenn es die Lesbarkeit nicht verschlechtert.
- Lesbarkeit schlägt Mikro-Optimierung. Eine Optimierung, die den Code verkompliziert, braucht eine Messung, die sie rechtfertigt — als Kommentar oder im Commit dokumentiert.

## 8. Testbarkeit

- **Jede neue Fähigkeit ist offline testbar** — in agentkit über geskriptete `FakeLlm`-Sequenzen (`src/testing.rs`), in ctxman über die Orchestrations-/Golden-Tests. Kein Test berührt das Netz; das bleibt so.
- Ein Test prüft **ein Verhalten** und ist als Spezifikation lesbar. `ctxman_rs/tests/orchestration.rs` ist das Vorbild: Wer die Tests liest, kennt das Soll-Verhalten.
- Testbarkeit ist der eine legitime Grund, eine Abstraktion früher einzuziehen als Regel 2 erlaubt — aber über die **vorhandenen Nähte** (`Llm`-Trait, `FakeLlm`, Event-Sink), nicht über neue Mock-Frameworks oder zusätzliche Indirektion im Produktivcode.
- Golden Fixtures werden **nie regeneriert, um einen Test grün zu machen** — ein Byte-Diff dort bedeutet eine Verhaltensänderung, die verstanden werden muss.

## Review-Checkliste

Vor jedem Merge kurz prüfen:

1. Könnte das mit **weniger** Code/Abstraktion genauso gut funktionieren?
2. Hat jede neue Abstraktion heute ≥ 2 reale Nutzer (oder den Testbarkeits-Grund aus Regel 8)?
3. Beschreibt sich jede neue/geänderte Funktion in einem Satz ohne „und"?
4. Sind Warum-Kommentare da, wo der Code eine Invariante nicht selbst zeigen kann?
5. Python-/C#-Gegenstück geprüft, Abweichung dokumentiert?
6. Verhaltenskontrakte (Events, Exit-Codes, Fixtures) unangetastet?
7. Offline-Tests (`--no-default-features` bzw. ctxman-Tests) grün, neue Fähigkeit mit `FakeLlm` abgedeckt?
8. Performance-Behauptungen gemessen statt vermutet?
