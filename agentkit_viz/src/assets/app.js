// agentkit viz — bewusst ohne Framework und ohne npm-Toolchain: eine Datei,
// `fetch` und ein Polling-Takt. Wer das Werkzeug erweitern will, soll es lesen
// können, ohne einen Build zu starten.
//
// Wichtigste Regel im Live-Betrieb: NICHTS neu zeichnen, was der Nutzer gerade
// aufgeklappt hat. Neue Ereignisse werden ANGEHÄNGT (/api/events?since=…), nicht
// durch einen Neuaufbau der Ansicht nachgeholt — sonst schnappt jedes geöffnete
// Tool-Ergebnis im Sekundentakt wieder zu, genau während man es liest.

const TOKEN = new URLSearchParams(location.search).get("t") || "";

const REITER = [
  { id: "verlauf", titel: "Verlauf" },
  { id: "kontext", titel: "Kontext" },
  { id: "zeitleiste", titel: "Zeitleiste" },
  { id: "schwarm", titel: "Schwarm" },
  { id: "graph", titel: "Graph" },
  { id: "work", titel: "Work" },
  { id: "benchmarks", titel: "Benchmarks" },
];

const zustand = {
  reiter: "verlauf",
  agent: null,        // null = noch keiner gewählt
  agenten: [],
  lastSeq: 0,         // bis hierher sind Ereignisse VERARBEITET (nicht nur geholt)
  projekt: null,
  // Die Trace-Datei, deren Ereignisse gerade angezeigt werden. Der Wechsel auf
  // eine andere ist der einzige Anlass, bei dem `lastSeq` zurückgesetzt werden
  // darf — die Sequenznummern zweier Läufe fangen beide bei 1 an.
  lauf: null,
  laeuft: false,      // ein `tick` ist unterwegs (siehe dort)
  graphRevision: null, // Stand, mit dem der Graph-Reiter gezeichnet wurde
  // Kontext-Reiter. Diese Stände überleben das Neuzeichnen, das jeder neue
  // Kontext-Datensatz auslöst — sonst sprängen Auswahl und Suche im Takt zurück.
  kontextBis: null,      // Zeitreise: seq des gezeigten Datensatzes, null = aktuell
  kontextAuswahl: null,  // im Inspektor festgehaltener Block
  kontextTote: false,    // verdichtete/verdrängte Segmente zeigen
  kontextRoutine: false, // Routine-Ereignisse (angehängt, gerendert) im GC-Strom
  kontextSuche: "",
};

/// Ein Wechsel von Agent oder Trace-Datei macht Zeitreise und Auswahl
/// bedeutungslos — sie gehören zu genau einem Kontext.
function kontextZuruecksetzen() {
  zustand.kontextBis = null;
  zustand.kontextAuswahl = null;
  zustand.kontextSuche = "";
}

async function api(pfad, params = {}) {
  // `run` bei JEDER Anfrage: der Server soll für alle Endpunkte dieselbe Datei
  // lesen, auch wenn nebenher ein zweiter Lauf eine neuere anlegt.
  const mit = zustand.lauf ? { run: zustand.lauf, ...params } : params;
  const q = new URLSearchParams({ ...mit, t: TOKEN });
  const antwort = await fetch(`${pfad}?${q}`);
  const text = await antwort.text();
  let daten;
  try {
    daten = JSON.parse(text);
  } catch {
    throw new Error(`${antwort.status}: ${text.slice(0, 200)}`);
  }
  if (!antwort.ok) throw new Error(daten.error || `${antwort.status}`);
  return daten;
}

// ------------------------------------------------------------------ Helfer

const el = (tag, klasse, text) => {
  const n = document.createElement(tag);
  if (klasse) n.className = klasse;
  if (text !== undefined) n.textContent = text;
  return n;
};

const zeit = (ms) => new Date(ms).toLocaleTimeString("de-DE", { hour12: false });

const kurz = (s, n = 120) => {
  const flach = String(s ?? "").replace(/\s+/g, " ");
  return flach.length > n ? flach.slice(0, n) + "…" : flach;
};

const leer = (text) => el("p", "leer", text);

// Zwischenstufen, die in JEDEM Sitzungspfad vorkommen und deshalb nichts
// unterscheiden.
const SITZUNG_RAUSCHEN = new Set(["agent", "trace", "logs", ".agentkit"]);

/// Der Anzeigename einer Sitzung. Die Kennung ist der volle relative Pfad, aber
/// als Beschriftung ist er unbrauchbar lang: bei einem Benchmark-Baum steckt die
/// Information im Trial-Verzeichnis (`polyglot_python_beer-song__q9Lwpmz`) —
/// nicht in den immer gleichen Zwischenstufen und nicht im Dateinamen aus PID
/// und Zeitstempel. Der volle Pfad bleibt als Tooltip erreichbar.
const kurzeSitzung = (name) => {
  const teile = String(name).split("/");
  const datei = teile.pop() || String(name);
  return teile.filter((t) => !SITZUNG_RAUSCHEN.has(t)).pop() || datei;
};

/// Die Art der Nutzlast (`tool_call`, `final`, …) und ihr Inhalt.
function nutzlast(data) {
  if (typeof data === "string") return [data, null];
  const art = Object.keys(data)[0];
  return [art, data[art]];
}

// ------------------------------------------------------------------ Gerüst

function zeichneReiter() {
  const nav = document.getElementById("reiter");
  nav.textContent = "";
  for (const r of REITER) {
    const b = el("button", zustand.reiter === r.id ? "aktiv" : "", r.titel);
    b.onclick = () => { zustand.reiter = r.id; zeichneReiter(); zeichneInhalt(); };
    nav.appendChild(b);
  }
}

function zeichneAgenten() {
  const ul = document.getElementById("agenten");
  ul.textContent = "";
  for (const a of zustand.agenten) {
    const li = el("li", zustand.agent === a.id ? "aktiv" : "");
    li.appendChild(el("span", `marke ${a.kind}`,
      { haupt: "haupt", work_item: "work", sub_agent: "sub" }[a.kind] || "schwarm"));
    li.appendChild(el("span", "name", a.label));
    li.appendChild(el("span", `zahl st-${a.status}`, `${a.events}`));
    const t = a.tokens || {};
    const tokens = (t.input_tokens || t.output_tokens)
      ? ` · Tokens ${t.input_tokens} ein (${t.cached_input_tokens} aus Cache) / ${t.output_tokens} aus`
      : "";
    li.title = `${a.steps} Schritte · ${a.tool_calls} Tool-Aufrufe · ${a.errors} Fehler${tokens} · ${a.status}`;
    li.onclick = () => {
      if (zustand.agent !== a.id) kontextZuruecksetzen();
      zustand.agent = a.id;
      zeichneAgenten();
      zeichneInhalt();
    };
    ul.appendChild(li);
  }
  if (zustand.agenten.length === 0) ul.appendChild(leer("noch keine Ereignisse"));
}

// ------------------------------------------------------------------ Ansichten

/// Zeichnet den aktiven Reiter VOLLSTÄNDIG neu — beim Wechsel von Reiter oder
/// Agent. Im laufenden Betrieb übernimmt stattdessen `haengeAn`.
/// `erzwingen = false` kommt aus dem Takt: Ansichten, die es koennen, duerfen
/// dann feststellen, dass sich nichts geaendert hat, und stehen bleiben.
async function zeichneInhalt(erzwingen = true) {
  const ziel = document.getElementById("inhalt");
  try {
    if (zustand.reiter === "verlauf") await verlauf(ziel);
    else if (zustand.reiter === "kontext") await kontext(ziel);
    else if (zustand.reiter === "zeitleiste") await zeitleiste(ziel);
    else if (zustand.reiter === "schwarm") await schwarm(ziel);
    else if (zustand.reiter === "graph") await graph(ziel, erzwingen);
    else if (zustand.reiter === "work") await work(ziel);
    else if (zustand.reiter === "benchmarks") await benchmarks(ziel);
  } catch (e) {
    ziel.textContent = "";
    ziel.appendChild(el("p", "fehler", String(e.message || e)));
  }
}

async function verlauf(ziel) {
  if (zustand.agent === null) {
    ziel.textContent = "";
    return void ziel.appendChild(leer("links einen Agenten wählen"));
  }
  const daten = await api(`/api/agents/${encodeURIComponent(zustand.agent)}/history`);
  ziel.textContent = "";
  ziel.appendChild(el("h3", "", `Verlauf · ${zustand.agent || "(Haupt-Agent)"}`));
  const liste = el("div", "strom");
  liste.id = "strom";
  for (const ev of daten.events) liste.appendChild(ereignisKnoten(ev));
  ziel.appendChild(liste);
  if (daten.events.length === 0) ziel.appendChild(leer("keine Ereignisse für diesen Agenten"));
}

// Ein Ereignis als aufklappbare Zeile: die Kopfzeile immer, das Volle auf Klick.
// Der Text der Kopfzeile kommt als `label` vom Server (dieselbe Formulierung wie
// in der Zeitleiste); hier bleibt nur die Einfärbung.
function ereignisKnoten(ev) {
  const [art, inhalt] = nutzlast(ev.data);
  const klasse = { tool_call: "werkzeug", error: "fehler", final: "final" }[art] || "";

  const details = el("details");
  const summary = el("summary");
  summary.appendChild(el("span", "zeit", zeit(ev.at_ms)));
  summary.appendChild(el("span", "typ", ev.etype));
  summary.appendChild(el("span", `text ${klasse}`, ev.label ?? art));
  details.appendChild(summary);
  details.appendChild(el("pre", "", JSON.stringify(inhalt ?? art, null, 2)));
  return details;
}

// -------------------------------------------------------------- Kontext
//
// Nach dem Vorbild der ctxman-UI: oben die Kennzahlen, darunter das Budget
// mit seinen Schwellen, der Token-Verlauf über den ganzen Lauf (mit Zeitreise
// per Klick), die Speicherkarte — jeder Block ein Segment bzw. eine Nachricht,
// Breite nach Tokens, Farbe nach Art —, die Zusammensetzung, die
// GC-Ereignisse und zuletzt die Nachrichten selbst. Ziel ist, auf einen Blick
// zu sehen, WAS im Kontext steht und WAS die Kontextverwaltung damit tut.

const SVG_NS = "http://www.w3.org/2000/svg";

// Farbe je Segment-Art — dieselbe, leicht entsättigte Palette wie die
// ctxman-UI, nach Funktion gruppiert (Statisches indigo, Gespräch blau/grün,
// Tool-Ein-/Ausgabe bernstein, Verdichtung türkis), damit die Karte ruhig
// wirkt statt wie ein Regenbogen.
const ART_FARBE = {
  system_prompt: "#7c74e0", tool_def: "#6366cf", skill_index: "#8b8fe6",
  skill_content: "#4f9d6b", mcp_resource: "#3f8a5c", task: "#b266c4",
  decision: "#c76896", user_msg: "#4f86d6", assistant_msg: "#3aa88a",
  tool_call: "#d39341", tool_result: "#c9793f", subagent_return: "#4aa3bd",
  ref_expansion: "#7fa84c", compaction_summary: "#3a9d96",
};

