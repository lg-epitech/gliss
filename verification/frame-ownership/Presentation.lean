import Std

/- A per-decoded-frame ownership model, not a model of browser rasterization.
   Synchronous JS actions cannot be interleaved by an epoch change. A serial
   promise chain processes one ticket at a time, but old and new connection
   chains can overlap. Bounds below are per ticket, not global GPU bounds. -/
namespace Presentation

structure State where
  live : Bool := true
  closes : Nat := 0
  current : Bool := true
  painted : Bool := false
  failed : Bool := false
  acked : Bool := false
  deriving DecidableEq

def initial : State := {}
def valid (s : State) : Prop :=
  (if s.live then 1 else 0) + s.closes = 1 ∧
  (s.acked = true → s.painted = true ∧ s.live = false)

inductive Action where
  | wait | invalidate | paint | fail | close | ack
  deriving DecidableEq

-- An operational model of epoch guards, try/finally, and ACK after drawing.
-- Guards describe source branches, not axioms about native browser calls.
def step (s : State) : Action → State
  | .wait => s
  | .invalidate => { s with current := false }
  | .paint => if s.live && s.current && !s.failed && !s.painted then
      { s with painted := true } else s
  | .fail => { s with failed := true }
  | .close => if s.live then { s with live := false, closes := s.closes + 1 } else s
  | .ack => if s.painted && !s.live && s.current && !s.failed then
      { s with acked := true } else s

theorem initial_valid : valid initial := by simp [valid, initial]

theorem step_valid (s : State) (h : valid s) (a : Action) : valid (step s a) := by
  rcases h with ⟨hc, ha⟩
  cases a <;> simp only [step]
  · exact ⟨hc, ha⟩
  · exact ⟨hc, ha⟩
  · split
    · simp_all [valid]
    · exact ⟨hc, ha⟩
  · exact ⟨hc, ha⟩
  · split
    · simp_all [valid]
    · exact ⟨hc, ha⟩
  · split
    · simp_all [valid, Bool.and_eq_true]
    · exact ⟨hc, ha⟩

def run (s : State) (actions : List Action) : State := actions.foldl step s

theorem run_valid (s : State) (h : valid s) (actions : List Action) :
    valid (run s actions) := by
  induction actions generalizing s with
  | nil => exact h
  | cons a tail ih => exact ih (step s a) (step_valid s h a)

theorem all_traces_conserve (actions : List Action) :
    valid (run initial actions) := run_valid initial initial_valid actions

theorem paint_requires_current_live (s : State) (h : (step s .paint).painted ≠ s.painted) :
    s.current = true ∧ s.live = true ∧ s.failed = false := by
  simp only [step] at h
  split at h
  · simp_all [Bool.and_eq_true]
  · exact False.elim (h rfl)

theorem cancelled_never_paints (s : State) :
    (step (step s .invalidate) .paint).painted = s.painted := by simp [step]

theorem close_idempotent (s : State) :
    step (step s .close) .close = step s .close := by
  simp only [step]
  split <;> simp_all

-- try/finally terminalization: even a throw after decoding closes the resource.
theorem terminal_exactly_one_close (actions : List Action) :
    let terminal := step (run initial actions) .close
    terminal.live = false ∧ terminal.closes = 1 := by
  have h := all_traces_conserve actions
  simp only [valid] at h
  simp only [step]
  split <;> simp_all

theorem ack_requires_painted_closed (actions : List Action)
    (h : (run initial actions).acked = true) :
    (run initial actions).painted = true ∧ (run initial actions).live = false :=
  (all_traces_conserve actions).2 h

theorem ack_event_requires_current (s : State) (h : (step s .ack).acked ≠ s.acked) :
    s.current = true ∧ s.failed = false := by
  simp only [step] at h
  split at h
  · simp_all [Bool.and_eq_true]
  · exact False.elim (h rfl)

end Presentation
