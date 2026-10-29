//! Hintergrund-Sub-Agenten — die Mailbox des Orchestrators.
//!
//! Dasselbe Muster wie ein Actor in `agentkit_swarm` (`actor_loop`): Arbeit läuft
//! in einem eigenen Thread, ihr Ergebnis landet als Nachricht in der Mailbox des
//! Empfängers, und der Empfänger wartet per `recv_timeout`-Takt, damit der
//! Stop-Knopf auch während des Wartens greift. Der Code selbst wird nicht geteilt:
//! der Agent-Kern darf den Schwarm nicht kennen (Abhängigkeit nur in eine
//! Richtung), und das Gemeinsame ist kaum mehr als ein `mpsc`-Kanal.
//!
//! Wer einspeist: das `task`-Tool mit `run_in_background` ([`Background::spawn`]).
//! Wer abholt: der Agent-Loop — an jeder Schrittgrenze ([`Background::take_finished`])
//! und blockierend, wenn das Modell seinen Zug beendet, während noch Arbeit läuft
//! ([`Background::wait_finished`]).

use crate::agent::{new_cancel, stopped, Cancel};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// Takt, in dem das Warten den Stop-Knopf prüft — derselbe wie im Schwarm-Actor.
const WARTE_TAKT: Duration = Duration::from_millis(100);

/// Ergebnis eines fertigen Hintergrund-Sub-Agenten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// Die beim Start vergebene ID (`bg-1`, `bg-2`, …).
    pub id: String,
    /// Die Rolle (`subagent_type`) des Sub-Agenten.
    pub kind: String,
    /// Seine finale Antwort (inkl. Fehler-Sentinels wie `"(abgebrochen)"`).
    pub result: String,
}

/// Mailbox für die Hintergrund-Sub-Agenten EINES Laufs.
///
/// Unbegrenzt (`channel`, nicht `sync_channel` wie im Schwarm): Backpressure gäbe
/// es hier nichts zu regeln — jede Nachricht ist das Ergebnis eines Starts, den der
/// Orchestrator selbst ausgelöst hat, und ein verworfenes Ergebnis wäre verlorene
/// Arbeit.
pub struct Background {
    tx: Sender<Finished>,
    rx: Receiver<Finished>,
    /// Gestartet, aber noch nicht abgeholt.
    running: usize,
    next_id: usize,
    /// Stop-Knopf ALLER Sub-Agenten dieser Mailbox — siehe [`Background::stop`].
    stop: Cancel,
}

impl Default for Background {
    fn default() -> Self {
        let (tx, rx) = channel();
        Background {
            tx,
            rx,
            running: 0,
            next_id: 1,
            stop: new_cancel(),
        }
    }
}

impl Background {
    /// Startet `work` in einem eigenen Thread und gibt sofort die vergebene ID
    /// zurück; das Ergebnis kommt später über die Mailbox. `work` bekommt den
    /// Stop-Knopf, den es seinem Sub-Agenten mitgeben soll.
    pub fn spawn<F>(&mut self, kind: &str, work: F) -> String
    where
        F: FnOnce(Cancel) -> String + Send + 'static,
    {
        let stop = self.stop.clone();
        let id = format!("bg-{}", self.next_id);
        self.next_id += 1;
        self.running += 1;
        let zustellung = Zustellung {
            tx: self.tx.clone(),
            id: id.clone(),
            kind: kind.to_string(),
            result: None,
        };
        std::thread::spawn(move || {
            // Die ganze Struktur in den Thread bewegen: Ohne diese Zeile fängt die
            // Closure (Edition 2021) nur das Feld `result` ein, `zustellung` stirbt
            // noch in `spawn` — und ihr Drop meldete einen Absturz, bevor die
            // Arbeit überhaupt begonnen hat.
            let mut zustellung = zustellung;
            zustellung.result = Some(work(stop));
        });
        id
    }

    /// Hält alle noch laufenden Sub-Agenten dieser Mailbox an.
    ///
    /// Gebraucht, wenn der Lauf, der sie gestartet hat, ohne sie endet (Abbruch,
    /// `max_steps`, Modellfehler): niemand liest ihr Ergebnis mehr, aber sie
    /// teilen den Workspace und schrieben sonst weiter, nachdem der Lauf schon
    /// sein Ende gemeldet hat. Ein eigener Knopf statt des Stop-Knopfs des Laufs,
    /// weil der dem Aufrufer gehört — und ohne ihn (`Agent::run`) gibt es keinen.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Läuft noch etwas, dessen Ergebnis nicht abgeholt ist?
    pub fn has_open(&self) -> bool {
        self.running > 0
    }