/// Unbekannte Arten bekommen eine stabile Hash-Farbe im selben Register.
function artFarbe(art) {
  if (ART_FARBE[art]) return ART_FARBE[art];
  let h = 0;
  for (const c of String(art)) h = (h * 31 + c.charCodeAt(0)) | 0;
  return `hsl(${Math.abs(h) % 360} 42% 56%)`;
}

// Die Abschnitte des `ContextReport` tragen deutsche Namen (agentkit/app.rs);
// für die Farbe zählt die Art dahinter.
const ABSCHNITT_ART = {
  "System-Prompt": "system_prompt", "Tool-Schemas": "tool_def",
  "Verlaufs-Zusammenfassung": "compaction_summary", "User-Nachrichten": "user_msg",
  "Assistant-Antworten": "assistant_msg", "Tool-Aufrufe": "tool_call",
  "Tool-Ergebnisse": "tool_result", "Skill-Inhalte": "skill_content",
  "Sub-Agent-Ergebnisse": "task", "MCP-Ressourcen": "mcp_resource",
  "Ref-Expansionen": "ref_expansion",
};

const SCHWELLE_FARBE = { ok: "#22c55e", soft: "#eab308", hard: "#f97316", emergency: "#ef4444" };

// ctxman-Ereignistypen (Spec §6) — Farben wie in der ctxman-UI.
const CTX_EREIGNIS_FARBE = {
  segment_appended: "#3b82f6", segment_externalized: "#f59e0b",
  segment_evicted: "#ef4444", unit_evicted: "#dc2626",
  compaction_started: "#14b8a6", compaction_completed: "#0d9488",
  fact_promoted: "#ec4899", frame_pushed: "#22d3ee", frame_popped: "#06b6d4",
  ref_expanded: "#a3e635", static_epoch_bumped: "#8b5cf6",
  blob_swept: "#64748b", watermark_crossed: "#f97316", render_served: "#10b981",
};

// Was ein ctxman-Ereignis für den Kontext bedeutet — in einem Halbsatz.
const CTX_EREIGNIS_TEXT = {
  segment_appended: "Segment angehängt",
  segment_externalized: "ausgelagert (verlustfrei, per expand_context_ref zurückholbar)",
  segment_evicted: "verdrängt (Inhalt weg)",
  unit_evicted: "Tool-Aufruf samt Ergebnis verdrängt",
  compaction_started: "Verdichtung gestartet",
  compaction_completed: "verdichtet (Zusammenfassung ersetzt Segmente)",
  fact_promoted: "Fakt ins Langzeitgedächtnis übernommen",
  ref_expanded: "ausgelagerten Inhalt zurückgeholt (Page Fault)",
  watermark_crossed: "Schwelle überschritten",
  render_served: "Kontext für einen Modell-Call gerendert",
  static_epoch_bumped: "statischer Teil geändert (Cache-Präfix neu)",
};

/// Tokens kurz: 950, 12,4k.
const tok = (n) => {
  const z = Number(n) || 0;
  return z >= 10000 ? `${(z / 1000).toFixed(1).replace(".", ",")}k` : String(z);
};
const prozent = (teil, ganz) =>
  ganz > 0 ? `${((100 * teil) / ganz).toFixed(1).replace(".", ",")} %` : "–";

/// Grobe Token-Schätzung (Zeichen/4) — dieselbe Heuristik, die agentkit ohne
/// ctxman benutzt. Es geht um das Verhältnis der Teile zueinander.
function schaetzeTokens(roh) {
  return Math.max(1, Math.round(String(roh).length / 4));
}

/// Ein SVG mit `viewBox`, das sich der Breite anpasst, plus ein Helfer, der
/// Kindknoten anlegt (`ziel` = anderes Elternelement, etwa eine Gruppe).
function svgFlaeche(B, H, klasse) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${B} ${H}`);
  svg.classList.add(klasse);
  const knoten = (tag, attrs, text, ziel = svg) => {
    const n = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
    if (text !== undefined) n.textContent = text;
    ziel.appendChild(n);
    return n;
  };
  return [svg, knoten];
}

/// Ein Panel mit Überschrift und einem Satz Erklärung darunter — die
/// Erklärung ist Absicht: die Ansicht soll zeigen, WIE Kontextverwaltung
/// funktioniert, nicht nur Zahlen nebeneinanderstellen.
function panel(titel, erklaerung, inhalt) {
  const p = el("div", "panel");
  p.appendChild(el("h3", "", titel));
  if (erklaerung) p.appendChild(el("p", "erklaerung", erklaerung));
  if (inhalt) p.appendChild(inhalt);
  return p;
}

async function kontext(ziel) {
  if (zustand.agent === null) {
    ziel.textContent = "";
    return void ziel.appendChild(leer("links einen Agenten wählen"));
  }
  const params = zustand.kontextBis !== null ? { bis: zustand.kontextBis } : {};
  const k = await api(`/api/agents/${encodeURIComponent(zustand.agent)}/context`, params);
  ziel.textContent = "";
  ziel.appendChild(el("h3", "", `Kontext · ${zustand.agent || "(Haupt-Agent)"}`));

  if (zustand.kontextBis !== null) {
    const hinweis = el("div", "zeitreise");
    hinweis.appendChild(el("span", "",
      `Zeitreise: Stand ${k.step ? `vor Schritt ${k.step}` : "am Ende eines Laufs"} ` +
      `(Datensatz #${k.seq}) — neue Ereignisse werden nicht nachgezeichnet.`));
    const zurueck = el("button", "link", "zurück zum aktuellen Stand");
    zurueck.onclick = () => { zustand.kontextBis = null; zeichneInhalt(); };
    hinweis.appendChild(zurueck);
    ziel.appendChild(hinweis);
  }
  if (k.rekonstruiert) {
    ziel.appendChild(el("p", "warnung",
      "Rekonstruiert aus dem Ereignisstrom: dieser Agent hat keinen Kontext-Datensatz " +
      "hinterlassen — System-Prompt und Verdichtungen fehlen deshalb hier. Traces ab " +
      "agentkit 0.14 tragen ihn für jeden Agenten, auch für Sub-Agenten und Schwarm-Mitglieder."));
  }
  if (k.unvollstaendig) {
    ziel.appendChild(el("p", "warnung",
      "Lückenhaft: ein Kontext-Datensatz setzte hinter dem an, was im Trace steht — " +
      "die Nachrichten stimmen, ihre Positionen nicht."));
  }

  const m = kontextModell(k);
  ziel.appendChild(kennzahlen(k, m));

  ziel.appendChild(panel("Budget & Schwellen",
    m.verwaltet
      ? "ctxman räumt beim Rendern nach Füllstand auf: ab soft lagert der Minor GC große " +
        "Tool-Ergebnisse verlustfrei aus, ab hard verdichtet der Major GC alte Züge zu einer " +
        "Zusammenfassung, ab emergency wird synchron verdrängt."
      : "Ohne ctxman verdichtet agentkit selbst: überschreitet der Verlauf (geschätzt oder " +
        "vom Provider gemessen) das Budget, fasst das Modell die älteren Züge zusammen — " +
        "der Auftrag und die letzten Nachrichten bleiben wörtlich.",
    budgetAnzeige(m)));

  ziel.appendChild(panel("Token-Verlauf",
    "Ein Punkt je Kontext-Datensatz (vor jedem Modell-Call und am Ende eines Laufs); " +
    "der Sägezahn zeigt, wann Verdichtung oder GC den Kontext wieder schrumpfen lassen. " +
    "Klick auf einen Punkt zeigt den Kontext zu diesem Zeitpunkt.",
    tokenZeitleiste(k, m)));

  ziel.appendChild(panel("Speicherkarte",
    "Jeder Block ist " + (m.verwaltet ? "ein ctxman-Segment" : "eine Nachricht") +
    " in Render-Reihenfolge; Breite ∝ Tokens (logarithmisch), Farbe = Art. Der statische " +
    "Teil geht bei jedem Call unverändert mit, der Arbeitsbereich ist das, was " +
    "Verdichtung und GC kürzen. Überfahren zeigt den Inhalt, Klick hält ihn fest.",
    speicherkarte(k, m)));

  const unten = el("div", "zweispaltig");
  if (k.report) unten.appendChild(panel("Zusammensetzung", "Tokens je Art des Inhalts.", zusammensetzung(k.report)));
  unten.appendChild(gcAktivitaet(k, m));
  ziel.appendChild(unten);

  ziel.appendChild(nachrichtenPanel(k, m));
}

/// Was alle Teile der Ansicht brauchen, einmal ausgerechnet.
function kontextModell(k) {
  const report = k.report || {};
  const verwaltet = !!(k.ctx && Array.isArray(k.ctx.segments));
  const bloecke = verwaltet ? segmentBloecke(k) : nachrichtenBloecke(k);
  // Tool-Schemas gehen bei jedem Call mit, stehen aber in keiner Nachricht
  // und in keinem Segment — ohne diesen Block fehlte oft der größte Posten.
  const schemas = (report.segments || []).find((s) => s.label === "Tool-Schemas");
  const nachSystem = bloecke.findIndex((b) => !b.statisch);
  const einfuegen = (neu) => bloecke.splice(nachSystem < 0 ? bloecke.length : nachSystem, 0, ...neu);
  if (Array.isArray(k.tools) && k.tools.length) {
    // Je Tool ein Block mit seinem vollen Schema — so sieht man, WELCHES
    // Werkzeug das Budget frisst (oft sind es lange MCP-Beschreibungen).
    einfuegen(k.tools.map((t, i) => {
      const f = t.function || t;
      const json = JSON.stringify(t, null, 2);
      return {
        id: `tool-${i}`, art: "tool_def", zustand: "live", statisch: true,
        tokens: schaetzeTokens(JSON.stringify(t)), titel: f.name || `tool_${i}`,
        meta: ["Tool-Schema", "bei jedem Call mitgeschickt"],
        text: json,
      };
    }));
  } else if (schemas) {
    einfuegen([{
      id: "tool-schemas", art: "tool_def", zustand: "live", statisch: true,
      tokens: schemas.tokens, titel: `Tool-Schemas (${schemas.count})`,
      meta: [`${schemas.count} Tools`, "bei jedem Call mitgeschickt"],
      text: "Die JSON-Schemas aller angebotenen Tools. Sie stehen in keiner Nachricht, " +
        "zählen aber bei jedem Modell-Call — und keine Verdichtung macht sie kleiner. " +
        "Viele (MCP-)Tools sind deshalb ein fester Sockel im Budget. (Dieser Trace " +
        "trägt die einzelnen Schemas noch nicht — das tun erst neuere agentkit-Versionen.)",
    }]);
  }
  const lebend = (b) => b.zustand === "live" || b.zustand === "externalized";
  const summe = bloecke.filter(lebend).reduce((s, b) => s + b.tokens, 0);
  return {
    verwaltet,
    bloecke,
    budget: (k.ctx && k.ctx.budget) || report.budget || 0,
    belegt: report.total ?? summe,
    marken: verwaltet ? k.ctx.watermarks : null,
    lebend,
  };
}

