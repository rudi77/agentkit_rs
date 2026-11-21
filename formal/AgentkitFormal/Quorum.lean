/-!
# Erreichbarkeit des Schwarm-Quorums

Modell von `quorum` und `peers_of` in `agentkit_swarm/src/dynamic.rs`.

Die Zusage aus dem Doc-Kommentar dort: das Quorum ist nie größer als die Zahl
der Stimmen, die JEDES Mitglied einsammeln kann — sonst läuft der Schwarm stumm
in den Leerlauf-Abbruch. Abstimmen können genau die Nachbarn (`peers`).

Agent-IDs sind hier natürliche Zahlen; die Aussagen hängen nur an der
Gleichheit von IDs, nicht an ihrer Gestalt.
-/

namespace AgentkitFormal.Swarm

abbrev Kante := Nat × Nat

/-- Berührt die Kante den Agenten? -/
def beruehrt (id : Nat) (e : Kante) : Bool := e.1 == id || e.2 == id

/-- Knotengrad über die ROHE Kantenliste — so zählte `quorum` ursprünglich. -/
def grad (edges : List Kante) (id : Nat) : Nat := (edges.filter (beruehrt id)).length

/-- Der Nachbar am anderen Ende, falls die Kante den Agenten genau einmal berührt. -/
def gegenueber (id : Nat) (e : Kante) : Option Nat :=
  if e.1 = id then (if e.2 = id then none else some e.2)
  else (if e.2 = id then some e.1 else none)

/-- Entfernt Doppelte (entspricht `sort` + `dedup` in `peers_of`, die Reihenfolge
spielt für die Anzahl keine Rolle). -/
def dedup : List Nat → List Nat
  | [] => []
  | x :: xs => if x ∈ xs then dedup xs else x :: dedup xs

/-- `peers_of`: die tatsächlichen Nachbarn, also die möglichen Stimmen. -/
def peers (edges : List Kante) (id : Nat) : List Nat :=
  dedup (edges.filterMap (gegenueber id))

/-- Kleinster Wert von `f` über die Mitglieder (`None` beim leeren Schwarm). -/
def minUeber (f : Nat → Nat) : List Nat → Option Nat
  | [] => none
  | x :: xs =>
    match minUeber f xs with
    | none => some (f x)
    | some m => some (min (f x) m)

/-- Das Quorum zu einer gegebenen Zählweise der einsammelbaren Stimmen. -/
def quorumMit (stimmen : Nat → Nat) (gewuenscht : Option Nat) (ids : List Nat) : Nat :=
  let erreichbar := (minUeber stimmen ids).getD 0
  min (gewuenscht.getD ((erreichbar + 1) / 2)) erreichbar

/-- Ursprüngliche Fassung: zählt Kanten. -/
def quorumAlt (gewuenscht : Option Nat) (ids : List Nat) (edges : List Kante) : Nat :=
  quorumMit (grad edges) gewuenscht ids

/-- Korrigierte Fassung: zählt Nachbarn. -/
def quorum (gewuenscht : Option Nat) (ids : List Nat) (edges : List Kante) : Nat :=
  quorumMit (fun id => (peers edges id).length) gewuenscht ids

/-! ## Das Gegenbeispiel

Zwei Agenten, dieselbe Verbindung in beiden Richtungen angegeben, Einstimmigkeit
verlangt: die alte Fassung fordert 2 Stimmen, einsammelbar ist genau eine. -/

example : quorumAlt (some 2) [0, 1] [(0, 1), (1, 0)] = 2 := by decide
example : (peers [(0, 1), (1, 0)] 0).length = 1 := by decide
example : quorum (some 2) [0, 1] [(0, 1), (1, 0)] = 1 := by decide

/-! ## Die Zusage, für die korrigierte Fassung bewiesen -/

theorem minUeber_some_le (f : Nat → Nat) (ids : List Nat) (id : Nat) (h : id ∈ ids) :
    ∃ m, minUeber f ids = some m ∧ m ≤ f id := by
  induction ids with
  | nil => cases h
  | cons x xs ih =>
    simp only [minUeber]
    rcases List.mem_cons.mp h with rfl | hxs
    · cases minUeber f xs with
      | none => exact ⟨_, rfl, Nat.le_refl _⟩
      | some m => exact ⟨_, rfl, Nat.min_le_left _ _⟩
    · obtain ⟨m, hm, hle⟩ := ih hxs
      rw [hm]
      exact ⟨_, rfl, Nat.le_trans (Nat.min_le_right _ _) hle⟩

theorem minUeber_le (f : Nat → Nat) (ids : List Nat) (id : Nat) (h : id ∈ ids) :
    (minUeber f ids).getD 0 ≤ f id := by
  obtain ⟨m, hm, hle⟩ := minUeber_some_le f ids id h
  simp [hm, hle]

/-- Das Quorum ist für jede Zählweise durch deren Wert bei jedem Mitglied beschränkt. -/
theorem quorumMit_le (stimmen : Nat → Nat) (gewuenscht : Option Nat) (ids : List Nat)
    (id : Nat) (h : id ∈ ids) : quorumMit stimmen gewuenscht ids ≤ stimmen id := by
  have := minUeber_le stimmen ids id h
  simp only [quorumMit]
  omega

/-- **Erreichbarkeit.** Für beliebige Kantenlisten — auch mit Doppelten, auch mit
Selbstkanten — verlangt das Quorum von keinem Mitglied mehr Stimmen, als es
Nachbarn hat. -/
theorem quorum_erreichbar (gewuenscht : Option Nat) (ids : List Nat) (edges : List Kante)
    (id : Nat) (h : id ∈ ids) : quorum gewuenscht ids edges ≤ (peers edges id).length :=
  quorumMit_le _ gewuenscht ids id h