    /// Holt alle bereits fertigen Ergebnisse ab, ohne zu warten.
    pub fn take_finished(&mut self) -> Vec<Finished> {
        let mut fertig = Vec::new();
        while let Ok(f) = self.rx.try_recv() {
            fertig.push(f);
        }
        self.running = self.running.saturating_sub(fertig.len());
        fertig
    }

    /// Wartet auf mindestens ein Ergebnis und holt dann alle fertigen ab.
    /// `None`, sobald der Stop-Knopf gedrückt ist.
    pub fn wait_finished(&mut self, cancel: Option<&Cancel>) -> Option<Vec<Finished>> {
        loop {
            if stopped(cancel) {
                return None;
            }
            match self.rx.recv_timeout(WARTE_TAKT) {
                Ok(erstes) => {
                    self.running = self.running.saturating_sub(1);
                    let mut fertig = vec![erstes];
                    fertig.extend(self.take_finished());
                    return Some(fertig);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                // Unerreichbar, solange `self.tx` lebt — die Mailbox hält ihren
                // eigenen Sender.
                Err(RecvTimeoutError::Disconnected) => return Some(Vec::new()),
            }
        }
    }
}

/// Stellt das Ergebnis eines Hintergrund-Threads zu — auch wenn er abstürzt.
///
/// Ohne diesen Drop-Pfad bliebe ein panickender Sub-Agent für immer „laufend",
/// und ein Orchestrator, der auf ihn wartet, käme nur noch über den Stop-Knopf
/// heraus.
struct Zustellung {
    tx: Sender<Finished>,
    id: String,
    kind: String,
    result: Option<String>,
}

impl Drop for Zustellung {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| "ERROR: Sub-Agent ist ohne Ergebnis abgestürzt.".to_string());
        // Ein Fehler heißt: der Lauf, der ihn gestartet hat, ist vorbei und hat
        // seine Mailbox verworfen — dann will das Ergebnis niemand mehr.
        let _ = self.tx.send(Finished {
            id: std::mem::take(&mut self.id),
            kind: std::mem::take(&mut self.kind),
            result,
        });
    }
}

/// Die Nachricht, mit der ein Ergebnis in den Verlauf des Orchestrators kommt.
pub fn render_finished(id: &str, kind: &str, result: &str) -> String {
    format!("[Hintergrund] Sub-Agent {id} ({kind}) ist fertig. Sein Ergebnis:\n\n{result}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_erreicht_laufende_arbeit() {
        let mut bg = Background::default();
        bg.spawn("general", |stop| {
            while !stopped(Some(&stop)) {
                std::thread::sleep(Duration::from_millis(5));
            }
            "angehalten".to_string()
        });
        bg.stop();
        assert_eq!(bg.wait_finished(None).unwrap()[0].result, "angehalten");
    }

    #[test]
    fn ergebnis_kommt_ueber_die_mailbox() {
        let mut bg = Background::default();
        let id = bg.spawn("explorer", |_| "fertig".to_string());
        assert_eq!(id, "bg-1");
        assert!(bg.has_open());
        let fertig = bg.wait_finished(None).unwrap();
        assert_eq!(
            fertig,
            vec![Finished {
                id: "bg-1".into(),
                kind: "explorer".into(),
                result: "fertig".into()
            }]
        );
        assert!(!bg.has_open());
    }

    #[test]
    fn panic_im_thread_meldet_trotzdem_ein_ergebnis() {
        let mut bg = Background::default();
        bg.spawn("general", |_| panic!("absichtlich"));
        let fertig = bg.wait_finished(None).unwrap();
        assert!(fertig[0].result.contains("abgestürzt"));
        assert!(!bg.has_open());
    }

    #[test]
    fn warten_endet_beim_stop_knopf() {
        let mut bg = Background::default();
        let (_halt, gate) = channel::<()>();
        bg.spawn("general", move |_| {
            let _ = gate.recv();
            "zu spät".to_string()
        });
        let cancel = crate::agent::new_cancel();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(bg.wait_finished(Some(&cancel)).is_none());
        assert!(bg.has_open());
    }
}