/// Mit ctxman: die Segmente, wie die Session sie führt — inklusive
/// ausgelagerter, verdichteter und verdrängter.
function segmentBloecke(k) {
  // Ein Segment trägt nur einen Anriss; das volle Tool-Ergebnis steht im
  // gespiegelten Verlauf und findet sich über die tool_call_id.
  const voll = new Map();
  for (const n of k.messages) {
    if (n.role === "tool" && n.tool_call_id) voll.set(n.tool_call_id, n.content);
  }
  return [...k.ctx.segments].sort((a, b) => a.seq - b.seq).map((s) => {
    const ganz = s.state === "live" && s.tool_call_id && voll.get(s.tool_call_id);
    const hinweis = {
      externalized: "Ausgelagert: im Kontext steht nur noch diese Zusammenfassung plus ein Verweis; der volle Inhalt liegt im Blob Store und ist per expand_context_ref zurückholbar.",
      compacted: "Verdichtet: das Segment ist in einer Zusammenfassung aufgegangen und wird nicht mehr gerendert.",
      evicted: "Verdrängt: das Segment ist aus dem Kontext entfernt.",
    }[s.state];
    return {
      id: s.id, art: s.kind, zustand: s.state, statisch: s.region === "static",
      tokens: s.tokens, gepinnt: s.pinned,
      titel: s.kind,
      meta: [`seq ${s.seq}`, `Zug ${s.created_turn}`, s.role && `Rolle ${s.role}`,
        s.tool_call_id && `Unit ${s.tool_call_id}`, s.pinned && "gepinnt"].filter(Boolean),
      text: (ganz || s.preview || "") +
        (!ganz && (s.preview || "").length >= 200 ? " …" : ""),
      hinweis,
    };
  });
}

/// Ohne ctxman: die Nachrichten der `ShortTermMemory`, Tool-Aufrufe einzeln.
function nachrichtenBloecke(k) {
  const bloecke = [];
  let systemGesehen = false;
  k.messages.forEach((n, i) => {
    const text = typeof n.content === "string" ? n.content : JSON.stringify(n.content ?? "");
    const basis = { zustand: "live", statisch: false, meta: [`Nachricht ${i}`, n.role] };
    if (n.role === "system") {
      // Die erste System-Nachricht ist der Prompt, jede weitere eine
      // Verdichtung („Bisheriger Verlauf (komprimiert)").
      const prompt = !systemGesehen;
      systemGesehen = true;
      bloecke.push({ ...basis, id: `m${i}`, art: prompt ? "system_prompt" : "compaction_summary",
        statisch: prompt, tokens: schaetzeTokens(text), text,
        titel: prompt ? "System-Prompt" : "Verlaufs-Zusammenfassung" });
    } else if (n.role === "user") {
      bloecke.push({ ...basis, id: `m${i}`, art: "user_msg", tokens: schaetzeTokens(text), text, titel: "User" });
    } else if (n.role === "assistant") {
      if (text) {
        bloecke.push({ ...basis, id: `m${i}`, art: "assistant_msg", tokens: schaetzeTokens(text), text, titel: "Assistant" });
      }
      (n.tool_calls || []).forEach((c, j) => {
        const name = c.function?.name ?? "?";
        const args = c.function?.arguments ?? "";
        bloecke.push({ ...basis, id: `m${i}c${j}`, art: "tool_call",
          tokens: schaetzeTokens(JSON.stringify(c)), text: `${name}(${args})`,
          titel: `Aufruf ${name}`, meta: [...basis.meta, c.id && `Unit ${c.id}`].filter(Boolean) });
      });
    } else if (n.role === "tool") {
      bloecke.push({ ...basis, id: `m${i}`, art: "tool_result", tokens: schaetzeTokens(text), text,
        titel: `Ergebnis ${n.name || ""}`.trim(),
        meta: [...basis.meta, n.tool_call_id && `Unit ${n.tool_call_id}`].filter(Boolean) });
    } else {
      bloecke.push({ ...basis, id: `m${i}`, art: n.role || "?", tokens: schaetzeTokens(text), text, titel: n.role || "?" });
    }
  });
  return bloecke;
}

/// Die Kacheln oben: Belegung, Budget, Umfang, Stand.
function kennzahlen(k, m) {
  const reihe = el("div", "kennzahlen");
  const kachel = (titel, wert, unter, klasse = "") => {
    const kd = el("div", `kachel ${klasse}`);
    kd.appendChild(el("div", "kachel-titel", titel));
    kd.appendChild(el("div", "kachel-wert", wert));
    kd.appendChild(el("div", "kachel-unter", unter));
    reihe.appendChild(kd);
  };
  const stufe = schwelleVon(m);
  kachel("Belegt", tok(m.belegt), `${prozent(m.belegt, m.budget)} von ${tok(m.budget)}`, `stufe-${stufe}`);
  kachel("Schwelle", stufe, m.verwaltet ? "ctxman-Watermark" : "agentkit-Budget", `stufe-${stufe}`);
  // Bei einer Zeitreise zählt der Call, der MIT diesem Kontext gemacht wurde —
  // der Datensatz steht unmittelbar vor ihm.
  const zeitreise = zustand.kontextBis !== null;
  const letzte = zeitreise
    ? (k.usage.find((u) => u.seq > k.seq) ?? null)
    : (k.usage.length ? k.usage[k.usage.length - 1] : null);
  kachel("Gemessen", letzte ? tok(letzte.input_tokens) : "–",
    letzte ? `${zeitreise ? "dieser Call" : "letzter Call"} · ${prozent(letzte.cached_input_tokens, letzte.input_tokens)} aus Cache`
      : "kein token_usage im Trace");
  kachel("Nachrichten", String(k.messages.length), `${m.bloecke.length} Blöcke`);
  if (m.verwaltet) {
    const zaehle = (z) => m.bloecke.filter((b) => b.zustand === z).length;
    kachel("Segmente", String(zaehle("live")),
      `${zaehle("externalized")} ausgelagert · ${zaehle("compacted")} verdichtet · ${zaehle("evicted")} verdrängt`);
  } else {
    const verdichtet = k.verlauf.filter((p) => p.neu_geschrieben).length;
    kachel("Verdichtungen", String(verdichtet), "Verlauf neu geschrieben");
  }
  kachel("Stand", k.step ? `vor Schritt ${k.step}` : (k.seq ? "Lauf-Ende" : "–"),
    `${k.verlauf.length} Datensätze · ${m.verwaltet ? "ctxman" : "ShortTermMemory"}`);
  return reihe;
}

/// ok | soft | hard | emergency — ohne ctxman gibt es nur ok und „über Budget".
function schwelleVon(m) {
  const f = m.budget > 0 ? m.belegt / m.budget : 0;
  if (!m.marken) return f > 1 ? "emergency" : f > 0.8 ? "soft" : "ok";
  if (f >= m.marken.emergency) return "emergency";
  if (f >= m.marken.hard) return "hard";
  if (f >= m.marken.soft) return "soft";
  return "ok";
}

/// Der Budget-Balken mit den Schwellen-Zonen und einer Nadel für den Stand.
function budgetAnzeige(m) {
  const B = 1000, H = 74, y = 18, h = 18;
  const [svg, n] = svgFlaeche(B, H, "budget");
  const w = m.marken;
  const zonen = w
    ? [[0, w.soft, "ok"], [w.soft, w.hard, "soft"], [w.hard, w.emergency, "hard"], [w.emergency, 1, "emergency"]]
    : [[0, 0.8, "ok"], [0.8, 1, "soft"]];
  const frac = m.budget > 0 ? Math.min(m.belegt / m.budget, 1.06) : 0;
  const x = (f) => Math.min(f, 1.06) * B / 1.06;
  for (const [von, bis, stufe] of zonen) {
    n("rect", { x: x(von), y, width: x(bis) - x(von), height: h, fill: SCHWELLE_FARBE[stufe], opacity: 0.15 });
    const voll = Math.min(frac, bis);
    if (voll > von) {
      n("rect", { x: x(von), y, width: x(voll) - x(von), height: h, fill: SCHWELLE_FARBE[stufe], opacity: 0.85 });
    }
  }
  if (frac > 1) n("rect", { x: x(1), y, width: x(frac) - x(1), height: h, fill: SCHWELLE_FARBE.emergency });
  const linien = w
    ? [["soft", w.soft], ["hard", w.hard], ["emergency", w.emergency], ["Budget", 1]]
    : [["Verdichtung", 1]];
  // Beschriftungen abwechselnd über und unter den Balken: emergency (95 %)
  // und Budget (100 %) liegen sonst übereinander.
  linien.forEach(([name, f], i) => {
    const unten = i % 2 === 1;
    n("line", { x1: x(f), y1: y - 5, x2: x(f), y2: y + h + 5, class: "marke-linie" });
    n("text", { x: x(f), y: unten ? y + h + 15 : y - 8, "text-anchor": f >= 0.99 ? "end" : "middle", class: "achse" },
      `${name} ${Math.round(f * 100)}%`);
  });
  n("line", { x1: x(frac), y1: y - 3, x2: x(frac), y2: y + h + 3, class: "nadel" });
  n("text", { x: 4, y: y + h + 30, "text-anchor": "start", class: "nadel-text" },
    `Stand: ${prozent(m.belegt, m.budget)} · ${tok(m.belegt)} von ${tok(m.budget)} Tokens`);
  return svg;
}

