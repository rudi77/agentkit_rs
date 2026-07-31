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
      a.kind === "haupt" ? "haupt" : a.kind === "sub_agent" ? "sub" : "schwarm"));
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
async function zeichneInhalt() {
  const ziel = document.getElementById("inhalt");
  try {
    if (zustand.reiter === "verlauf") await verlauf(ziel);
    else if (zustand.reiter === "kontext") await kontext(ziel);
    else if (zustand.reiter === "zeitleiste") await zeitleiste(ziel);
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
    if (zustand.reiter === "work") {
      if (!document.getElementById("inhalt").contains(document.activeElement)) {
        await zeichneInhalt();
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

    // Der Kontext-Reiter wächst nur mit einem neuen `context_snapshot`; ohne
    // einen bleibt er stehen, statt im Sekundentakt zuzuklappen.
    const kontextNeu = nachschub.events.some(
      (e) => e.source === zustand.agent && e.etype === "structured");
    if (erster || (zustand.reiter === "kontext" && kontextNeu)) {
      await zeichneInhalt();
    } else if (!haengeAn(nachschub.events) && zustand.reiter !== "kontext" && zustand.reiter !== "work") {
      await zeichneInhalt();
    }
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
