//! In-code workflow catalog: maps a `workflow_id` to an ordered step plan.
//!
//! Built-in workflows are defined in code; DB-backed, versioned workflow
//! definitions are a deliberate follow-up. Step kinds: `Transform` (placeholder
//! compute), `HumanApproval` (suspend/resume gate), `Fail` (fault injection), and
//! `Tool` (a governed tool-gateway call — PR2).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::domain::ApprovalMode;

/// What a single step does when the executor reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Placeholder compute — produces an output and completes.
    Transform,
    /// Human-in-the-loop gate — suspends the run `waiting` on a checkpoint.
    HumanApproval,
    /// Fault injection — always fails (exercises the failed-terminal path).
    Fail,
    /// A governed tool-gateway call. Auto-approved tools complete the step inline;
    /// an approval-required tool suspends the run `waiting` (like `HumanApproval`).
    /// The invocation config rides in [`StepDef::tool`].
    Tool,
    /// A timer: defer the run for `wait_secs` (armed on `run_steps.next_attempt_at`), then the
    /// background worker completes it once the delay elapses (a time-based, non-human gate).
    Wait,
}

impl StepKind {
    /// The `run_steps.kind` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Transform => "transform",
            StepKind::HumanApproval => "human_approval",
            StepKind::Fail => "fail",
            StepKind::Tool => "tool",
            StepKind::Wait => "wait",
        }
    }
}

/// A Tool step's invocation config — materialized onto the `run_steps.config`
/// jsonb at start and read back by the executor (so it never re-resolves the
/// catalog). Round-trips via serde.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStep {
    /// The tool/connector id to invoke through the gateway.
    pub tool_id: String,
    /// Arguments passed to the tool (opaque JSON).
    #[serde(default)]
    pub arguments: Value,
    /// The client-declared approval mode. `Required` forces the human-in-the-loop
    /// suspend; the tenant's skill policy can still force it server-side either way.
    pub approval_mode: ApprovalMode,
}

/// One step in a workflow plan.
#[derive(Debug, Clone)]
pub struct StepDef {
    /// Human-readable step name (recorded on the `run_steps` row + events).
    pub name: String,
    /// What the step does.
    pub kind: StepKind,
    /// Attempts before the step is terminal-failed. `> 1` enables retry/backoff (0017): a
    /// retriable failure reschedules the step until `attempts` reaches `max_attempts`.
    pub max_attempts: i32,
    /// Present iff `kind == Tool`: the gateway call to make.
    pub tool: Option<ToolStep>,
    /// Test-only flaky behavior for a `Transform` step: fail (retriably) on every attempt
    /// before this attempt number, then succeed. `Some(3)` fails attempts 1–2, succeeds on 3.
    /// Idempotent (placeholder compute, no external side effect) so retrying is SAFE — the
    /// vehicle that exercises retry/backoff without the non-idempotent-tool hazard.
    pub flaky_fail_until: Option<i32>,
    /// Present iff `kind == Wait`: the RELATIVE timer delay in seconds (materialized to
    /// `run_steps.config.wait_secs`; the executor arms `next_attempt_at = now() + wait_secs`).
    pub wait_secs: Option<i64>,
    /// Present iff `kind == Wait`: when true, this is an ABSOLUTE-time timer whose wake instant is
    /// read from the run input's `wait_until` (RFC3339 UTC) at materialization and stored as
    /// `run_steps.config.wait_until`; the executor arms `next_attempt_at` to that exact instant (a
    /// fixed wall-clock deadline, not an offset). Mutually exclusive with `wait_secs`.
    pub wait_until_from_input: bool,
}

impl Default for StepDef {
    /// A single-attempt `Transform` with no tool / flaky / timer config — the base every
    /// constructor overrides. Centralizing the defaults keeps the constructors (and any future
    /// field) uniform.
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: StepKind::Transform,
            max_attempts: 1,
            tool: None,
            flaky_fail_until: None,
            wait_secs: None,
            wait_until_from_input: false,
        }
    }
}