/// Tokens über den Lauf: der Report je Datensatz (geschätzt bzw. von ctxman
/// gezählt) als Fläche, die vom Provider gemessene Prompt-Größe je Call als
/// zweite Linie, dazu Budget, Schwellen und die Spuren von Verdichtung und GC.
function tokenZeitleiste(k, m) {
  const punkte = k.verlauf;
  if (!punkte.length && !k.usage.length) {
    return leer("noch keine Kontext-Datensätze — die Zeitleiste füllt sich mit dem ersten Schritt");
  }
  const B = 1000, H = 230, links = 56, rechts = 14, oben = 14, unten = 26;
  const [svg, n] = svgFlaeche(B, H, "zeitleiste-kontext");
  const seqs = [...punkte.map((p) => p.seq), ...k.usage.map((u) => u.seq)];
  const seqMin = Math.min(...seqs), seqMax = Math.max(...seqs);
  const budget = m.budget || Math.max(...punkte.map((p) => p.budget), 1);
  const yMax = Math.max(budget, ...punkte.map((p) => p.total), ...k.usage.map((u) => u.input_tokens)) * 1.06 || 1;
  const x = (seq) => seqMax === seqMin ? (links + B - rechts) / 2
    : links + ((seq - seqMin) / (seqMax - seqMin)) * (B - links - rechts);
  const y = (t) => oben + (1 - t / yMax) * (H - oben - unten);

  // Schwellen und Budget.
  const linien = m.marken
    ? [["soft", m.marken.soft, SCHWELLE_FARBE.soft], ["hard", m.marken.hard, SCHWELLE_FARBE.hard],
       ["emergency", m.marken.emergency, SCHWELLE_FARBE.emergency], [tok(budget), 1, "#cbd5e1"]]
    : [[tok(budget), 1, "#cbd5e1"]];
  // Das Budget links an der Achse, die Schwellen rechts über ihrer Linie —
  // emergency (95 %) und Budget lägen links sonst übereinander.
  for (const [name, f, farbe] of linien) {
    n("line", { x1: links, y1: y(budget * f), x2: B - rechts, y2: y(budget * f), stroke: farbe, "stroke-dasharray": "4 4", opacity: 0.55 });
    const budgetLinie = f === 1;
    n("text", budgetLinie
      ? { x: links - 5, y: y(budget) + 3.5, fill: farbe, "text-anchor": "end", class: "achse" }
      : { x: B - rechts - 2, y: y(budget * f) - 3, fill: farbe, "text-anchor": "end", class: "achse" }, name);
  }
  n("text", { x: links - 5, y: y(0) + 3.5, "text-anchor": "end", class: "achse" }, "0");

  // Die Spuren der Kontextverwaltung: Verdichtungen (Verlauf neu geschrieben)
  // und die GC-Ereignisse von ctxman, je Datensatz zusammengefasst.
  for (const p of punkte.filter((q) => q.neu_geschrieben)) {
    n("line", { x1: x(p.seq), y1: oben, x2: x(p.seq), y2: H - unten, stroke: "#8b5cf6", "stroke-dasharray": "2 3" });
    n("text", { x: x(p.seq) + 3, y: oben + 9, fill: "#8b5cf6", class: "achse" }, "verdichtet");
  }
  const gc = new Map();
  for (const e of k.ctx_events) {
    if (!["segment_externalized", "compaction_completed", "segment_evicted", "unit_evicted", "watermark_crossed"].includes(e.event_type)) continue;
    const liste = gc.get(e.seq) || [];
    liste.push(e);
    gc.set(e.seq, liste);
  }
  for (const [seq, liste] of gc) {
    const farbe = CTX_EREIGNIS_FARBE[liste[0].event_type] || "#94a3b8";
    const g = n("g", {});
    n("path", { d: `M${x(seq) - 5},${H - unten + 2} L${x(seq) + 5},${H - unten + 2} L${x(seq)},${H - unten - 6} Z`, fill: farbe }, undefined, g);
    const zaehlung = {};
    for (const e of liste) zaehlung[e.event_type] = (zaehlung[e.event_type] || 0) + 1;
    n("title", {}, Object.entries(zaehlung).map(([t, z]) => `${z}× ${CTX_EREIGNIS_TEXT[t] || t}`).join("\n"), g);
  }

  // Der gewählte Stand.
  if (k.seq) {
    n("line", { x1: x(k.seq), y1: oben, x2: x(k.seq), y2: H - unten, class: "auswahl-linie" });
  }

  // Gemessen: eine dünne Linie mit Punkten.
  if (k.usage.length) {
    const d = k.usage.map((u, i) => `${i ? "L" : "M"}${x(u.seq).toFixed(1)},${y(u.input_tokens).toFixed(1)}`).join(" ");
    n("path", { d, fill: "none", stroke: "#22d3ee", "stroke-width": 1.4, "stroke-dasharray": "3 3" });
    for (const u of k.usage) {
      const c = n("circle", { cx: x(u.seq), cy: y(u.input_tokens), r: 2.6, fill: "#22d3ee" });
      n("title", {}, `gemessen: ${u.input_tokens} Tokens ein (${u.cached_input_tokens} aus Cache) · ${u.output_tokens} aus`, c);
    }
  }

  // Report je Datensatz: Fläche + Linie + anklickbare Punkte.
  if (punkte.length) {
    const d = punkte.map((p, i) => `${i ? "L" : "M"}${x(p.seq).toFixed(1)},${y(p.total).toFixed(1)}`).join(" ");
    const defs = n("defs", {});
    const verlauf = n("linearGradient", { id: "kontext-fuellung", x1: 0, y1: 0, x2: 0, y2: 1 }, undefined, defs);
    n("stop", { offset: "0%", "stop-color": "#6366f1", "stop-opacity": 0.45 }, undefined, verlauf);
    n("stop", { offset: "100%", "stop-color": "#6366f1", "stop-opacity": 0.02 }, undefined, verlauf);
    if (punkte.length > 1) {
      n("path", { d: `${d} L${x(punkte[punkte.length - 1].seq).toFixed(1)},${y(0)} L${x(punkte[0].seq).toFixed(1)},${y(0)} Z`, fill: "url(#kontext-fuellung)" });
    }
    n("path", { d, fill: "none", stroke: "#818cf8", "stroke-width": 2.2, "stroke-linejoin": "round" });
    punkte.forEach((p, i) => {
      const gewaehlt = p.seq === k.seq;
      const c = n("circle", { cx: x(p.seq), cy: y(p.total), r: gewaehlt ? 6 : 4, class: `punkt${gewaehlt ? " gewaehlt" : ""}` });
      n("title", {}, `${p.step ? `vor Schritt ${p.step}` : "Ende eines Laufs"} · ${p.total} Tokens · ` +
        `${p.messages_total} Nachrichten${p.neu_geschrieben ? " · nach Verdichtung" : ""}\nKlick: Kontext zu diesem Zeitpunkt`, c);
      c.onclick = () => {
        // Der jüngste Punkt IST der aktuelle Stand — dorthin heißt: live.
        zustand.kontextBis = i === punkte.length - 1 ? null : p.seq;
        zustand.kontextAuswahl = null;
        zeichneInhalt();
      };
    });
  }

  const rahmen = el("div");
  rahmen.appendChild(svg);
  const legende = el("div", "legende");
  const eintrag = (farbe, text, klasse = "") => {
    const e = el("span", "eintrag");
    const s = el("span", `swatch ${klasse}`);
    s.style.background = farbe;
    e.appendChild(s);
    e.appendChild(document.createTextNode(text));
    legende.appendChild(e);
  };
  eintrag("#818cf8", "Kontext laut Report");
  if (k.usage.length) eintrag("#22d3ee", "gemessen (Provider, je Call)");
  if (punkte.some((p) => p.neu_geschrieben)) eintrag("#8b5cf6", "Verdichtung");
  if (gc.size) eintrag("#f59e0b", "GC-Ereignis (Dreieck, Tooltip)");
  rahmen.appendChild(legende);
  return rahmen;
}

/// Die Speicherkarte: statischer Teil und Arbeitsbereich als Blöcke, darunter
/// die Vorschau (beim Überfahren) und der Inspektor (beim Klick).
function speicherkarte(k, m) {
  const rahmen = el("div");
  const steuer = el("div", "steuerung");
  const tote = m.bloecke.filter((b) => !m.lebend(b)).length;
  if (tote) {
    const lab = el("label", "dim");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = zustand.kontextTote;
    box.onchange = () => { zustand.kontextTote = box.checked; zeichneKarte(); };
    lab.appendChild(box);
    lab.appendChild(document.createTextNode(` verdichtete/verdrängte zeigen (${tote})`));
    steuer.appendChild(lab);
  }
  rahmen.appendChild(steuer);
  const karte = el("div");
  rahmen.appendChild(karte);
  const vorschau = el("div", "vorschau");
  rahmen.appendChild(vorschau);
  const inspektor = el("div", "inspektor");
  rahmen.appendChild(inspektor);

  // Logarithmisch: ein 9k-Ergebnis sprengt die Karte nicht, kleine Blöcke
  // bleiben anklickbar.
  const maxTok = Math.max(1, ...m.bloecke.map((b) => b.tokens));
  const breite = (t) => Math.round(26 + (Math.log(1 + t) / Math.log(1 + maxTok)) * (260 - 26));

  const zeigeVorschau = (b) => {
    vorschau.textContent = "";
    if (!b) {
      vorschau.appendChild(el("span", "dim", "Über einen Block fahren, um den Inhalt zu sehen — Klick hält ihn im Inspektor fest."));
      return;
    }
    const kopf = el("div", "vorschau-kopf");
    const s = el("span", "swatch");
    s.style.background = artFarbe(b.art);
    kopf.appendChild(s);
    kopf.appendChild(el("b", "", b.titel));
    kopf.appendChild(el("span", "dim", `${b.tokens} Tokens · ${b.zustand} · ${b.meta.join(" · ")}`));
    vorschau.appendChild(kopf);
    vorschau.appendChild(el("div", "vorschau-text", kurz(b.text, 600) || "(leer)"));
  };
  const zeigeInspektor = () => {
    inspektor.textContent = "";
    const b = m.bloecke.find((x) => x.id === zustand.kontextAuswahl);
    if (!b) return;
    const kopf = el("div", "vorschau-kopf");
    const s = el("span", "swatch");
    s.style.background = artFarbe(b.art);
    kopf.appendChild(s);
    kopf.appendChild(el("b", "", b.titel));
    kopf.appendChild(el("span", `zustand z-${b.zustand}`, b.zustand));
    kopf.appendChild(el("span", "dim", `${b.tokens} Tokens · ${b.statisch ? "statisch" : "Arbeitsbereich"}`));
    const zu = el("button", "link", "schließen");
    zu.onclick = () => { zustand.kontextAuswahl = null; zeichneKarte(); };
    kopf.appendChild(zu);
    inspektor.appendChild(kopf);
    inspektor.appendChild(el("div", "dim", b.meta.join(" · ") + (b.id.length > 20 ? ` · id ${b.id}` : "")));
    if (b.hinweis) inspektor.appendChild(el("p", "warnung", b.hinweis));
    inspektor.appendChild(el("pre", "", b.text || "(leer)"));
  };

  const block = (b) => {
    const tot = !m.lebend(b);
    const klassen = ["block", b.zustand === "externalized" ? "ausgelagert" : "", tot ? "tot" : "",
      b.gepinnt ? "gepinnt" : "", zustand.kontextAuswahl === b.id ? "gewaehlt" : ""].filter(Boolean);
    const d = el("div", klassen.join(" "));
    d.style.width = `${breite(b.tokens)}px`;
    d.style.background = artFarbe(b.art);
    d.appendChild(el("span", "block-text", b.titel.replace(/_/g, " ")));
    d.onmouseenter = () => zeigeVorschau(b);
    d.onmouseleave = () => zeigeVorschau(null);
    d.onclick = () => {
      zustand.kontextAuswahl = zustand.kontextAuswahl === b.id ? null : b.id;
      zeichneKarte();
    };
    return d;
  };

  const region = (titel, bloecke) => {
    const sichtbar = zustand.kontextTote ? bloecke : bloecke.filter(m.lebend);
    const summe = bloecke.filter(m.lebend).reduce((s, b) => s + b.tokens, 0);
    const kopf = el("div", "region-kopf");
    kopf.appendChild(el("span", "", `${titel} · ${bloecke.filter(m.lebend).length} Blöcke · ${tok(summe)} Tokens`));
    kopf.appendChild(el("span", "region-linie"));
    karte.appendChild(kopf);
    const zeile = el("div", "karte");
    for (const b of sichtbar) zeile.appendChild(block(b));
    if (!sichtbar.length) zeile.appendChild(el("span", "dim", "(leer)"));
    karte.appendChild(zeile);
  };

  function zeichneKarte() {
    karte.textContent = "";
    region("Statisch", m.bloecke.filter((b) => b.statisch));
    region("Arbeitsbereich", m.bloecke.filter((b) => !b.statisch));
    zeigeVorschau(null);
    zeigeInspektor();
  }
  zeichneKarte();

  // Legende: nur die Arten, die vorkommen, plus die Zustände.
  const legende = el("div", "legende");
  const arten = [...new Set(m.bloecke.map((b) => b.art))];
  for (const art of arten) {
    const e = el("span", "eintrag");
    const s = el("span", "swatch");
    s.style.background = artFarbe(art);
    e.appendChild(s);
    e.appendChild(document.createTextNode(art));
    legende.appendChild(e);
  }
  if (m.verwaltet) {
    for (const [klasse, text] of [["ausgelagert", "ausgelagert (Verweis + Zusammenfassung)"], ["tot", "verdichtet / verdrängt"], ["gepinnt", "gepinnt"]]) {
      const e = el("span", "eintrag");
      const s = el("span", `swatch block ${klasse}`);
      s.style.background = "#64748b";
      e.appendChild(s);
      e.appendChild(document.createTextNode(text));
      legende.appendChild(e);
    }
  }
  rahmen.appendChild(legende);
  return rahmen;
}

