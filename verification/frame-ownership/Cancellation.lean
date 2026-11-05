import Presentation
namespace Presentation

def cancelOwned (s : State) := step (step s .invalidate) .close

theorem cancel_owned_terminal (s : State) (h : valid s) :
    (cancelOwned s).current = false ∧
    (cancelOwned s).live = false ∧ (cancelOwned s).closes = 1 := by
  rcases h with ⟨hc, ha⟩
  cases live : s.live <;> simp_all [cancelOwned, step]

-- This models abort + the resumed promise microtask. It requires no RAF action.
theorem cancelled_trace_terminal (actions : List Action) :
    let s := cancelOwned (run initial actions)
    s.current = false ∧ s.live = false ∧ s.closes = 1 :=
  cancel_owned_terminal _ (all_traces_conserve actions)

theorem invalidate_alone_retains :
    (step initial .invalidate).live = true := rfl

-- Labels represent fresh JS function identities, not wrapping native RAF IDs.
def clearIfOwned (pending : Option Nat) (callback : Nat) : Option Nat :=
  if pending = some callback then none else pending

theorem late_callback_preserves_new (old fresh : Nat) (h : old ≠ fresh) :
    clearIfOwned (some fresh) old = some fresh := by
  simp [clearIfOwned, Ne.symm h]

theorem finish_idempotent (pending : Option Nat) (callback : Nat) :
    clearIfOwned (clearIfOwned pending callback) callback = clearIfOwned pending callback := by
  by_cases h : pending = some callback <;> simp [clearIfOwned, h]
end Presentation