impl StepDef {
    fn transform(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
    fn approval(name: &str) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::HumanApproval,
            ..Self::default()
        }
    }
    fn fail(name: &str) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Fail,
            ..Self::default()
        }
    }
    fn tool(name: &str, tool_id: &str, arguments: Value, approval_mode: ApprovalMode) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Tool,
            tool: Some(ToolStep {
                tool_id: tool_id.into(),
                arguments,
                approval_mode,
            }),
            ..Self::default()
        }
    }
    /// A retriable tool step: `max_attempts > 1` lets a failed tool call be RESCHEDULED and
    /// re-driven by the worker (re-sending the same deterministic idempotency key, so a compliant
    /// MCP server dedups). Safe only under the two-sided MCP idempotency contract.
    fn tool_retriable(
        name: &str,
        tool_id: &str,
        arguments: Value,
        approval_mode: ApprovalMode,
        max_attempts: i32,
    ) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Tool,
            max_attempts,
            tool: Some(ToolStep {
                tool_id: tool_id.into(),
                arguments,
                approval_mode,
            }),
            ..Self::default()
        }
    }
    /// A retriable (idempotent) `Transform` that fails until attempt `fail_until` then succeeds,
    /// bounded by `max_attempts`. Exercises retry/backoff end-to-end.
    fn flaky(name: &str, fail_until: i32, max_attempts: i32) -> Self {
        Self {
            name: name.into(),
            max_attempts,
            flaky_fail_until: Some(fail_until),
            ..Self::default()
        }
    }
    /// A RELATIVE timer step: defer the run `wait_secs` seconds (the worker completes it once
    /// elapsed).
    fn wait(name: &str, wait_secs: i64) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Wait,
            wait_secs: Some(wait_secs),
            ..Self::default()
        }
    }
    /// An ABSOLUTE-time timer step: its wake instant is taken from the run input's `wait_until`
    /// (RFC3339 UTC) at materialization; the executor arms `next_attempt_at` to that fixed instant
    /// and the worker fires it once it passes.
    fn wait_until_input(name: &str) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Wait,
            wait_until_from_input: true,
            ..Self::default()
        }
    }
}

/// An ordered step plan.
#[derive(Debug, Clone)]
pub struct WorkflowDef {
    /// Steps executed in order.
    pub steps: Vec<StepDef>,
}

/// Resolve a `workflow_id` into a step plan.
///
/// A handful of built-in workflows are defined in code. An UNKNOWN `workflow_id`
/// falls back to a SINGLE step — a `HumanApproval` when the caller set
/// `callbacks.human_approval`, else a `Transform` — which preserves the
/// pre-executor behavior for every existing `run_type`
/// (`start->completed`, or `start->waiting->resume->completed`).
pub fn resolve(workflow_id: &str, human_approval: bool) -> WorkflowDef {
    let steps = match workflow_id {
        "pipeline.three" => vec![
            StepDef::transform("extract"),
            StepDef::transform("transform"),
            StepDef::transform("load"),
        ],
        "review.gated" => vec![
            StepDef::transform("prepare"),
            StepDef::approval("review"),
            StepDef::transform("finalize"),
        ],
        "review.double-gated" => vec![
            StepDef::approval("first-gate"),
            StepDef::approval("second-gate"),
        ],
        "pipeline.faulty" => vec![
            StepDef::transform("ok"),
            StepDef::fail("boom"),
            StepDef::transform("unreached"),
        ],
        // A flaky step that fails ONCE (attempt 1) then succeeds on the retry (attempt 2).
        // Exercises retry/backoff → eventual success (needs the background worker to drive
        // the due retry).
        "retry.recovers" => vec![StepDef::flaky("flaky", 2, 3)],
        // A flaky step that keeps failing until it EXHAUSTS max_attempts (2) → terminal fail.
        "retry.exhausts" => vec![StepDef::flaky("doomed", 99, 2)],
        // A timer: a step, then a 1h delay, then a step — the run defers on the wait until the
        // worker fires it (start -> running; worker completes it once the wake time elapses).
        "wait.then" => vec![
            StepDef::transform("before"),
            StepDef::wait("pause", 3600),
            StepDef::transform("after"),
        ],
        // An ABSOLUTE-time timer: a step, then a pause until the wall-clock instant supplied as the
        // run input's `wait_until` (RFC3339 UTC), then a step. The run defers on the wait until the
        // worker fires it once that instant passes (start -> running; a `wait_until` already in the
        // past completes inline). Requires `input.wait_until`.
        "wait.until" => vec![
            StepDef::transform("before"),
            StepDef::wait_until_input("pause"),
            StepDef::transform("after"),
        ],
        // A single auto-approved tool call: executes inline, run -> completed.
        "tool.auto" => vec![StepDef::tool(
            "call-echo",
            "echo",
            json!({ "msg": "hello" }),
            ApprovalMode::None,
        )],
        // A RETRIABLE auto tool (max_attempts = 3): a failed connector call is rescheduled and the
        // worker re-drives it (re-sending the same deterministic Idempotency-Key). Exercises
        // tool-step retry under the two-sided MCP idempotency contract.
        "tool.auto.retries" => vec![StepDef::tool_retriable(
            "call-flaky",
            "flaky-echo",
            json!({ "msg": "hello" }),
            ApprovalMode::None,
            3,
        )],
        // A tool call that requires approval, inside a pipeline: prepare -> gated
        // tool (suspends `waiting`) -> resume approves+finalizes -> notify.
        "tool.gated" => vec![
            StepDef::transform("prepare"),
            StepDef::tool("deploy", "deploy.prod", json!({}), ApprovalMode::Required),
            StepDef::transform("notify"),
        ],
        _ => vec![if human_approval {
            StepDef::approval("approval")
        } else {
            StepDef::transform("run")
        }],
    };
    WorkflowDef { steps }
}

