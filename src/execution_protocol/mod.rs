//! Execution Protocol v1 side-by-side implementation.
//!
//! This module is deliberately side-effect free and is not wired into hosted
//! production routing yet. It defines the versioned aggregate, typed events,
//! reducer and invariants, plus deterministic profiling, discovery, planning,
//! target-context preparation, and verified mutation lifecycles exercised by
//! checked-in protocol fixtures.

mod discovery;
mod error;
mod event;
mod identity;
mod implementation;
mod model;
mod mutation;
mod planning;
mod profile;
mod publication;
mod reducer;
mod review;
mod store;
mod validation;
mod validation_process;

pub(crate) use discovery::*;
pub(crate) use error::*;
pub(crate) use event::*;
pub(crate) use identity::*;
pub(crate) use implementation::*;
pub(crate) use model::*;
pub(crate) use mutation::*;
pub(crate) use planning::*;
pub(crate) use profile::*;
pub(crate) use publication::*;
#[allow(unused_imports)]
pub(crate) use reducer::{
    build_prepared_discovery_action, build_prepared_planning_action, decide, reduce, validate_state,
};
pub(crate) use review::*;
#[allow(unused_imports)]
pub(crate) use store::InMemoryEventStore;
pub(crate) use validation::*;
#[allow(unused_imports)]
pub(crate) use validation_process::*;

#[cfg(test)]
mod tests;