/// Der `ContextReport` als gestapelter Balken plus Tabelle.
function zusammensetzung(report) {
  const rahmen = el("div");
  const summe = Math.max(1, report.total);
  const balken = el("div", "balken dick");
  for (const s of report.segments) {
    const teil = el("div");
    teil.style.width = `${(s.tokens / summe) * 100}%`;
    teil.style.background = artFarbe(ABSCHNITT_ART[s.label] || s.label);
    teil.title = `${s.label}: ${s.tokens} Tokens`;
    balken.appendChild(teil);
  }
  rahmen.appendChild(balken);
  const tab = el("table");
  tab.appendChild(kopfzeile(["", "Abschnitt", "Tokens", "Anteil", "Einträge", "Hinweis"]));
  for (const s of report.segments) {
    const tr = datenzeile(["", s.label, tok(s.tokens), prozent(s.tokens, report.total), String(s.count), s.note ?? ""]);
    const sw = el("span", "swatch");
    sw.style.background = artFarbe(ABSCHNITT_ART[s.label] || s.label);
    tr.firstChild.appendChild(sw);
    tab.appendChild(tr);
  }
  rahmen.appendChild(tab);
  return rahmen;
}

/// Was die Kontextverwaltung getan hat: Zähler je Ereignistyp und die
/// Ereignisse selbst (neueste zuerst). Ohne ctxman die Verdichtungen von
/// agentkit, soweit der Trace sie zeigt.
function gcAktivitaet(k, m) {
  if (!m.verwaltet && !k.ctx_events.length) {
    const verdichtet = k.verlauf.filter((p) => p.neu_geschrieben);
    const inhalt = el("div");
    if (!verdichtet.length) {
      inhalt.appendChild(el("p", "dim", "Bisher keine Verdichtung — der Verlauf passt ins Budget."));
    } else {
      for (const p of verdichtet) {
        inhalt.appendChild(el("div", "ereignis", `${p.step ? `Schritt ${p.step}` : "Lauf-Ende"} · Verlauf verdichtet → ${p.messages_total} Nachrichten, ${tok(p.total)} Tokens`));
      }
    }
    return panel("Verdichtung", "Ohne ctxman gibt es keinen GC, nur die Verdichtung durch agentkit.", inhalt);
  }
  const inhalt = el("div");
  const zaehlung = {};
  for (const e of k.ctx_events) zaehlung[e.event_type] = (zaehlung[e.event_type] || 0) + 1;
  const chips = el("div", "chips");
  for (const [typ, z] of Object.entries(zaehlung).sort((a, b) => b[1] - a[1])) {
    const c = el("span", "chip");
    c.style.borderColor = CTX_EREIGNIS_FARBE[typ] || "#94a3b8";
    c.title = CTX_EREIGNIS_TEXT[typ] || typ;
    c.appendChild(document.createTextNode(`${typ} `));
    c.appendChild(el("b", "", String(z)));
    chips.appendChild(c);
  }
  inhalt.appendChild(chips);
  // `render_served` und `segment_appended` kommen in jedem Schritt; im
  // Strom stehen sie nur auf Wunsch, sonst verdecken sie, was der GC tut.
  const routine = new Set(["render_served", "segment_appended"]);
  const lab = el("label", "dim");
  const box = document.createElement("input");
  box.type = "checkbox";
  box.checked = zustand.kontextRoutine;
  lab.appendChild(box);
  lab.appendChild(document.createTextNode(" auch Routine (angehängt, gerendert)"));
  inhalt.appendChild(lab);
  const strom = el("div", "ereignisse");
  inhalt.appendChild(strom);
  const zeichne = () => {
    strom.textContent = "";
    const liste = k.ctx_events.filter((e) => zustand.kontextRoutine || !routine.has(e.event_type));
    for (const e of liste.slice(-200).reverse()) {
      const z = el("div", "ereignis");
      if (k.seq && e.seq > k.seq) z.classList.add("zukunft");
      const typ = el("span", "typ", e.event_type);
      typ.style.color = CTX_EREIGNIS_FARBE[e.event_type] || "#94a3b8";
      z.appendChild(el("span", "dim", e.step ? `S${e.step}` : "–"));
      z.appendChild(typ);
      z.appendChild(el("span", "text", kurz(JSON.stringify(e.payload ?? {}), 140)));
      z.title = CTX_EREIGNIS_TEXT[e.event_type] || e.event_type;
      strom.appendChild(z);
    }
    if (!liste.length) strom.appendChild(el("p", "dim", "bisher nur Routine — der GC musste noch nicht eingreifen"));
  };
  box.onchange = () => { zustand.kontextRoutine = box.checked; zeichne(); };
  zeichne();
  return panel("GC-Aktivität", "Die Ereignisse der ctxman-Session (Spec §6), je Schritt abgeholt.", inhalt);
}

/// Die Nachrichten selbst, durchsuchbar — bei ctxman der gespiegelte Verlauf,
/// nicht das gerenderte Ergebnis (das zeigt die Speicherkarte).
function nachrichtenPanel(k, m) {
  const inhalt = el("div");
  if (k.messages.length === 0) {
    inhalt.appendChild(leer("kein Kontext aufgezeichnet"));
  } else {
    // Suchfeld: ein Kontext mit hundert Nachrichten ist ohne Filter nicht zu
    // lesen, und genau dann will man wissen, was da eigentlich drinsteht.
    const liste = el("div");
    const suche = document.createElement("input");
    suche.type = "search";
    suche.placeholder = "in den Nachrichten suchen …";
    suche.className = "suche";
    suche.value = zustand.kontextSuche;
    suche.oninput = () => { zustand.kontextSuche = suche.value; zeichneNachrichten(liste, k.messages, suche.value); };
    inhalt.appendChild(suche);
    inhalt.appendChild(liste);
    zeichneNachrichten(liste, k.messages, zustand.kontextSuche);
  }
  return panel(`Nachrichten (${k.messages.length})`,
    m.verwaltet
      ? "Der vollständige Verlauf, den agentkit neben ctxman mitführt. Was davon das Modell tatsächlich sieht, zeigt die Speicherkarte (ausgelagerte Segmente nur als Zusammenfassung)."
      : "Genau diese Nachrichten gehen mit dem nächsten Modell-Call raus (plus die Tool-Schemas).",
    inhalt);
}

/// Eine Zeile je Nachricht: Nummer, Rolle, geschätzte Tokens und der Anfang.
/// Aufgeklappt steht die Nachricht roh da — das ist der Sinn der Ansicht.
function zeichneNachrichten(ziel, messages, filter) {
  ziel.textContent = "";
  const f = filter.trim().toLowerCase();
  let gezeigt = 0;
  messages.forEach((m, i) => {
    const roh = JSON.stringify(m, null, 2);
    if (f && !roh.toLowerCase().includes(f)) return;
    gezeigt++;
    const details = el("details", `rolle-${m.role ?? "x"}`);
    const summary = el("summary");
    summary.appendChild(el("span", "typ", `${i}. ${m.role ?? "?"}`));
    summary.appendChild(el("span", "zahl", `~${schaetzeTokens(roh)}`));
    summary.appendChild(el("span", "text", kurz(vorschau(m))));
    details.appendChild(summary);
    details.appendChild(el("pre", "", roh));
    ziel.appendChild(details);
  });
  if (gezeigt === 0) ziel.appendChild(leer("keine Nachricht enthält das"));
}

/// Was diese Nachricht ausmacht — bei Tool-Aufrufen die Werkzeugnamen, sonst der
/// Text. Ohne das stünde ausgerechnet bei den Tool-Zügen eine leere Zeile.
function vorschau(m) {
  if (Array.isArray(m.tool_calls) && m.tool_calls.length > 0) {
    const namen = m.tool_calls.map((c) => c.function?.name ?? "?").join(", ");
    return `→ ${namen}  ${m.content ?? ""}`;
  }
  return typeof m.content === "string" ? m.content : JSON.stringify(m.content ?? "");
}

async function zeitleiste(ziel) {
  const daten = await api("/api/timeline");
  ziel.textContent = "";
  ziel.appendChild(el("h3", "", "Zeitleiste"));
  const liste = el("div");
  liste.id = "strom";
  for (const e of daten.entries) liste.appendChild(zeitleistenZeile(e));
  ziel.appendChild(liste);
  if (daten.entries.length === 0) ziel.appendChild(leer("noch keine Ereignisse"));
}

function zeitleistenZeile(e) {
  const z = el("div", "zeile");
  z.appendChild(el("span", "zeit", zeit(e.at_ms)));
  z.appendChild(el("span", "typ", e.etype));
  z.appendChild(el("span", "quelle", e.source || "haupt"));
  z.appendChild(el("span", "text", e.label));
  z.onclick = () => {
    if (zustand.agent !== e.source) kontextZuruecksetzen();
    zustand.agent = e.source;
    zustand.reiter = "verlauf";
    zeichneAgenten();
    zeichneReiter();
    zeichneInhalt();
  };
  return z;
}

// -------------------------------------------------------------- Schwarm

