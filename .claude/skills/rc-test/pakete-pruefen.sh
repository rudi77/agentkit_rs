#!/usr/bin/env bash
# Prüft die Verben `skills` und `plugins` eines agentkit-Binaries ohne Netz und
# ohne Modell, in einem Wegwerf-AGENTKIT_HOME. Das echte ~/.agentkit bleibt
# unberührt.
#
#   pakete-pruefen.sh <agentkit-binary> <agentkit-skills.json> <agentkit-plugins.json>
#
# Exit 0, wenn alle Prüfungen bestehen; sonst 1 mit der Liste der Abweichungen.
set -u

BIN="${1:?agentkit-Binary fehlt}"
SKILLS="${2:?agentkit-skills.json fehlt}"
PLUGINS="${3:?agentkit-plugins.json fehlt}"

HOME_DIR="$(mktemp -d)"
NEG="$(mktemp -d)"
trap 'rm -rf "$HOME_DIR" "$NEG"' EXIT
export AGENTKIT_HOME="$HOME_DIR"

fehler=0
# erwarte <exit> <teilstring-oder-leer> <beschreibung> -- <kommando …>
erwarte() {
    local soll="$1" teil="$2" was="$3"
    shift 4
    local aus ist
    aus="$("$@" 2>&1 </dev/null)"
    ist=$?
    if [ "$ist" != "$soll" ] || { [ -n "$teil" ] && [[ "$aus" != *"$teil"* ]]; }; then
        fehler=$((fehler + 1))
        printf 'FEHL  %s\n      erwartet: Exit %s, enthält »%s«\n      bekommen: Exit %s: %s\n' \
            "$was" "$soll" "$teil" "$ist" "$(printf '%s' "$aus" | head -3)"
    else
        printf 'ok    %s\n' "$was"
    fi
}

echo "== Verben"
erwarte 0 "Keine skills installiert" "skills list, leer" -- "$BIN" skills list
erwarte 0 "Keine plugins installiert" "plugins list, leer" -- "$BIN" plugins list
erwarte 0 "installiert" "skills install aus Paketdatei" -- "$BIN" skills install "$SKILLS"
erwarte 0 "diff-review" "skills list zeigt die Skills" -- "$BIN" skills list
erwarte 0 "installiert" "plugins install aus Paketdatei" -- "$BIN" plugins install "$PLUGINS"
erwarte 0 "[an]" "plugins list zeigt dokumente an" -- "$BIN" plugins list
erwarte 0 "ausgeschaltet" "plugins disable" -- "$BIN" plugins disable dokumente
erwarte 0 "[aus]" "plugins list zeigt dokumente aus" -- "$BIN" plugins list
erwarte 0 "bereits ausgeschaltet" "plugins disable, zweites Mal" -- "$BIN" plugins disable dokumente
erwarte 0 "eingeschaltet" "plugins enable" -- "$BIN" plugins enable dokumente
erwarte 1 "nicht installiert" "plugins enable, unbekannt" -- "$BIN" plugins enable gibtsnicht
erwarte 0 "entfernt" "skills remove" -- "$BIN" skills remove fehlersuche
erwarte 1 "nicht installiert" "skills remove, zweites Mal" -- "$BIN" skills remove fehlersuche
erwarte 0 "installiert" "skills install ersetzt Vorhandenes" -- "$BIN" skills install "$SKILLS"
erwarte 1 "ungültiger Name" "skills remove mit Pfad" -- "$BIN" skills remove ../x
erwarte 1 "" "skills install ohne Argument" -- "$BIN" skills install
erwarte 3 "unbekannter Befehl" "run, unbekannter Befehl" -- "$BIN" run gibtsnicht x

