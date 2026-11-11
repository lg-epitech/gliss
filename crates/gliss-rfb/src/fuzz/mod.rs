//! Fuzz tests of everything a VNC server can make this crate read.
//!
//! They live inside the crate because the entry points are private, and they
//! run with the other tests. `docs/development.md` says how to run a campaign.

mod model;
mod stream;
#[allow(dead_code)]
#[path = "../../../../test-support/rust/fuzz.rs"]
mod support;

/// Lets a fuzzer bound the largest allocation an input can cause.
#[global_allocator]
static ALLOCATOR: support::MeasuringAllocator = support::MeasuringAllocator;