// Farbe je `MessageKind` — die Art einer Nachricht soll man im Diagramm sehen,
// ohne zu lesen. Unbekannte Arten bekommen die Grundfarbe statt zu fehlen.
const KIND_FARBE = {
  task: "#7aa2f7", request: "#7dcfff", reply: "#73daca", observation: "#9ece6a",
  information: "#a9b1d6", proposal: "#bb9af7", critique: "#e0af68",
  vote: "#f7c66e", completion: "#9ece6a",
};

async function schwarm(ziel) {
  const s = await api("/api/swarm");
  ziel.textContent = "";
  if (!s.members.length) {
    return void ziel.appendChild(leer(
      "kein Schwarm in diesem Trace — Schwarm-Verkehr entsteht über das `swarm`-Tool " +
      "oder ein Work Item mit Schwarm-Vorlage."));
  }

  ziel.appendChild(el("h3", "", `Sequenz · ${s.members.length} Mitglieder · ${s.messages.length} Nachrichten`));
  ziel.appendChild(sequenzDiagramm(s));

  ziel.appendChild(el("h3", "", `Abstimmung · ${s.proposals.length} ${s.proposals.length === 1 ? "Vorschlag" : "Vorschläge"}`));
  if (s.proposals.length) {
    const tab = el("table");
    tab.appendChild(kopfzeile(["Vorschlag", "Von", "Zustimmung", "Ablehnung", "Ausgang", "Inhalt"]));
    for (const p of s.proposals) {
      const tr = datenzeile([
        p.id, p.from, p.approvals.join(", "), p.rejections.join(", "),
      ]);
      const ausgang = el("td", p.accepted ? "st-fertig" : "dim",
        p.accepted ? "angenommen" : "offen");
      tr.appendChild(ausgang);
      tr.appendChild(el("td", "", kurz(p.content, 100)));
      tab.appendChild(tr);
    }
    ziel.appendChild(tab);
  } else {
    ziel.appendChild(leer("noch kein Abschluss-Vorschlag"));
  }

  if (s.dead_letters.length) {
    ziel.appendChild(el("h3", "", `Unzustellbar · ${s.dead_letters.length}`));
    const tab = el("table");
    tab.appendChild(kopfzeile(["Zeit", "Von", "An", "Art", "Grund", "Inhalt"]));
    for (const d of s.dead_letters) {
      tab.appendChild(datenzeile([
        d.at_ms ? zeit(d.at_ms) : "", d.from, d.to, d.kind, d.reason, kurz(d.content, 80),
      ]));
    }
    ziel.appendChild(tab);
  }

  for (const r of s.results) {
    const k = el("div", "karte");
    k.appendChild(el("h3", "", "Ergebnis"));
    k.appendChild(el("div", "dim",
      `${abschlussgrund(r.reason)} · ${r.messages_sent} Zustellungen · ` +
      `${r.required_approvals} Zustimmung(en) nötig`));
    k.appendChild(el("pre", "", JSON.stringify(r, null, 2)));
    ziel.appendChild(k);
  }
}

/// `CompletionReason` ist extern getaggt: die Varianten OHNE Felder (`idle`,
/// `stopped`, …) sind ein blanker String, die MIT Feldern ein Objekt mit genau
/// einem Schlüssel. `Object.keys` auf einen String liefert dessen Indizes —
/// deshalb wird der Fall ausdrücklich unterschieden.
const abschlussgrund = (reason) =>
  typeof reason === "string" ? reason : Object.keys(reason || {})[0] || "?";