/// Deterministic idempotency key for a step, unique within a run.
pub fn idempotency_key(run_id: Uuid, step_index: i32) -> String {
    format!("{run_id}:{step_index:04}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_workflow_falls_back_to_one_step_by_approval_flag() {
        let auto = resolve("mystery", false);
        assert_eq!(auto.steps.len(), 1);
        assert_eq!(auto.steps[0].kind, StepKind::Transform);

        let gated = resolve("mystery", true);
        assert_eq!(gated.steps.len(), 1);
        assert_eq!(gated.steps[0].kind, StepKind::HumanApproval);
    }

    #[test]
    fn known_workflows_resolve_to_their_plans() {
        assert_eq!(resolve("pipeline.three", false).steps.len(), 3);
        let gated = resolve("review.gated", false);
        assert_eq!(gated.steps.len(), 3);
        assert_eq!(
            gated
                .steps
                .iter()
                .filter(|s| s.kind == StepKind::HumanApproval)
                .count(),
            1
        );
        assert_eq!(resolve("review.double-gated", false).steps.len(), 2);
        assert!(resolve("pipeline.faulty", false)
            .steps
            .iter()
            .any(|s| s.kind == StepKind::Fail));
    }

    #[test]
    fn tool_workflows_carry_tool_config() {
        let auto = resolve("tool.auto", false);
        assert_eq!(auto.steps.len(), 1);
        assert_eq!(auto.steps[0].kind, StepKind::Tool);
        let t = auto.steps[0].tool.as_ref().expect("tool config present");
        assert_eq!(t.tool_id, "echo");
        assert_eq!(t.approval_mode, ApprovalMode::None);

        let gated = resolve("tool.gated", false);
        let tool_step = gated
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Tool)
            .expect("a tool step");
        assert_eq!(
            tool_step.tool.as_ref().unwrap().approval_mode,
            ApprovalMode::Required
        );
        // Non-tool steps carry no tool config.
        assert!(gated.steps[0].tool.is_none());
    }

    #[test]
    fn wait_workflows_carry_the_right_timer_config() {
        // Relative timer: `wait.then` carries `wait_secs`, not the absolute-from-input marker.
        let relative = resolve("wait.then", false);
        let w = relative
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Wait)
            .expect("a wait step");
        assert_eq!(w.wait_secs, Some(3600));
        assert!(!w.wait_until_from_input);

        // Absolute timer: `wait.until` carries the from-input marker, not `wait_secs`.
        let absolute = resolve("wait.until", false);
        assert_eq!(absolute.steps.len(), 3);
        let w = absolute
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Wait)
            .expect("a wait step");
        assert!(
            w.wait_until_from_input,
            "wait.until sources its wake instant from input"
        );
        assert_eq!(w.wait_secs, None, "an absolute timer has no relative delay");
    }

    #[test]
    fn retry_workflows_carry_flaky_config_and_multi_attempt() {
        let recovers = resolve("retry.recovers", false);
        assert_eq!(recovers.steps.len(), 1);
        assert_eq!(recovers.steps[0].kind, StepKind::Transform);
        assert_eq!(recovers.steps[0].flaky_fail_until, Some(2));
        assert_eq!(recovers.steps[0].max_attempts, 3);

        let exhausts = resolve("retry.exhausts", false);
        assert_eq!(exhausts.steps[0].flaky_fail_until, Some(99));
        assert_eq!(exhausts.steps[0].max_attempts, 2);

        // Ordinary transforms are not flaky.
        assert_eq!(
            resolve("pipeline.three", false).steps[0].flaky_fail_until,
            None
        );
    }

    #[test]
    fn wait_workflow_carries_a_timer_step() {
        let plan = resolve("wait.then", false);
        assert_eq!(plan.steps.len(), 3);
        let timer = plan
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Wait)
            .expect("a wait step");
        assert_eq!(timer.wait_secs, Some(3600));
        assert_eq!(timer.kind.as_str(), "wait");
        // Ordinary steps carry no timer.
        assert_eq!(plan.steps[0].wait_secs, None);
    }

    #[test]
    fn idempotency_key_is_deterministic_and_per_step_unique() {
        let run = Uuid::nil();
        assert_eq!(idempotency_key(run, 0), idempotency_key(run, 0));
        assert_ne!(idempotency_key(run, 0), idempotency_key(run, 1));
    }
}