/-- Ein ausdrücklich gewünschtes Quorum wird nie überschritten. -/
theorem quorum_le_gewuenscht (g : Nat) (ids : List Nat) (edges : List Kante) :
    quorum (some g) ids edges ≤ g := by
  simp only [quorum, quorumMit, Option.getD_some]
  omega

/-! ## Die Korrektur ändert nichts, wo die alte Fassung stimmte

Auf einer sauberen Kantenliste (keine Selbstkante, keine Verbindung doppelt —
das liefern die Presets `mesh`/`kette`/`stern`) sind Kantenzahl und
Nachbarzahl gleich, beide Fassungen also auch. -/

theorem dedup_length_le (l : List Nat) : (dedup l).length ≤ l.length := by
  induction l with
  | nil => simp [dedup]
  | cons x xs ih =>
    simp only [dedup]
    split <;> simp <;> omega

theorem dedup_of_nodup (l : List Nat) (h : l.Nodup) : dedup l = l := by
  induction l with
  | nil => rfl
  | cons x xs ih =>
    have ⟨hx, hxs⟩ := List.nodup_cons.mp h
    simp [dedup, hx, ih hxs]

/-- Sauber für einen Agenten: keine Selbstkante an ihm, kein Nachbar doppelt. -/
def Sauber (edges : List Kante) (id : Nat) : Prop :=
  (∀ e ∈ edges, ¬(e.1 = id ∧ e.2 = id)) ∧ (edges.filterMap (gegenueber id)).Nodup

theorem gegenueber_beruehrt (id : Nat) (e : Kante) (x : Nat)
    (h : gegenueber id e = some x) : beruehrt id e = true := by
  by_cases h1 : e.1 = id <;> by_cases h2 : e.2 = id <;> simp_all [gegenueber, beruehrt]

theorem beruehrt_false_of_none (id : Nat) (e : Kante) (hs : ¬(e.1 = id ∧ e.2 = id))
    (h : gegenueber id e = none) : beruehrt id e = false := by
  by_cases h1 : e.1 = id <;> by_cases h2 : e.2 = id <;> simp_all [gegenueber, beruehrt]

theorem filterMap_length_eq_filter (edges : List Kante) (id : Nat)
    (h : ∀ e ∈ edges, ¬(e.1 = id ∧ e.2 = id)) :
    (edges.filterMap (gegenueber id)).length = (edges.filter (beruehrt id)).length := by
  induction edges with
  | nil => rfl
  | cons e es ih =>
    have ih' := ih (fun e' he' => h e' (List.mem_cons_of_mem _ he'))
    cases hg : gegenueber id e with
    | none =>
      have hb := beruehrt_false_of_none id e (h e List.mem_cons_self) hg
      rw [List.filterMap_cons_none hg, List.filter_cons_of_neg (by simp [hb])]
      exact ih'
    | some x =>
      rw [List.filterMap_cons_some hg, List.filter_cons_of_pos (gegenueber_beruehrt id e x hg)]
      simp [ih']

theorem grad_eq_peers (edges : List Kante) (id : Nat) (h : Sauber edges id) :
    grad edges id = (peers edges id).length := by
  simp only [grad, peers, dedup_of_nodup _ h.2, filterMap_length_eq_filter edges id h.1]

/-- Auf sauberen Kanten liefern alte und korrigierte Fassung dasselbe Quorum. -/
theorem quorum_eq_quorumAlt (gewuenscht : Option Nat) (ids : List Nat) (edges : List Kante)
    (h : ∀ id ∈ ids, Sauber edges id) :
    quorum gewuenscht ids edges = quorumAlt gewuenscht ids edges := by
  have hmin : ∀ l : List Nat, (∀ id ∈ l, Sauber edges id) →
      minUeber (fun id => (peers edges id).length) l = minUeber (grad edges) l := by
    intro l
    induction l with
    | nil => intro _; rfl
    | cons x xs ih =>
      intro hl
      simp only [minUeber, ih (fun id hid => hl id (List.mem_cons_of_mem _ hid)),
        grad_eq_peers edges x (hl x List.mem_cons_self)]
  simp only [quorum, quorumAlt, quorumMit, hmin ids h]

theorem filterMap_length_le_filter (edges : List Kante) (id : Nat) :
    (edges.filterMap (gegenueber id)).length ≤ (edges.filter (beruehrt id)).length := by
  induction edges with
  | nil => simp
  | cons e es ih =>
    cases hg : gegenueber id e with
    | none =>
      rw [List.filterMap_cons_none hg, List.filter_cons]
      split
      · simp; omega
      · exact ih
    | some x =>
      rw [List.filterMap_cons_some hg, List.filter_cons_of_pos (gegenueber_beruehrt id e x hg)]
      simp
      omega

/-- Ohne Sauberkeit gilt nur die eine Richtung: Doppelte blähen den Grad auf,
nie die Nachbarzahl. Die Korrektur kann das Quorum also nur senken. -/
theorem peers_length_le_grad (edges : List Kante) (id : Nat) :
    (peers edges id).length ≤ grad edges id := by
  have h1 := dedup_length_le (edges.filterMap (gegenueber id))
  have h2 := filterMap_length_le_filter edges id
  simp only [peers, grad]
  omega

end AgentkitFormal.Swarm