/// Das Sequenzdiagramm als handgeschriebenes SVG: Mitglieder als Spalten, Zeit
/// nach unten, Nachrichten als Pfeile. Bewusst kein Diagramm-Framework — es
/// sind Linien und Text, und das Repo soll keinen vendorten JS-Blob tragen.
function sequenzDiagramm(s) {
  const SPALTE = 170, ZEILE = 26, KOPF = 34, RAND = 20;
  const breite = RAND * 2 + Math.max(1, s.members.length - 1) * SPALTE + 120;
  const hoehe = KOPF + RAND + Math.max(1, s.messages.length) * ZEILE + 10;
  const x = (id) => RAND + 60 + s.members.indexOf(id) * SPALTE;

  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${breite} ${hoehe}`);
  svg.setAttribute("width", String(breite));
  svg.setAttribute("height", String(hoehe));
  svg.classList.add("sequenz");
  const knoten = (tag, attrs, text) => {
    const n = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
    if (text !== undefined) n.textContent = text;
    svg.appendChild(n);
    return n;
  };

  // Spaltenköpfe und Lebenslinien.
  s.members.forEach((m) => {
    knoten("text", { x: x(m), y: 16, "text-anchor": "middle", class: "kopf" }, m);
    knoten("line", { x1: x(m), y1: KOPF - 12, x2: x(m), y2: hoehe - 6, class: "lebenslinie" });
  });

  s.messages.forEach((m, i) => {
    const y = KOPF + i * ZEILE;
    const farbe = KIND_FARBE[m.kind] || "#a9b1d6";
    const vonX = x(m.from);
    // Broadcast: ein Balken über alle Spalten statt eines Pfeils ins Nichts.
    const nachX = s.members.includes(m.to) ? x(m.to) : breite - RAND - 40;
    const linie = knoten("line", { x1: vonX, y1: y, x2: nachX, y2: y });
    linie.setAttribute("stroke", farbe);
    linie.setAttribute("stroke-width", "1.5");
    if (!m.delivered) linie.setAttribute("stroke-dasharray", "4 3");
    // Pfeilspitze von Hand: ein Dreieck spart eine <defs>/<marker>-Definition,
    // die je Farbe eine eigene bräuchte.
    const richtung = nachX >= vonX ? -1 : 1;
    const spitze = knoten("polygon", {
      points: `${nachX},${y} ${nachX + richtung * 7},${y - 3.5} ${nachX + richtung * 7},${y + 3.5}`,
    });
    spitze.setAttribute("fill", farbe);
    const beschriftung = knoten("text", {
      x: (vonX + nachX) / 2, y: y - 5, "text-anchor": "middle", class: "pfeil",
    }, `${m.kind}${m.delivered ? "" : ` ✖ ${m.reason}`}: ${kurz(m.content, 46)}`);
    beschriftung.setAttribute("fill", m.delivered ? "#7f8895" : "#f7768e");
    const titel = document.createElementNS("http://www.w3.org/2000/svg", "title");
    titel.textContent =
      `${zeit(m.at_ms)} ${m.id}: ${m.from} → ${m.to} (${m.kind})\n${m.content}`;
    beschriftung.appendChild(titel);
  });

  const rahmen = el("div", "diagramm");
  rahmen.appendChild(svg);
  return rahmen;
}

// ---------------------------------------------------------------- Graph

// Farbe je Claim-Status. Ein `superseded` Claim soll man sofort von einem
// `confirmed` unterscheiden — das ist die Frage, mit der man den Graphen aufmacht.
const STATUS_FARBE = {
  observation: "#7dcfff", hypothesis: "#e0af68",
  confirmed: "#9ece6a", superseded: "#f7768e",
};

const graphFilter = { layer: "", scope: "", status: "" };

async function graph(ziel, erzwingen) {
  const g = await api("/api/graph");
  // Der Graph aendert sich nur mit seiner Revision. Ohne diesen Vergleich
  // liefe im Sekundentakt die ganze Kraftsimulation (220 Schritte x O(n^2))
  // plus ein SVG-Neubau — und die gerade geoeffnete Provenance-Karte waere
  // jedes Mal wieder weg, bevor man sie lesen kann.
  if (!erzwingen && g.revision === zustand.graphRevision) return;
  zustand.graphRevision = g.revision;
  ziel.textContent = "";

  const scopes = [...new Set(g.entities.map((e) => `${e.scope.kind}:${e.scope.id}`))].sort();
  const kopf = el("div", "karte");
  kopf.appendChild(auswahl("Ebene", ["working", "canonical"], "layer"));
  kopf.appendChild(auswahl("Scope", scopes, "scope"));
  kopf.appendChild(auswahl("Status", Object.keys(STATUS_FARBE), "status"));
  ziel.appendChild(kopf);

  const claims = g.claims.filter(passt);
  // Nur Entities zeigen, an denen nach dem Filtern noch eine Kante haengt —
  // eine Wolke unverbundener Punkte beantwortet keine Frage. Der Filter gilt
  // dabei NUR den Claims: eine Entity darf in einer anderen Ebene oder einem
  // anderen Scope liegen als die Kante, die auf sie zeigt (`resolve_or_create`
  // greift auf eine schon promotete Entity zu). Sie mitzufiltern liess genau
  // die Kanten verschwinden, die den Filter bestanden hatten.
  const beteiligt = new Set(claims.flatMap((c) => [c.subject, c.object]));
  const entities = g.entities.filter((e) => beteiligt.has(e.id));

  ziel.appendChild(el("h3", "", `Graph · Revision ${g.revision} · ` +
    `${entities.length}/${g.entities.length} Entities · ${claims.length}/${g.claims.length} Claims`));
  if (!entities.length) {
    return void ziel.appendChild(leer("nichts, was zu diesem Filter passt"));
  }
  ziel.appendChild(graphDiagramm(entities, claims, g.sources));

  ziel.appendChild(el("h3", "", "Claims"));
  const tab = el("table");
  tab.appendChild(kopfzeile(["Subjekt", "Prädikat", "Objekt", "Status", "Konfidenz", "Ebene", "Autor"]));
  const name = (id) => g.entities.find((e) => e.id === id)?.canonical_name || id;
  for (const c of claims) {
    const tr = datenzeile([name(c.subject), c.predicate, name(c.object)]);
    const status = el("td", "", c.status);
    status.style.color = STATUS_FARBE[c.status] || "";
    tr.appendChild(status);
    tr.appendChild(el("td", "", c.confidence.toFixed(2)));
    tr.appendChild(el("td", "", c.layer));
    tr.appendChild(el("td", "", c.created_by));
    tr.onclick = () => zeigeProvenance(ziel, c, g.sources, name);
    tab.appendChild(tr);
  }
  ziel.appendChild(tab);

  function passt(x) {
    return (!graphFilter.layer || x.layer === graphFilter.layer)
      && (!graphFilter.scope || `${x.scope.kind}:${x.scope.id}` === graphFilter.scope)
      && (!graphFilter.status || !x.status || x.status === graphFilter.status);
  }

  function auswahl(titel, werte, feld) {
    const wrap = el("span", "filter");
    wrap.appendChild(el("span", "dim", `${titel}: `));
    const sel = el("select");
    const alle = el("option", "", "(alle)");
    alle.value = "";
    sel.appendChild(alle);
    for (const w of werte) {
      const o = el("option", "", w);
      o.value = w;
      if (graphFilter[feld] === w) o.selected = true;
      sel.appendChild(o);
    }
    sel.onchange = () => { graphFilter[feld] = sel.value; zeichneInhalt(true); };
    wrap.appendChild(sel);
    return wrap;
  }
}

/// Die Provenance einer Kante: welche Quellen sie belegen. Bei Work-Claims
/// stehen dort Projekt, Lauf, Item, Versuch und Agent.
function zeigeProvenance(ziel, claim, sources, name) {
  const alt = document.getElementById("provenance");
  if (alt) alt.remove();
  const k = el("div", "karte");
  k.id = "provenance";
  k.appendChild(el("h3", "", `Provenance · ${name(claim.subject)} —${claim.predicate}→ ${name(claim.object)}`));
  if (claim.promoted_from) {
    k.appendChild(el("div", "dim",
      `promotet aus ${claim.promoted_from.kind}:${claim.promoted_from.id}`));
  }
  if (claim.superseded_by) {
    k.appendChild(el("div", "warnung", `ersetzt durch ${claim.superseded_by}`));
  }
  const tab = el("table");
  tab.appendChild(kopfzeile(["Quelle", "Art", "Agent", "Lauf", "Auszug"]));
  for (const id of claim.source_ids) {
    const q = sources.find((s) => s.id === id);
    tab.appendChild(datenzeile(q
      ? [q.id, q.source_type, q.agent_id ?? "", q.run_id ?? "", kurz(q.excerpt ?? "", 90)]
      : [id, "(Quelle fehlt im Graphen)", "", "", ""]));
  }
  k.appendChild(tab);
  ziel.appendChild(k);
  k.scrollIntoView({ block: "nearest" });
}

/// Knoten = Entities, Kanten = Claims. Das Layout ist eine handgeschriebene
/// Kraftsimulation: Kanten ziehen zusammen, alle Knoten stossen einander ab.
/// Bei Debug-Graphen mit Dutzenden bis Hunderten Knoten reicht das — und es
/// haelt einen vendorten d3/cytoscape-Blob aus dem Repo. Wird es zu langsam,
/// ist eine Bibliothek der dokumentierte naechste Schritt.
function graphDiagramm(entities, claims, sources) {
  const B = 900, H = 520, SCHRITTE = 220;
  const pos = new Map();
  // Startlage auf einem Kreis statt zufaellig: dieselbe Eingabe ergibt dasselbe
  // Bild, und ein Layout, das bei jedem Nachladen anders aussieht, ist beim
  // Vergleichen zweier Staende unbrauchbar.
  entities.forEach((e, i) => {
    const w = (2 * Math.PI * i) / entities.length;
    pos.set(e.id, { x: B / 2 + Math.cos(w) * 200, y: H / 2 + Math.sin(w) * 180 });
  });

  const kanten = claims
    .filter((c) => pos.has(c.subject) && pos.has(c.object) && c.subject !== c.object);
  const ids = [...pos.keys()];
  // Fruchterman-Reingold: Abstoßung k²/d, Anziehung d²/k, und je Schritt eine
  // Wegbegrenzung („Temperatur"), die linear auf null fällt. `k` ist der
  // Idealabstand — die Fläche je Knoten. Ohne diese Normierung fliegen die
  // Knoten schon im ersten Schritt an den Rand und kleben dort fest.
  const k = Math.sqrt((B * H) / Math.max(1, ids.length));
  for (let schritt = 0; schritt < SCHRITTE; schritt++) {
    const kraft = new Map(ids.map((id) => [id, { x: 0, y: 0 }]));
    // Abstoßung: jeder gegen jeden. O(n²), aber n ist hier zweistellig.
    for (let i = 0; i < ids.length; i++) {
      for (let j = i + 1; j < ids.length; j++) {
        const a = pos.get(ids[i]), b = pos.get(ids[j]);
        // Zwei Knoten exakt aufeinander hätten keine Richtung — ein winziger
        // fester Versatz löst das, ohne Zufall ins Layout zu bringen.
        const dx = a.x - b.x || 0.01, dy = a.y - b.y || 0.01;
        const d = Math.hypot(dx, dy);
        const f = (k * k) / d / d;
        kraft.get(ids[i]).x += dx * f; kraft.get(ids[i]).y += dy * f;
        kraft.get(ids[j]).x -= dx * f; kraft.get(ids[j]).y -= dy * f;
      }
    }
    // Anziehung entlang der Kanten.
    for (const c of kanten) {
      const a = pos.get(c.subject), b = pos.get(c.object);
      const dx = b.x - a.x, dy = b.y - a.y;
      const d = Math.hypot(dx, dy) || 0.01;
      const f = d / k;
      kraft.get(c.subject).x += dx * f; kraft.get(c.subject).y += dy * f;
      kraft.get(c.object).x -= dx * f; kraft.get(c.object).y -= dy * f;
    }
    // Schwerkraft zur Mitte. Ohne sie treiben Teilgraphen, die durch keine
    // Kante verbunden sind, unbegrenzt auseinander — die Abstoßung zwischen
    // ihnen hat nichts, was sie ausgleicht. Nach dem Einpassen wäre jede
    // Gruppe für sich dann winzig zusammengedrückt.
    for (const [id, p] of pos) {
      kraft.get(id).x += (B / 2 - p.x) * 0.12;
      kraft.get(id).y += (H / 2 - p.y) * 0.12;
    }
    const temperatur = (B / 10) * (1 - schritt / SCHRITTE);
    for (const [id, p] of pos) {
      const f = kraft.get(id);
      const laenge = Math.hypot(f.x, f.y) || 1;
      const schrittweite = Math.min(laenge, temperatur) / laenge;
      p.x += f.x * schrittweite;
      p.y += f.y * schrittweite;
    }
  }
  // Erst ZUM SCHLUSS in die Zeichenfläche legen, statt während der Simulation
  // an ihren Rändern zu klemmen: geklemmte Knoten kleben dort fest, weil die
  // Abstoßung sie weiter nach außen drückt, und das Bild wird zum Rahmen statt
  // zum Graphen. Die Skalierung ist einheitlich für x und y — sonst verzerrte
  // sie die Abstände, die die Simulation gerade erst ausgerechnet hat.
  einpassen(pos, B, H);

  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${B} ${H}`);
  svg.setAttribute("width", String(B));
  svg.setAttribute("height", String(H));
  svg.classList.add("graph");
  const knoten = (tag, attrs, text) => {
    const n = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
    if (text !== undefined) n.textContent = text;
    svg.appendChild(n);
    return n;
  };

  for (const c of kanten) {
    const a = pos.get(c.subject), b = pos.get(c.object);
    const farbe = STATUS_FARBE[c.status] || "#a9b1d6";
    const linie = knoten("line", { x1: a.x, y1: a.y, x2: b.x, y2: b.y });
    linie.setAttribute("stroke", farbe);
    linie.setAttribute("stroke-width", String(0.8 + c.confidence * 1.6));
    if (c.status === "superseded") linie.setAttribute("stroke-dasharray", "4 3");
    const beschriftung = knoten("text", {
      x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 - 3, "text-anchor": "middle", class: "kante",
    }, c.predicate);
    const titel = document.createElementNS("http://www.w3.org/2000/svg", "title");
    titel.textContent =
      `${c.predicate} (${c.status}, ${c.confidence.toFixed(2)}) · ${c.created_by}\n` +
      c.source_ids.map((id) => beleg(sources, id)).join("\n");
    beschriftung.appendChild(titel);
  }
  for (const e of entities) {
    const p = pos.get(e.id);
    const kreis = knoten("circle", { cx: p.x, cy: p.y, r: 5 });
    kreis.setAttribute("fill", e.layer === "canonical" ? "#9ece6a" : "#7aa2f7");
    const beschriftung = knoten("text", {
      x: p.x, y: p.y - 9, "text-anchor": "middle", class: "knoten",
    }, kurz(e.canonical_name, 28));
    const titel = document.createElementNS("http://www.w3.org/2000/svg", "title");
    titel.textContent =
      `${e.canonical_name} (${e.entity_type}, ${e.layer})\n` +
      `${e.scope.kind}:${e.scope.id}\n${e.description ?? ""}`;
    beschriftung.appendChild(titel);
  }

  const rahmen = el("div", "diagramm");
  rahmen.appendChild(svg);
  return rahmen;
}

/// Legt das fertige Layout mittig in die Zeichenfläche, mit Rand für die
/// Beschriftungen. Ein einzelner Knoten (oder mehrere auf demselben Punkt)
/// hätte eine Ausdehnung von null — dann bleibt der Maßstab 1.
function einpassen(pos, B, H) {
  const RAND_X = 70, RAND_Y = 26;
  const xs = [...pos.values()].map((p) => p.x);
  const ys = [...pos.values()].map((p) => p.y);
  const minX = Math.min(...xs), maxX = Math.max(...xs);
  const minY = Math.min(...ys), maxY = Math.max(...ys);
  const breite = maxX - minX, hoehe = maxY - minY;
  const massstab = Math.min(
    breite > 0 ? (B - 2 * RAND_X) / breite : 1,
    hoehe > 0 ? (H - 2 * RAND_Y) / hoehe : 1,
  );
  const versatzX = (B - breite * massstab) / 2 - minX * massstab;
  const versatzY = (H - hoehe * massstab) / 2 - minY * massstab;
  for (const p of pos.values()) {
    p.x = p.x * massstab + versatzX;
    p.y = p.y * massstab + versatzY;
  }
}

const beleg = (sources, id) => {
  const q = sources.find((s) => s.id === id);
  return q ? `${q.source_type} ${q.agent_id ?? ""} ${q.run_id ?? ""}`.trim() : id;
};

/// Der Benchmark-Reiter: alle Läufe im Ergebnisbaum, mit ihrem gemessenen
/// Ergebnis und dem Sprung in die Sitzung eines einzelnen Tasks.
///
/// Bewusst ohne `run=`-Bindung: dieser Reiter zeigt den ganzen BAUM, nicht die
/// gewählte Sitzung. Er ist der Überblick, aus dem man in eine Sitzung
/// hineingeht — die anderen Reiter sind die Sitzung selbst.
async function benchmarks(ziel) {
  const daten = await api("/api/benchmarks");
  ziel.textContent = "";
  if (!daten.runs.length) {
    return void ziel.appendChild(leer("keine Benchmark-Läufe unter der Trace-Wurzel"));
  }
  ziel.appendChild(el("h3", "", `Benchmark-Läufe · ${daten.runs.length}`));
  for (const lauf of daten.runs) {
    ziel.appendChild(benchLauf(lauf));
  }
}

const BENCH_OK = new Set(["resolved", "reward 1.0"]);

