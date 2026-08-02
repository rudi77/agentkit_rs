//! Feature-Tests auf SESSION-Ebene — die Nähte zwischen den Bausteinen.
//!
//! Die vorhandenen Suiten decken die Bausteine je für sich ab: `domain` die
//! Invarianten eines Segments, `gc_minor`/`gc_major` die Sammelpläne,
//! `rendering` den Planer und die Adapter, `orchestration` den Hot Path.
//!
//! Der Fehler, der 10 von 64 Benchmark-Tasks getötet hat, saß in keiner
//! davon — er saß DAZWISCHEN: der Planer meldete die offene Unit korrekt, das
//! Heilen hängte korrekt an, der Adapter formatierte korrekt, und in Summe
//! entstand eine Nachrichtenfolge, die der Provider ablehnt. Diese Datei
//! prüft deshalb Zusammenspiele, nicht Einzelteile.

use ctxman::domain::{OnToolRemoved, PolicyConfig, RenderScope, Role, SegmentState};
use ctxman::rendering::static_diff::StaticSegmentSpec;
use ctxman::storage::FileSystemBlobStore;
use ctxman::{AppendRequest, ContextSession, CtxmanServices, CtxmanStore, RenderOptions};

fn services() -> CtxmanServices {
    CtxmanServices {
        clock: Box::new(|| 0),
        ..Default::default()
    }
}

fn session() -> ContextSession {
    ContextSession::new(PolicyConfig::default_policy(), services())
}

fn render(s: &mut ContextSession, scope: RenderScope) -> serde_json::Value {
    s.render(RenderOptions {
        provider: "openai".to_string(),
        scope,
        turn_advance: false,
    })
    .expect("Render muss gelingen")
    .request_fragment
}

/// Alle Textinhalte der gerenderten Nachrichten, zum Suchen.
fn texte(fragment: &serde_json::Value) -> String {
    fragment["messages"].to_string()
}

// ---------------------------------------------------------------- Frames

/// `scope = frame` ist die Sub-Agenten-Sicht: Der Sub-Agent sieht seinen
/// EIGENEN Frame plus gepinnte Root-Segmente — nicht den ganzen Verlauf des
/// Orchestrators.
///
/// Auf Planer-Ebene ist das geprüft; hier über die Session, weil erst dort
/// `push_frame` die `frame_id` vergibt und der Tip-Frame ermittelt wird.
#[test]
fn frame_scope_zeigt_dem_subagenten_nur_seinen_eigenen_kontext() {
    let mut s = session();
    s.append_segment(AppendRequest::inline(
        "user_msg",
        Some(Role::User),
        "GEHEIM-ROOT: langer Verlauf des Orchestrators",
    ))
    .unwrap();
    s.append_segment(AppendRequest {
        pinned: true,
        ..AppendRequest::inline("user_msg", Some(Role::User), "MERKE-ROOT: gilt überall")
    })
    .unwrap();

    let frame = s.push_frame("subagent");
    s.append_segment(AppendRequest::inline(
        "user_msg",
        Some(Role::User),
        "AUFTRAG-FRAME: nur für den Sub-Agenten",
    ))
    .unwrap();

    // Sicht des Orchestrators: alles.
    let pfad = texte(&render(&mut s, RenderScope::Path));
    assert!(pfad.contains("GEHEIM-ROOT"));
    assert!(pfad.contains("AUFTRAG-FRAME"));

    // Sicht des Sub-Agenten: der eigene Frame plus GEPINNTES aus dem Root.
    let eigen = texte(&render(&mut s, RenderScope::Frame));
    assert!(eigen.contains("AUFTRAG-FRAME"), "der eigene Auftrag fehlt");
    assert!(eigen.contains("MERKE-ROOT"), "gepinntes Root-Wissen fehlt");
    assert!(
        !eigen.contains("GEHEIM-ROOT"),
        "der Sub-Agent sieht den ungepinnten Verlauf des Orchestrators: {eigen}"
    );

    // Nach dem Pop ist der Frame geschlossen — sein Inhalt verschwindet aus
    // der Pfad-Sicht, das Rückgabe-Segment bleibt.
    s.pop_frame(frame, "ERGEBNIS: fertig", None).unwrap();
    let danach = texte(&render(&mut s, RenderScope::Path));
    assert!(
        !danach.contains("AUFTRAG-FRAME"),
        "der geschlossene Frame wird weiter gerendert"
    );
    assert!(danach.contains("ERGEBNIS"), "das Rückgabe-Segment fehlt");
}

