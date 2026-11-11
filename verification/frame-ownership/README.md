# Decoded frame ownership

Run `bash verification/frame-ownership/check.sh` with Lean 4.35.0 installed.
`LEAN_BINARY` can name an explicit Lean executable. The project uses only Std.
Run the source regression tests with `node --test tests/frame-ownership.test.mjs`.

The model tracks one decoded frame through waiting, invalidation, painting,
failure, close and ACK actions. It proves ownership conservation, terminal
release, and that ACKs require painted, closed frames. Cancellation releases the
frame after the waiting promise resumes, without requiring an animation frame.
A late callback cannot clear a fresh connection's cancellation function.

The source bridge is `web/client.js`: a local owned-frame reference is released
in `finally`; disconnect resolves the presentation wait; callback identity
protects the current cancellation slot. Invalidation and cancellation
precede cleanup callbacks, and audio notifications follow core transport cleanup.
Regression tests execute the real client
with controlled scheduling and errors, checking close, paint and ACK outcomes.

These are mathematical models with a reviewed and tested source bridge. They do
not verify JavaScript compilation, browser resource destruction or rasterization.
They assume ordinary JS promise/function-identity semantics, distinct active
epochs, and the specified browser API contracts. Native close call counts are
tested separately from the model's idempotent logical close action. Cancellation
progress requires the resumed promise microtask. Bounds are per frame; this is
neither a global GPU-memory bound nor a CPU-speed claim.