function benchLauf(lauf) {
  const d = el("details", "bench");
  const s = el("summary");
  s.appendChild(el("span", "bench-name", lauf.name));
  s.appendChild(el("span", "marke", lauf.kind));
  if (lauf.summary) {
    const { ok, total, source } = lauf.summary;
    const quote = total ? Math.round((100 * ok) / total) : 0;
    const zelle = el("span", `bench-quote ${ok > 0 ? "gut" : "null"}`, `${ok}/${total} · ${quote} %`);
    zelle.title = `Quelle: ${source}`;
    s.appendChild(zelle);
  } else {
    s.appendChild(el("span", "bench-quote offen", "nicht ausgewertet"));
  }
  s.appendChild(el("span", "dim", `${lauf.tasks.length} Tasks`));
  d.appendChild(s);

  // Kopfdaten des Treibers, soweit vorhanden — Modell und Startzeit sind das,
  // was einen Lauf von einem anderen unterscheidet.
  const m = lauf.meta || {};
  const kopf = [m.model_name, m.dataset, m.started_at].filter(Boolean).join(" · ");
  if (kopf) d.appendChild(el("p", "dim", kopf));

  const tab = el("table", "bench-tasks");
  for (const t of lauf.tasks) {
    const tr = el("tr");
    tr.appendChild(el("td", "", t.id));
    tr.appendChild(el("td", `status ${BENCH_OK.has(t.status) ? "ok" : t.status ? "rot" : ""}`,
      t.status || "—"));
    tr.appendChild(el("td", "dim", t.work ? "work" : ""));
    const sprung = el("td");
    if (t.session) {
      const b = el("button", "link", "Verlauf");
      b.title = t.session;
      // Genau das, was die Sitzungsauswahl oben rechts tut — nur von hier aus.
      b.onclick = () => {
        zustand.lauf = t.session;
        zustand.lastSeq = 0;
        zustand.agent = null;
        zustand.reiter = "verlauf";
        zeichneReiter();
        tick();
      };
      sprung.appendChild(b);
    }
    tr.appendChild(sprung);
    tab.appendChild(tr);
  }
  d.appendChild(tab);
  return d;
}

async function work(ziel) {
  const liste = await api("/api/work");
  ziel.textContent = "";
  if (!liste.projects.length) return void ziel.appendChild(leer(`keine Work-Projekte unter ${liste.work_dir}`));

  if (!zustand.projekt || !liste.projects.includes(zustand.projekt)) {
    zustand.projekt = liste.projects[0];
  }
  const wahl = el("select");
  for (const p of liste.projects) {
    const o = el("option", "", p);
    o.value = p;
    if (p === zustand.projekt) o.selected = true;
    wahl.appendChild(o);
  }
  wahl.onchange = () => { zustand.projekt = wahl.value; zeichneInhalt(); };
  const kopf = el("div", "karte");
  kopf.appendChild(el("span", "dim", "Projekt: "));
  kopf.appendChild(wahl);
  ziel.appendChild(kopf);

  const w = await api(`/api/work/${encodeURIComponent(zustand.projekt)}`);
  if (w.project) {
    const k = el("div", "karte");
    k.appendChild(el("h3", "", w.project.title || zustand.projekt));
    k.appendChild(el("div", "dim", w.project.objective || ""));
    ziel.appendChild(k);
  }

  const items = Object.values(w.items || {});
  ziel.appendChild(el("h3", "", `Work Items (${items.length})`));
  const tab = el("table");
  tab.appendChild(kopfzeile(["ID", "Titel", "Status", "Art", "Prio", "Versuche", "Abhängig von"]));
  for (const it of items) {
    const zeileD = datenzeile([
      it.id, it.title, "", it.kind, String(it.priority ?? ""),
      `${it.attempt_count ?? 0}/${it.max_attempts ?? 0}`,
      (it.dependencies || []).join(", "),
    ]);
    const statusZelle = zeileD.children[2];
    statusZelle.textContent = it.status;
    statusZelle.className = `st-${statusStil(it.status)}`;
    tab.appendChild(zeileD);
  }
  ziel.appendChild(tab);

  const artefakte = Object.values(w.artifacts || {});
  if (artefakte.length) {
    ziel.appendChild(el("h3", "", `Artefakte (${artefakte.length})`));
    const t2 = el("table");
    t2.appendChild(kopfzeile(["ID", "Item", "Art", "Pfad", "Zusammenfassung"]));
    for (const a of artefakte) {
      t2.appendChild(datenzeile([a.id, a.work_item_id ?? "", a.kind ?? "", a.path ?? "", kurz(a.summary ?? "", 80)]));
    }
    ziel.appendChild(t2);
  }
}

const statusStil = (s) =>
  s === "done" || s === "verified" ? "fertig" :
  s === "failed" || s === "blocked" ? "fehler" : "läuft";

function kopfzeile(spalten) {
  const tr = el("tr");
  for (const s of spalten) tr.appendChild(el("th", "", s));
  return tr;
}

function datenzeile(werte) {
  const tr = el("tr");
  for (const w of werte) tr.appendChild(el("td", "", w));
  return tr;
}

// ------------------------------------------------------------------ Takt

/// Muss der aktive Reiter vollständig neu gezeichnet werden?
///
/// Die Regel je Reiter, an einer Stelle: Verlauf und Zeitleiste hängen an
/// (`haengeAn`), Kontext und Schwarm wachsen nur mit einem passenden
/// `structured`-Datensatz, Work zieht in `tick` selbst nach (es hängt am
/// Work-Journal, nicht am Trace).
function brauchtNeuzeichnung(neu) {
  // Work und Graph haengen an ihren EIGENEN Dateien, nicht am Trace — sie
  // ziehen in `tick` selbst nach.
  if (zustand.reiter === "work" || zustand.reiter === "graph") return false;
  if (zustand.reiter === "kontext") {
    // Eine Zeitreise hält den Stand fest; wer gerade sucht, verlöre beim
    // Neuzeichnen den Fokus mitten im Tippen.
    if (zustand.kontextBis !== null) return false;
    if (document.activeElement?.classList?.contains("suche")) return false;
    return neu.some((e) => e.source === zustand.agent &&
      ["context_snapshot", "ctx_events"].includes(e.data?.structured?.kind));
  }
  if (zustand.reiter === "schwarm") {
    // Nur bei Schwarm-Datensätzen: `context_snapshot` kommt nach JEDEM Zug und
    // ließe die Ansicht sonst im Sekundentakt neu bauen, ohne dass sich am
    // Schwarm etwas geändert hätte.
    return neu.some((e) => (e.data?.structured?.kind || "").startsWith("swarm_"));
  }
  return !haengeAn(neu);
}

/// Hängt neue Ereignisse an die laufende Ansicht an, ohne sie neu zu bauen.
/// Gibt `true` zurück, wenn der Reiter das Nachladen selbst erledigt hat.
function haengeAn(neu) {
  const strom = document.getElementById("strom");
  if (!strom) return false;
  if (zustand.reiter === "verlauf") {
    for (const ev of neu) {
      if (ev.source === zustand.agent) strom.appendChild(ereignisKnoten(ev));
    }
    return true;
  }
  if (zustand.reiter === "zeitleiste") {
    for (const ev of neu) {
      strom.appendChild(zeitleistenZeile({
        at_ms: ev.at_ms, etype: ev.etype, source: ev.source, label: ev.label,
      }));
    }
    return true;
  }
  return false;
}

async function tick() {
  // Ein Takt darf den nächsten nicht überholen: `tick` ist asynchron, und bei
  // einer Antwort > 1 s liefe sonst ein zweiter Durchlauf mit demselben
  // `lastSeq` los — und hängte jedes Ereignis ein zweites Mal an.
  if (zustand.laeuft) return;
  zustand.laeuft = true;
  try {
    const lauf = await api("/api/runs");
    const info = document.getElementById("lauf");
    // `active_name` ist der Sitzungsname (Pfad relativ zur Trace-Wurzel) — die
    // Kennung, die auch als `run=` zurückgeht. `active` ist der volle
    // Betriebssystem-Pfad und taugt nur als Tooltip.
    const datei = lauf.active_name || "(kein Trace)";
    info.textContent = `${kurzeSitzung(datei)} · ${lauf.events} Ereignisse`;
    info.title = lauf.active || lauf.trace_dir;
    zeichneLaeufe(lauf);
    const hinweise = [];
    if (lauf.error) hinweise.push(lauf.error);
    if (lauf.skipped_lines > 0) hinweise.push(`${lauf.skipped_lines} unlesbare Zeile(n) übersprungen`);
    document.getElementById("hinweis").textContent = hinweise.join(" · ");

    // Der Work-Reiter hängt am Work-Journal, nicht am Trace — er muss auch dann
    // nachziehen, wenn im Trace nichts passiert (oder gar keiner geschrieben wird).
    if (zustand.reiter === "work" || zustand.reiter === "graph" || zustand.reiter === "benchmarks") {
      if (!document.getElementById("inhalt").contains(document.activeElement)) {
        await zeichneInhalt(false);
      }
    }

    // Die Datei ist der Anker, NICHT die Sequenznummer: zwei Läufe fangen beide
    // bei 1 an, ein Wechsel wäre an `last_seq` also nicht zuverlässig zu
    // erkennen (der neue Lauf kann längst weiter sein als der alte war).
    if (datei !== zustand.lauf) {
      zustand.lauf = lauf.active ? datei : null;
      zustand.lastSeq = 0;
      zustand.agent = null;
      kontextZuruecksetzen();
    }
    if (lauf.last_seq === zustand.lastSeq) return;

    const nachschub = await api("/api/events", { since: zustand.lastSeq });
    const erster = zustand.lastSeq === 0;

    const a = await api("/api/agents");
    zustand.agenten = a.agents;
    if (zustand.agent === null && a.agents.length) zustand.agent = a.agents[0].id;
    zeichneAgenten();

    if (erster || brauchtNeuzeichnung(nachschub.events)) await zeichneInhalt();
    // ERST jetzt weiterzählen: bricht oben etwas ab, wird derselbe Nachschub
    // im nächsten Takt erneut geholt, statt lautlos zu fehlen.
    zustand.lastSeq = nachschub.last_seq;
  } catch (e) {
    document.getElementById("lauf").textContent = String(e.message || e);
  } finally {
    zustand.laeuft = false;
  }
}

/// Die Auswahl der Trace-Dateien im Kopf. Ohne sie könnte der Nutzer bei zwei
/// parallelen Läufen nicht bestimmen, welchen er sieht — und der Server dürfte
/// die Datei nicht von sich aus wechseln (siehe `VizServer::refresh`).
function zeichneLaeufe(lauf) {
  const ziel = document.getElementById("laeufe");
  if (ziel.dataset.stand === String(lauf.files.length) && ziel.firstChild) return;
  ziel.dataset.stand = String(lauf.files.length);
  ziel.textContent = "";
  if (lauf.files.length < 2) return;
  const wahl = el("select");
  for (const f of lauf.files) {
    const o = el("option", "", `${kurzeSitzung(f.name)} (${f.bytes} B)`);
    o.value = f.name;
    o.title = f.name;
    if (zustand.lauf === f.name) o.selected = true;
    wahl.appendChild(o);
  }
  wahl.onchange = () => {
    zustand.lauf = wahl.value;
    zustand.lastSeq = 0;
    zustand.agent = null;
    kontextZuruecksetzen();
    tick();
  };
  ziel.appendChild(wahl);
}

zeichneReiter();
tick();
setInterval(() => {
  if (document.getElementById("auto").checked) tick();
}, 1000);
