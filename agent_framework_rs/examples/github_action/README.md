# agentkit als GitHub Action

agentkit läuft als ein statisches Binary und hält einen festen Exit-Code-Vertrag ein. Damit
passt es ohne Umbau in einen Workflow: Die Action lädt das Release-Binary, führt einen Auftrag
aus, legt die Antwort als Output `result` ab und hängt sie auf Wunsch als Kommentar an den
Pull Request bzw. das Issue.

```yaml
- uses: actions/checkout@v4
- uses: rudi77/agentkit_rs@main
  with:
    provider: anthropic
    anthropic-api-key: ${{ secrets.ANTHROPIC_API_KEY }}
    prompt: "Aktualisiere die Versionsangaben in README.md auf 2.0."
```

Zwei vollständige Workflows zum Kopieren nach `.github/workflows/`:

| Datei | Auslöser | Was passiert |
|---|---|---|
| [`pr-review.yml`](pr-review.yml) | Pull Request geöffnet/aktualisiert | Review des Diffs als PR-Kommentar, nur lesend (`allow-shell: "false"`) |
| [`issue-fix.yml`](issue-fix.yml) | Label `agentkit` an einem Issue | Umsetzung auf eigenem Branch, Tests, Pull Request |

## Inputs

| Input | Default | Bedeutung |
|---|---|---|
| `prompt` | — | der Auftrag (Pflicht) |
| `provider` | `auto` | `auto` \| `azure` \| `openai` \| `anthropic` \| `demo` |
| `model` | leer | Modell bzw. Azure-Deployment |
| `anthropic-api-key`, `openai-api-key`, `openai-base-url`, `azure-openai-*` | leer | Zugangsdaten; nur ausgefüllte werden weitergegeben |
| `workspace` | `.` | Sandbox des Agenten |
| `allow-shell` | `"true"` | `run_shell` ohne Rückfrage (`-y`); im Runner fragt niemand nach |
| `max-steps` | `100` | Obergrenze der Loop-Schritte |
| `token-limit` | leer | Abbruch ab N gemessenen Tokens (Kostenbremse) |
| `extra-args` | leer | weitere Optionen, z. B. `--plan --verify` |
| `comment` | `"false"` | Antwort als Kommentar an PR/Issue |
| `version` | `latest` | Release-Tag, z. B. `v0.31.0` |

Outputs: `result` (die Antwort) und `exit-code` (`0` ok, `1` Laufzeit, `2` API/Netz,
`3` Kontext). Bei einem Exit-Code ungleich 0 schlägt der Schritt fehl.

## Sicherheit

- **Der Prompt ist Eingabe von außen.** Die Action reicht ihn über die Umgebung an agentkit
  weiter, nie per Textersetzung in ein Shell-Skript. Ein Issue-Titel kann also keinen
  Shell-Code in den Runner schmuggeln. Was er dem **Modell** sagt, ist eine andere Frage:
  Mit `allow-shell: "true"` darf der Agent Befehle ausführen. Deshalb löst `issue-fix.yml`
  nur über ein Label aus, das allein Mitglieder mit Schreibrecht setzen können.
- **Keine Secrets für fremde Forks.** `pull_request` bekommt bei Forks keine Secrets, und das
  ist richtig so. Niemals auf `pull_request_target` umstellen und dabei den Fork-Code
  auschecken.
- **Kosten deckeln.** `token-limit` bricht einen Lauf ab, sobald der gemessene Verbrauch
  über der Grenze liegt.
