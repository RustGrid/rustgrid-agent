//! Canonical, serializable state for deterministic hosted execution.
//!
//! This module deliberately contains no repository, model, command, persistence,
//! or publication I/O.  It is the domain boundary shared by the hosted
//! orchestrator, its adapters, and the deterministic replay harness.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use crate::lifecycle::HostedExecutionStage;

pub const EXECUTION_GRAPH_SCHEMA_VERSION: u16 = 1;

// Keep the serialized domain schema in one public namespace while the
// implementation is organized by responsibility. This preserves every
// existing type path (`execution_graph::Type`) and its serde representation.
include!("serialization.rs");
include!("model.rs");
include!("node.rs");
include!("error.rs");
include!("graph.rs");
include!("invariant.rs");
include!("construction.rs");
include!("validation.rs");
include!("recovery.rs");
include!("budget.rs");
include!("publication.rs");
include!("transition.rs");
include!("snapshot.rs");

#[cfg(test)]
mod tests;
