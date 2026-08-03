//! Tool / connector gateway.
//!
//! The policy-guarded front door for side-effecting tool calls (often via MCP).
//! See [`gateway::ToolGateway`].

pub mod gateway;
pub mod registry;
