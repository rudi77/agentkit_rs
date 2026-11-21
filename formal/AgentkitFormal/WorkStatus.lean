/-!
# Statusmatrix eines Work Items

Modell von `WorkItemStatus::can_transition_to` in `agentkit_work/src/model.rs`.
Die Matrix ist 1:1 abgeschrieben; die Sätze darunter sind die Zusagen aus dem
Doc-Kommentar dort, hier für alle 36 Zustandspaare bewiesen.
-/

namespace AgentkitFormal.Work

inductive Status where
  | pending
  | running
  | awaitingVerification
  | completed
  | failed
  | canceled
  deriving DecidableEq, Repr

open Status

/-- Die erlaubte Übergangsmatrix (13 Paare, alles andere ist verboten). -/
def canTransition : Status → Status → Bool
  | pending, running => true
  | pending, canceled => true
  | running, completed => true
  | running, failed => true
  | running, pending => true
  | running, canceled => true
  | running, awaitingVerification => true
  | awaitingVerification, completed => true
  | awaitingVerification, failed => true
  | awaitingVerification, pending => true
  | awaitingVerification, canceled => true
  | failed, pending => true
  | failed, canceled => true
  | _, _ => false

/-- „`Completed` ist immer endgültig." -/
theorem completed_ist_endgueltig (s : Status) : canTransition completed s = false := by
  cases s <;> rfl

/-- `Canceled` hat ebenfalls keinen Ausgang. -/
theorem canceled_ist_endgueltig (s : Status) : canTransition canceled s = false := by
  cases s <;> rfl

/-- „`AwaitingVerification` erreicht man nur aus `Running`." -/
theorem awaiting_nur_aus_running (s : Status) :
    canTransition s awaitingVerification = true → s = running := by
  cases s <;> simp [canTransition]

/-- `Completed` erreicht man nur über einen Versuch: aus `Running` oder aus der
Prüfung, nie direkt aus `Pending` oder `Failed`. -/
theorem completed_nur_nach_versuch (s : Status) :
    canTransition s completed = true → s = running ∨ s = awaitingVerification := by
  cases s <;> simp [canTransition]

/-- Kein Übergang lässt den Status stehen — jedes Ereignis ändert ihn wirklich. -/
theorem keine_selbstschleife (s : Status) : canTransition s s = false := by
  cases s <;> rfl

/-- Jeder nicht endgültige Status lässt sich abbrechen: ein `RunCanceled` findet
nie ein Item vor, das es nicht nach `Canceled` bringen dürfte. -/
theorem abbruch_immer_moeglich (s : Status) :
    s ≠ completed → s ≠ canceled → canTransition s canceled = true := by
  cases s <;> simp [canTransition]

end AgentkitFormal.Work
