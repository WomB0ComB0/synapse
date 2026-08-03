//! Durable workflow orchestration.
//!
//! A run is materialized into ordered [`workflow`] steps at start and driven by
//! the [`executor`]; it can suspend on a human-approval gate and resume into the
//! next step via a single-use token. See [`runs::Orchestrator`].

pub mod executor;
pub mod runs;
pub mod worker;
pub mod workflow;
