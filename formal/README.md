# formal — Lean-Modelle der agentkit-Kernlogik

Kleine Lean-4-Modelle reiner Entscheidungslogik aus den Rust-Crates, mit
maschinell geprüften Beweisen ihrer Zusagen. Keine Abhängigkeit außer der
Lean-Toolchain selbst (kein Mathlib); die Rust-Crates wissen nichts von diesem
Verzeichnis.

## Bauen

```bash
cd formal
lake build      # Toolchain laut lean-toolchain; ein Fehler oder `sorry` heißt: Beweis fehlt
```

## Was bewiesen ist

| Datei | Rust-Vorbild | Sätze |
|---|---|---|
| `AgentkitFormal/WorkStatus.lean` | `WorkItemStatus::can_transition_to` (`agentkit_work/src/model.rs`) | `Completed` und `Canceled` sind endgültig; `AwaitingVerification` nur aus `Running`; `Completed` nur nach einem Versuch; keine Selbstschleife; jeder offene Status ist abbrechbar |
| `AgentkitFormal/Quorum.lean` | `quorum` / `peers_of` (`agentkit_swarm/src/dynamic.rs`) | Das Quorum übersteigt für keine Kantenliste die Nachbarzahl irgendeines Mitglieds (`quorum_erreichbar`); auf sauberen Kanten ist es gleich der früheren Kantenzählung (`quorum_eq_quorumAlt`) |

Das Quorum-Modell hat einen Fehler aufgedeckt: die frühere Fassung zählte Kanten
statt Nachbarn, eine doppelt angegebene Verbindung machte das Quorum
unerreichbar (Gegenbeispiel als `example` in der Datei, Regressionstest
`doppelte_verbindung_blaeht_das_quorum_nicht_auf`).

## Grenze

Bewiesen ist das Modell, nicht der Rust-Code — Lean liest kein Rust. Die Modelle
sind von Hand abgeschrieben; wer das Rust-Vorbild ändert, muss das Modell
nachziehen. Geeignet ist nur reine Logik (Zustandsmaschinen, Auswahlregeln,
Arithmetik), nichts mit Threads, Dateien oder LLM-Antworten.
