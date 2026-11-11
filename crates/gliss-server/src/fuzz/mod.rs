//! Fuzz tests of what a browser and a guest can make this server keep.
//!
//! Each state machine is taken through arbitrary sequences of its operations
//! next to a model that is simple enough to be read as its specification.
//! The fuzzers of a module's private state are that module's children and
//! live in this directory. `docs/development.md` says how to run a campaign.

mod cookie;
#[allow(dead_code)]
#[path = "../../../../test-support/rust/fuzz.rs"]
pub mod support;