// ------------------------------------------------------------------ Pin

/// Gepinnte Segmente sind für JEDE GC-Stufe unantastbar (Spec §2.2 I1) —
/// geprüft über die Session, nicht über den Sammelplan.
#[test]
fn gepinntes_ueberlebt_minor_und_major_gc() {
    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = 100;
    let mut s = ContextSession::new(policy, services());

    let id = s
        .append_segment(AppendRequest {
            pinned: true,
            ..AppendRequest::inline("user_msg", Some(Role::User), "UNANTASTBAR")
        })
        .unwrap();

    let _ = s.run_minor_gc();
    let _ = s.run_major_gc();

    let seg = s
        .segments()
        .iter()
        .find(|x| x.id() == id)
        .expect("Segment muss es noch geben");
    assert_eq!(
        seg.state(),
        SegmentState::Live,
        "gepinntes Segment wurde vom GC angefasst"
    );
    assert!(texte(&render(&mut s, RenderScope::Path)).contains("UNANTASTBAR"));

    // Erst nach `unpin` ist es überhaupt GC-fähig.
    s.unpin(id).unwrap();
    assert!(!s.segments().iter().find(|x| x.id() == id).unwrap().pinned());
}

// -------------------------------------------------- Units unter Eviction

/// Die Naht, an der der Produktionsfehler saß: ein `tool_result`, das nicht
/// mehr render-eligible ist, darf seinen `tool_call` NICHT verwaist rendern
/// lassen — der Planer muss die Unit als offen melden (I5), damit der
/// Aufrufer sie heilen kann.
///
/// Ohne diese Meldung ginge eine Assistant-Nachricht mit `tool_calls` ohne
/// Antwort an den Provider, und der lehnt den ganzen Request ab.
#[test]
fn ein_evictetes_ergebnis_laesst_seinen_aufruf_nicht_verwaisen() {
    let mut s = session();
    s.append_segment(AppendRequest {
        tool_call_id: Some("call_1".to_string()),
        source: Some("run_shell".to_string()),
        ..AppendRequest::inline("tool_call", Some(Role::Assistant), "{\"cmd\":\"ls\"}")
    })
    .unwrap();
    // `refetchable` ⇒ das Ergebnis fällt der Clean-Page-Eviction zum Opfer
    // (Phase 1, Spec §3.2.1), und die arbeitet SEGMENTWEISE — anders als die
    // TTL-Eviction in Phase 3, die eine gekoppelte Unit immer ganz nimmt.
    // Genau hier kann ein Aufruf allein zurückbleiben.
    s.append_segment(AppendRequest {
        tool_call_id: Some("call_1".to_string()),
        refetchable: true,
        ..AppendRequest::inline("tool_result", Some(Role::Tool), "a.txt")
    })
    .unwrap();

    // Vollständige Unit rendert sauber.
    let fragment = render(&mut s, RenderScope::Path);
    pruefe_openai_paarung(&fragment);

    // Turns hochzählen, bis die Kind-TTL (tool_result: 2) überschritten ist.
    // Render allein rührt `last_referenced_turn` nicht an — nur ein Page Fault.
    for _ in 0..3 {
        s.render(RenderOptions {
            provider: "openai".to_string(),
            scope: RenderScope::Path,
            turn_advance: true,
        })
        .unwrap();
    }
    let bericht = s.run_minor_gc().unwrap();
    assert_eq!(
        bericht.clean_page_evicted.len(),
        1,
        "die Clean-Page-Eviction hat das Ergebnis nicht genommen: {bericht:?}"
    );

    let fehler = s.render(RenderOptions {
        provider: "openai".to_string(),
        scope: RenderScope::Path,
        turn_advance: false,
    });
    match fehler {
        Err(ctxman::CtxmanError::IncompleteUnits { open_tool_call_ids }) => {
            assert_eq!(open_tool_call_ids, vec!["call_1".to_string()]);
        }
        other => panic!("erwartet: offene Unit gemeldet, bekommen: {other:?}"),
    }
}

/// Die Zusicherung des OpenAI-Formats als wiederverwendbare Prüfung: auf jede
/// Assistant-Nachricht mit `tool_calls` folgen unmittelbar ihre Antworten.
fn pruefe_openai_paarung(fragment: &serde_json::Value) {
    let messages = fragment["messages"].as_array().expect("messages");
    for (i, m) in messages.iter().enumerate() {
        let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array()) else {
            continue;
        };
        let folgend: Vec<&str> = messages[i + 1..]
            .iter()
            .take_while(|n| n["role"] == "tool")
            .filter_map(|n| n["tool_call_id"].as_str())
            .collect();
        for c in calls {
            let id = c["id"].as_str().unwrap_or("");
            assert!(
                folgend.contains(&id),
                "Nachricht {i} ruft '{id}' auf, die Antwort folgt nicht unmittelbar"
            );
        }
    }
}

