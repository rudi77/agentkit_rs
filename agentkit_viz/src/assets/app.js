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
};

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
    li.title = `${a.steps} Schritte · ${a.tool_calls} Tool-Aufrufe · ${a.errors} Fehler · ${a.status}`;
    li.onclick = () => { zustand.agent = a.id; zeichneAgenten(); zeichneInhalt(); };
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

async function kontext(ziel) {
  if (zustand.agent === null) {
    ziel.textContent = "";
    return void ziel.appendChild(leer("links einen Agenten wählen"));
  }
  const k = await api(`/api/agents/${encodeURIComponent(zustand.agent)}/context`);
  ziel.textContent = "";
  ziel.appendChild(el("h3", "", `Kontext · ${zustand.agent || "(Haupt-Agent)"}`));

  if (k.rekonstruiert) {
    ziel.appendChild(el("p", "warnung",
      "Rekonstruiert aus dem Ereignisstrom: für Sub-Agenten und Schwarm-Mitglieder gibt es " +
      "keine Kontext-Datensätze — System-Prompt und Verdichtungen fehlen deshalb hier."));
  }
  if (k.unvollstaendig) {
    ziel.appendChild(el("p", "warnung",
      "Lückenhaft: ein Kontext-Datensatz setzte hinter dem an, was im Trace steht — " +
      "die Nachrichten stimmen, ihre Positionen nicht."));
  }

  if (k.report) {
    const farben = ["#7aa2f7", "#9ece6a", "#e0af68", "#bb9af7", "#f7768e", "#7dcfff", "#73daca"];
    const summe = Math.max(1, k.report.total);
    const balken = el("div", "balken");
    k.report.segments.forEach((s, i) => {
      const teil = el("div");
      teil.style.width = `${(s.tokens / summe) * 100}%`;
      teil.style.background = farben[i % farben.length];
      teil.title = `${s.label}: ${s.tokens} Tokens`;
      balken.appendChild(teil);
    });
    const kopf = el("div", "karte");
    kopf.appendChild(el("div", "dim",
      `${k.report.total} von ${k.report.budget} Tokens${k.report.managed ? " · ctxman verwaltet" : ""}`));
    kopf.appendChild(balken);
    const tab = el("table");
    tab.appendChild(kopfzeile(["Abschnitt", "Tokens", "Einträge", "Hinweis"]));
    for (const s of k.report.segments) {
      tab.appendChild(datenzeile([s.label, String(s.tokens), String(s.count), s.note ?? ""]));
    }
    kopf.appendChild(tab);
    ziel.appendChild(kopf);
  }

  ziel.appendChild(el("h3", "", `Nachrichten (${k.messages.length})`));
  k.messages.forEach((m, i) => {
    const details = el("details");
    const summary = el("summary");
    summary.appendChild(el("span", "typ", `${i}. ${m.role ?? "?"}`));
    summary.appendChild(el("span", "text", kurz(m.content || JSON.stringify(m.tool_calls ?? ""))));
    details.appendChild(summary);
    details.appendChild(el("pre", "", JSON.stringify(m, null, 2)));
    ziel.appendChild(details);
  });
  if (k.messages.length === 0) ziel.appendChild(leer("kein Kontext aufgezeichnet"));
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
    return neu.some((e) => e.source === zustand.agent && e.etype === "structured");
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
    // Der Server liefert einen Betriebssystem-Pfad; der Dateiname ist der Teil
    // hinter dem letzten Trenner — unter Windows ist das ein Backslash.
    const datei = lauf.active ? lauf.active.split(/[\\/]/).pop() : "(kein Trace)";
    info.textContent = `${datei} · ${lauf.events} Ereignisse`;
    info.title = lauf.trace_dir;
    zeichneLaeufe(lauf);
    const hinweise = [];
    if (lauf.error) hinweise.push(lauf.error);
    if (lauf.skipped_lines > 0) hinweise.push(`${lauf.skipped_lines} unlesbare Zeile(n) übersprungen`);
    document.getElementById("hinweis").textContent = hinweise.join(" · ");

    // Der Work-Reiter hängt am Work-Journal, nicht am Trace — er muss auch dann
    // nachziehen, wenn im Trace nichts passiert (oder gar keiner geschrieben wird).
    if (zustand.reiter === "work" || zustand.reiter === "graph") {
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
    const o = el("option", "", `${f.name} (${f.bytes} B)`);
    o.value = f.name;
    if (zustand.lauf === f.name) o.selected = true;
    wahl.appendChild(o);
  }
  wahl.onchange = () => {
    zustand.lauf = wahl.value;
    zustand.lastSeq = 0;
    zustand.agent = null;
    tick();
  };
  ziel.appendChild(wahl);
}

zeichneReiter();
tick();
setInterval(() => {
  if (document.getElementById("auto").checked) tick();
}, 1000);