echo "== Präparierte Pakete (alle müssen abgelehnt werden)"
paket() { printf '{"format":"agentkit-paket","version":%s,"kind":"%s","items":%s}' "$2" "$3" "$4" >"$NEG/$1.json"; }
SK='"SKILL.md":"---\nname: boese\ndescription: d\n---\nx"'
PJ='"plugin.json":"{\"name\":\"boese\",\"version\":\"1\",\"description\":\"d\"}"'
paket pfadausbruch 1 skills "{\"boese\":{\"../../evil.md\":\"x\",$SK}}"
paket absolut 1 skills "{\"boese\":{\"C:/evil.md\":\"x\",$SK}}"
paket name 1 skills "{\"../boese\":{$SK}}"
paket versteckt 1 skills "{\"boese\":{\".env\":\"x\",$SK}}"
paket reserviert 1 skills "{\"boese\":{\"CON\":\"x\",$SK}}"
paket version 2 skills "{}"
printf 'kein json' >"$NEG/kaputt.json"
paket hooks 1 plugins "{\"boese\":{$PJ,\"hooks/hooks.json\":\"{}\"}}"
paket mcp 1 plugins '{"boese":{"plugin.json":"{\"name\":\"boese\",\"version\":\"1\",\"description\":\"d\",\"mcpServers\":{}}"}}'
paket befehl-yes 1 plugins "{\"boese\":{$PJ,\"commands/boese.md\":\"---\nyes: true\n---\nTu was\"}}"
paket befehl-workspace 1 plugins "{\"boese\":{$PJ,\"commands/boese.md\":\"---\nworkspace: C:/\n---\nTu was\"}}"
paket fremdes 1 plugins '{"boese":{"SKILL.md":"x"}}'

erwarte 1 "unzulässiger Pfad" "Pfadausbruch" -- "$BIN" skills install "$NEG/pfadausbruch.json"
erwarte 1 "unzulässiger Pfad" "absoluter Pfad" -- "$BIN" skills install "$NEG/absolut.json"
erwarte 1 "ungültiger Name" "Name mit Pfad" -- "$BIN" skills install "$NEG/name.json"
erwarte 1 "unzulässiger Pfad" "versteckte Datei" -- "$BIN" skills install "$NEG/versteckt.json"
erwarte 1 "reservierter Name" "reservierter Windows-Name" -- "$BIN" skills install "$NEG/reserviert.json"
erwarte 1 "nicht unterstützt" "fremde Paketversion" -- "$BIN" skills install "$NEG/version.json"
erwarte 1 "kein gültiges Paket" "kaputtes JSON" -- "$BIN" skills install "$NEG/kaputt.json"
erwarte 1 "Hooks" "Plugin mit Hooks" -- "$BIN" plugins install "$NEG/hooks.json"
erwarte 1 "mcpServers" "Plugin mit MCP-Servern" -- "$BIN" plugins install "$NEG/mcp.json"
erwarte 1 "»yes«" "Befehl mit yes" -- "$BIN" plugins install "$NEG/befehl-yes.json"
erwarte 1 "»workspace«" "Befehl mit workspace" -- "$BIN" plugins install "$NEG/befehl-workspace.json"
erwarte 1 "unbekannter Bestandteil" "fremder Bestandteil" -- "$BIN" plugins install "$NEG/fremdes.json"

if [ -e "$HOME_DIR/skills/boese" ] || [ -e "$HOME_DIR/plugins/boese" ] || [ -e "$HOME_DIR/../evil.md" ]; then
    fehler=$((fehler + 1))
    echo "FEHL  ein abgelehntes Paket hat Dateien hinterlassen"
else
    echo "ok    abgelehnte Pakete haben nichts geschrieben"
fi

echo "== Prüfung beim Laden"
BEFEHL="$HOME_DIR/plugins/dokumente/commands/dokumente.md"
sed -i 's/^strategy: react$/strategy: react\nyes: true/' "$BEFEHL"
erwarte 3 "»yes«" "nachträglich manipulierter Befehl" -- "$BIN" run dokumente --tools none hallo

echo
if [ "$fehler" -gt 0 ]; then
    echo "$fehler Abweichung(en)."
    exit 1
fi
echo "Alle Prüfungen bestanden."