/// Baut die Lage, in der der Produktionsfehler entstand: Aufruf, dazwischen
/// anderer Verkehr, und erst danach die nachgereichte Antwort.
fn session_mit_nachgereichter_antwort() -> ContextSession {
    let mut s = session();
    s.append_segment(AppendRequest {
        tool_call_id: Some("call_1".to_string()),
        source: Some("git".to_string()),
        ..AppendRequest::inline("tool_call", Some(Role::Assistant), "{\"cmd\":\"status\"}")
    })
    .unwrap();
    s.append_segment(AppendRequest::inline(
        "user_msg",
        Some(Role::User),
        "und weiter geht es",
    ))
    .unwrap();
    s.append_segment(AppendRequest {
        tool_call_id: Some("call_1".to_string()),
        ..AppendRequest::inline("tool_result", Some(Role::Tool), "(nachgereicht)")
    })
    .unwrap();
    s
}

/// Dieselbe Lage, aus der Sicht der Anthropic-API. Deren Zusicherung ist
/// strenger als die von OpenAI: die `tool_result`-Blöcke müssen in der
/// UNMITTELBAR FOLGENDEN Nachricht stehen, nicht bloß irgendwann danach.
#[test]
fn anthropic_tool_ergebnis_steht_in_der_naechsten_nachricht() {
    let mut s = session_mit_nachgereichter_antwort();
    let fragment = s
        .render(RenderOptions {
            provider: "anthropic".to_string(),
            scope: RenderScope::Path,
            turn_advance: false,
        })
        .unwrap()
        .request_fragment;

    let messages = fragment["messages"].as_array().expect("messages");
    for (i, m) in messages.iter().enumerate() {
        let blocks = m["content"].as_array().expect("content-Blöcke");
        let benutzt: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["id"].as_str())
            .collect();
        if benutzt.is_empty() {
            continue;
        }
        let naechste = messages
            .get(i + 1)
            .expect("nach tool_use fehlt die Antwort");
        let beantwortet: Vec<&str> = naechste["content"]
            .as_array()
            .map(|bs| {
                bs.iter()
                    .filter(|b| b["type"] == "tool_result")
                    .filter_map(|b| b["tool_use_id"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        for id in benutzt {
            assert!(
                beantwortet.contains(&id),
                "tool_use '{id}' in Nachricht {i}: die nächste Nachricht antwortet nicht darauf — {naechste}"
            );
        }
    }
}

// ------------------------------------------------------------ Epoch-Bump

/// `OnToolRemoved` steuert, was mit den Units eines entfernten Tools
/// passiert (Spec §4.2). `externalize` ist getestet; `keep` und `evict` sind
/// die beiden anderen Ausgänge und verhielten sich bisher ungeprüft.
#[test]
fn on_tool_removed_keep_und_evict_verhalten_sich_unterschiedlich() {
    for (modus, erwartet_live) in [(OnToolRemoved::Keep, true), (OnToolRemoved::Evict, false)] {
        let mut policy = PolicyConfig::default_policy();
        policy.on_tool_removed = modus;
        let mut s = ContextSession::new(policy, services());

        // Epoche 1: das Tool existiert.
        s.bump_static_epoch(vec![StaticSegmentSpec {
            kind: "tool_def".to_string(),
            role: None,
            content: "{\"name\":\"altes_tool\"}".to_string(),
            source: Some("altes_tool".to_string()),
        }])
        .unwrap();

        // Eine Unit, die es benutzt.
        let aufruf = s
            .append_segment(AppendRequest {
                tool_call_id: Some("c1".to_string()),
                source: Some("altes_tool".to_string()),
                ..AppendRequest::inline("tool_call", Some(Role::Assistant), "{}")
            })
            .unwrap();
        let ergebnis = s
            .append_segment(AppendRequest {
                tool_call_id: Some("c1".to_string()),
                ..AppendRequest::inline("tool_result", Some(Role::Tool), "Ergebnis")
            })
            .unwrap();

        // Epoche 2: das Tool ist weg.
        let diff = s.bump_static_epoch(Vec::new()).unwrap();
        assert_eq!(diff.diff.removed_tools, vec!["altes_tool".to_string()]);

        for id in [aufruf, ergebnis] {
            let seg = s.segments().iter().find(|x| x.id() == id).unwrap();
            assert_eq!(
                seg.state() == SegmentState::Live,
                erwartet_live,
                "{modus:?}: Zustand {:?} entspricht nicht der Erwartung",
                seg.state()
            );
        }

        // Und was auch immer der Modus tut — die Ordnung muss halten:
        // `evict` darf keinen verwaisten Aufruf hinterlassen.
        if let Ok(r) = s.render(RenderOptions {
            provider: "openai".to_string(),
            scope: RenderScope::Path,
            turn_advance: false,
        }) {
            pruefe_openai_paarung(&r.request_fragment);
        }
    }
}

// ------------------------------------------------------- Mehrere Sessions

/// Die Registry hält Sessions getrennt — ein Agent darf nie den Kontext eines
/// anderen sehen.
#[test]
fn sessions_sind_voneinander_isoliert() {
    let mut reg = CtxmanStore::new(services);
    let a = reg.create_session(PolicyConfig::default_policy());
    let b = reg.create_session(PolicyConfig::default_policy());
    assert_ne!(a, b);

    reg.session_mut(a)
        .unwrap()
        .append_segment(AppendRequest::inline(
            "user_msg",
            Some(Role::User),
            "NUR-IN-A",
        ))
        .unwrap();

    let in_b = texte(&render(reg.session_mut(b).unwrap(), RenderScope::Path));
    assert!(
        !in_b.contains("NUR-IN-A"),
        "Session B sieht den Inhalt von A"
    );

    assert_eq!(reg.session_ids().len(), 2);
    assert!(reg.remove(a).is_some());
    assert_eq!(reg.session_ids().len(), 1);
    assert!(
        reg.session_mut(a).is_none(),
        "entfernte Session lebt weiter"
    );
}

// -------------------------------------------------- Major GC end-to-end

/// Minimales Compaction-Modell: liefert für die Faktenextraktion und für die
/// Verdichtung je einen festen Text.
struct FestesModell;

impl ctxman::compaction::CompactionModel for FestesModell {
    fn summarize(
        &self,
        request: &ctxman::compaction::CompactionRequest,
    ) -> Result<ctxman::compaction::CompactionResult, ctxman::CtxmanError> {
        Ok(ctxman::compaction::CompactionResult {
            summary: if request.prompt_template_id == "fact-extraction-v1" {
                String::new() // keine Promotion — hier geht es um die Verdichtung
            } else {
                "ZUSAMMENFASSUNG des verdichteten Fensters".to_string()
            },
        })
    }
}

/// Die dritte Naht, an der ein verwaister Aufruf entstehen könnte: Major GC
/// ersetzt ein ganzes Fenster durch EIN Summary-Segment. Nimmt es einen
/// `tool_call` mit und lässt sein `tool_result` stehen (oder umgekehrt), geht
/// derselbe kaputte Request an den Provider wie beim nachgereichten Ergebnis.
///
/// Der Sammelplan hält Units atomar (in `gc_major` geprüft) — dieser Test
/// prüft, dass davon bis zum fertigen Wire-Format nichts verloren geht.
#[test]
fn nach_major_gc_bleibt_die_openai_ordnung_intakt() {
    let mut policy = PolicyConfig::default_policy();
    // Groß genug, dass der Render nach der Verdichtung wieder durchgeht —
    // geprüft wird die Ordnung, nicht das Budget.
    policy.budget_tokens = 2_000;
    // Das Fenster (max_share × budget) fasst nur die ältesten Units — die
    // jüngeren müssen die Verdichtung überleben, sonst prüft die Paarung nichts.
    policy.compaction.max_share = 0.1;
    let services = CtxmanServices {
        compaction_model: Some(Box::new(FestesModell)),
        clock: Box::new(|| 0),
        ..Default::default()
    };
    let mut s = ContextSession::new(policy, services);

    // Mehrere vollständige Units plus Fließtext dazwischen.
    for i in 0..4 {
        let id = format!("call_{i}");
        s.append_segment(AppendRequest {
            tool_call_id: Some(id.clone()),
            source: Some("run_shell".to_string()),
            ..AppendRequest::inline(
                "tool_call",
                Some(Role::Assistant),
                &format!("{{\"cmd\":\"schritt {i}\"}}"),
            )
        })
        .unwrap();
        s.append_segment(AppendRequest {
            tool_call_id: Some(id),
            ..AppendRequest::inline(
                "tool_result",
                Some(Role::Tool),
                &"Ausgabe des Schritts, ausfuehrlich. ".repeat(20),
            )
        })
        .unwrap();
        s.append_segment(AppendRequest::inline(
            "user_msg",
            Some(Role::User),
            "und weiter",
        ))
        .unwrap();
    }

    let bericht = s.run_major_gc().unwrap();
    assert!(
        bericht.summary_segment_id.is_some(),
        "es wurde nichts verdichtet — der Test prüft dann nichts: {bericht:?}"
    );
    assert!(bericht.tokens_after < bericht.tokens_before as u32);

    let fragment = render(&mut s, RenderScope::Path);
    pruefe_openai_paarung(&fragment);
    // Ohne überlebende Unit prüfte die Paarung nichts.
    assert!(
        fragment["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.get("tool_calls").is_some()),
        "keine Unit hat die Verdichtung überlebt — die Paarungsprüfung liefe ins Leere: {fragment}"
    );
    assert!(
        texte(&fragment).contains("ZUSAMMENFASSUNG"),
        "das Summary-Segment fehlt im Render"
    );
}

// ---------------------------------------------------------- Page Fault

/// Ein Page Fault ist mehr als Blob-Laden: er setzt `last_referenced_turn`
/// des Ursprungssegments neu (Spec §3.4). Praktische Folge — genau deshalb
/// steht es dort: was der Agent gerade wieder gebraucht hat, überlebt den
/// nächsten GC-Lauf, statt sofort wieder eingesammelt zu werden.
#[test]
fn ein_page_fault_verlaengert_die_lebenszeit_des_segments() {
    let mut policy = PolicyConfig::default_policy();
    policy.externalize_threshold_tokens = 1;
    let mut s = ContextSession::new(policy, services());

    let gross = s
        .append_segment(AppendRequest::inline(
            "tool_result",
            Some(Role::Tool),
            &"viel Text. ".repeat(200),
        ))
        .unwrap();

    let bericht = s.run_minor_gc().unwrap();
    assert_eq!(bericht.externalized, vec![gross], "nicht externalisiert");

    // Turns weit über die TTL hinaus …
    for _ in 0..10 {
        s.render(RenderOptions {
            provider: "openai".to_string(),
            scope: RenderScope::Path,
            turn_advance: true,
        })
        .unwrap();
    }

    // … aber der Agent holt den Inhalt zurück.
    let expansion = s.expand_ref(gross).unwrap();
    assert!(expansion.content.contains("viel Text"));

    // Damit ist das Segment wieder frisch: der nächste GC nimmt es nicht.
    let bericht = s.run_minor_gc().unwrap();
    assert!(
        !bericht.clean_page_evicted.contains(&gross)
            && !bericht
                .unit_evicted
                .iter()
                .any(|u| u.segment_ids.contains(&gross)),
        "das eben benutzte Segment wurde sofort wieder eingesammelt: {bericht:?}"
    );
}

// ---------------------------------------------------------- Watermarks

/// Die Watermark-Leiter (Spec §3.1) ist die eigentliche Steuerung des
/// Systems: sie entscheidet, wann gesammelt wird und wann der Hot Path
/// abbricht. Einzeln sind die Stufen geprüft; hier die Reihenfolge als
/// Ganzes — sie muss monoton eskalieren, nie springen oder zurückfallen.
#[test]
fn die_watermark_leiter_eskaliert_monoton() {
    use ctxman::domain::WatermarkLevel;
    use ctxman::gc::GcLevel;

    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = 400;
    let mut s = ContextSession::new(policy, services());

    let mut gesehen: Vec<WatermarkLevel> = Vec::new();
    let mut empfehlungen: Vec<Option<GcLevel>> = Vec::new();
    for i in 0..40 {
        // Gepinnt, damit der Hot-Path-GC nichts wegräumen kann und der
        // Füllstand wirklich steigt.
        s.append_segment(AppendRequest {
            pinned: true,
            ..AppendRequest::inline(
                "user_msg",
                Some(Role::User),
                &format!("Nachricht {i}: {}", "Inhalt ".repeat(10)),
            )
        })
        .unwrap();
        match s.render(RenderOptions {
            provider: "openai".to_string(),
            scope: RenderScope::Path,
            turn_advance: false,
        }) {
            Ok(out) => {
                if gesehen.last() != Some(&out.watermark) {
                    gesehen.push(out.watermark);
                    empfehlungen.push(out.recommended_gc);
                }
            }
            // Über dem Budget bricht der Hot Path ab (Spec §3.1) — das IST die
            // oberste Stufe, auch wenn kein Render-Ergebnis mehr entsteht.
            Err(ctxman::CtxmanError::BudgetExceeded { .. }) => {
                if gesehen.last() != Some(&WatermarkLevel::Emergency) {
                    gesehen.push(WatermarkLevel::Emergency);
                    empfehlungen.push(None);
                }
                break;
            }
            other => panic!("unerwartet: {other:?}"),
        }
    }

    assert_eq!(
        gesehen.first(),
        Some(&WatermarkLevel::Ok),
        "der leere Context startet nicht bei ok: {gesehen:?}"
    );
    assert_eq!(
        gesehen.last(),
        Some(&WatermarkLevel::Emergency),
        "der volle Context endet nicht im Notfall: {gesehen:?}"
    );
    let rang = |w: &WatermarkLevel| match w {
        WatermarkLevel::Ok => 0,
        WatermarkLevel::Soft => 1,
        WatermarkLevel::Hard => 2,
        WatermarkLevel::Emergency => 3,
    };
    assert!(
        gesehen.windows(2).all(|p| rang(&p[0]) < rang(&p[1])),
        "die Leiter eskaliert nicht monoton: {gesehen:?}"
    );
    // Über `soft` empfiehlt der Render immer eine Sammlung — sonst wüsste der
    // Aufrufer nicht, dass er etwas tun soll.
    for (w, e) in gesehen.iter().zip(&empfehlungen) {
        if rang(w) == 1 || rang(w) == 2 {
            assert!(e.is_some(), "{w:?} ohne GC-Empfehlung");
        }
    }
}

// ----------------------------------------------------------- Snapshots

/// Ein Snapshot ist der Wiedereinstieg nach einem Neustart (agentkit legt ihn
/// nach jedem Lauf ab). Der bestehende Roundtrip-Test prüft den einfachen
/// Fall; hier der harte: ein offener Frame und ein externalisiertes Segment,
/// dessen Inhalt nur noch im Blob Store liegt. Übersteht der Blob-Verweis den
/// Neustart nicht, ist der Inhalt verloren, ohne dass es jemand merkt.
#[test]
fn ein_snapshot_uebersteht_den_neustart_mit_frame_und_blob() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    let datei = dir.join("features-neustart.json");
    let blob_dir = dir.join("features-neustart-blobs");
    std::fs::create_dir_all(&blob_dir).unwrap();

    let gross = "sehr viel Inhalt. ".repeat(200);
    let (segment_id, frame_id) = {
        let mut policy = PolicyConfig::default_policy();
        policy.externalize_threshold_tokens = 1;
        let mut s = ContextSession::new(
            policy,
            CtxmanServices {
                blob_store: Box::new(FileSystemBlobStore::new(&blob_dir)),
                clock: Box::new(|| 0),
                ..Default::default()
            },
        );
        let frame_id = s.push_frame("subagent");
        let segment_id = s
            .append_segment(AppendRequest::inline(
                "tool_result",
                Some(Role::Tool),
                &gross,
            ))
            .unwrap();
        s.run_minor_gc().unwrap();
        s.save_to_file(&datei).unwrap();
        (segment_id, frame_id)
    };

    // Neustart: neue Session-Instanz, gleicher Blob Store.
    let mut s = ContextSession::load_from_file(
        &datei,
        CtxmanServices {
            blob_store: Box::new(FileSystemBlobStore::new(&blob_dir)),
            clock: Box::new(|| 0),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        s.segments()
            .iter()
            .find(|x| x.id() == segment_id)
            .unwrap()
            .state(),
        SegmentState::Externalized,
        "der externalisierte Zustand ging beim Laden verloren"
    );
    assert_eq!(
        s.expand_ref(segment_id).unwrap().content,
        gross,
        "der Blob-Inhalt ist nach dem Neustart nicht mehr erreichbar"
    );
    assert!(
        s.frames().iter().any(|f| f.id() == frame_id),
        "der offene Frame überlebte den Neustart nicht"
    );
    // Und der Frame lässt sich normal schließen — der Zustand ist wirklich da,
    // nicht bloß eine Datenleiche.
    s.pop_frame(frame_id, "ERGEBNIS nach Neustart", None)
        .unwrap();

    std::fs::remove_file(&datei).ok();
    std::fs::remove_dir_all(&blob_dir).ok();
}
