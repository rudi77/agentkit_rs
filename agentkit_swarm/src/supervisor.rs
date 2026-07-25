//! Supervisor — rein technischer Lebenszyklus, KEIN Orchestrator: er prüft
//! nur, ob Actor-Threads noch leben. MVP-Policy: stirbt ein Actor (Panic),
//! wird der gesamte Schwarm kontrolliert gestoppt — das ist einfacher als die
//! Wiederherstellung einer teilweise fortgeschrittenen Agent-Memory.
//!
//! Erkennung über Polling (`JoinHandle::is_finished`), nicht über Events: ein
//! panickender Thread publiziert nichts mehr, und der Monitor hängt sonst am
//! Event-Empfänger. Die Panic-Details liefert später `join()` (Unwinding —
//! deshalb setzt dieses Crate kein `panic = "abort"`).

use crate::message::AgentId;
use std::any::Any;
use std::thread::JoinHandle;

/// Erster Actor, dessen Thread außerhalb eines Shutdowns beendet ist —
/// im laufenden Betrieb kann das nur ein Panic sein (die Laufzeit hält alle
/// Sender, ein regulärer Loop-Exit passiert erst nach dem Stop-Signal).
pub(crate) fn find_failed_actor(handles: &[(AgentId, JoinHandle<()>)]) -> Option<AgentId> {
    handles
        .iter()
        .find(|(_, handle)| handle.is_finished())
        .map(|(id, _)| id.clone())
}

/// Panic-Payload eines gejointen Threads in lesbaren Text übersetzen.
pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Panic ohne Textnachricht".to_string()
    }
}
