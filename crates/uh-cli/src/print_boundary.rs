//! The print runtime's turn-boundary compaction checks: the TS
//! `_checkCompaction` flow at the settled-turn boundary (`agent_end`) and
//! its pre-turn companion (`_runPreTurnCompaction`, which TS runs before
//! every admitted prompt).
//!
//! The overflow arm (Case 1) is the compact-and-retry recovery for a request
//! that exceeds the context window: a settled turn that errors with a
//! provider context-overflow drops the error turn from the loop context,
//! runs one compaction, and re-issues the turn on the compacted context
//! without re-adding the user message (TS `agent.continue()`). One attempt
//! per overflow; a retry that still overflows ends the turn with the TS
//! failure surface — the durable `compaction_outcome` row plus the
//! `compaction_end` event carrying the TS failure text. The arm also runs
//! before the next admitted prompt, so a stale overflow error left by a
//! previous run gets its recovery attempt on the resumed context.
//!
//! The pre-turn check is the full TS `_checkCompaction` call, not just the
//! overflow arm: when Case 1 stays silent the threshold arm runs too — a
//! session resumed above the reserve headroom compacts before its first
//! admitted prompt. A pre-turn compaction never re-issues: the admitted
//! prompt continues the loop on the compacted context.
//!
//! Every turn the print loop issues crosses the boundary pair, not just
//! the CLI prompts: the autonomous continuation loop admits its follow-up
//! turns through [`TurnBoundary::admit_continuation`] (TS: the session
//! admits an owed continuation through its own turn loop, so the arms
//! fire on continuation turns exactly like on prompt turns — #229's
//! print flag was reconciled here).
//!
//! The settled boundary first re-issues a turn that failed with a
//! retryable provider error ([`PrintRetry`], TS `_handleRetryableError`
//! runs before `_checkCompaction` at `agent_end`); json mode streams its
//! `auto_retry_start`/`auto_retry_end` events.
//!
//! Output surfaces: json mode streams the TS session events (the
//! `compaction_start`/`compaction_end` pair and the outcome row's message
//! pair) on stdout; text mode stays quiet here — the durable rows surface
//! through the headless terminal result (stderr plus the exit code).

use serde_json::{json, Value};
use uh_core::session_engine::compact_session::{CompactOutcome, CompactRun};
use uh_core::session_engine::engine::SessionEngine;
use uh_core::session_engine::messages::{CompactionOutcomeKind, CompactionOutcomeReason};
use uh_core::session_engine::provider_adapter::json_round_trip;
use uh_core::session_engine::provider_retry::is_context_overflow_failure;
use uh_core::session_engine::turn_retry::{auto_retry_event_json, TurnRetryHost};
use uh_core::session_engine::TrailingAssistantFilter;
use uh_types::ai::Model;
use uh_types::session::AgentMessage as SessionAgentMessage;

// The boundary renderers (the TS wire-event surface: the
// `compaction_start`/`compaction_end` event builders and the json-mode
// emission methods) moved to the child module; the arms child reaches
// the builders through the facade glob (the `use` below) and the methods
// through the type at pub(super).
mod events;

use events::{compaction_end_success_event, compaction_start_event};

// The divider logic (the TS `_checkCompaction` arms: the overflow
// compact-and-retry machine with its three state enums, and the
// requested/threshold compaction arms) moved to the child module; the
// facade drivers reach the machine through pub(super) + the `use` below
// (the tests glob re-exposes the failure-text const through cfg(test)).
mod compaction_arms;

use compaction_arms::{OverflowBoundary, OverflowOutcome, OverflowRecovery};

#[cfg(test)]
use compaction_arms::OVERFLOW_RECOVERY_FAILED_MESSAGE;

/// Where the boundary's json events go: stdout in the product, a captured
/// buffer in tests.
type EventSink = std::sync::Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// What the settled boundary's provider retry needs from the run.
pub(crate) struct PrintRetry {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) agent_dir: std::path::PathBuf,
    pub(crate) provider_target: crate::print_runtime::ProviderTargetSlot,
    /// `false` under the faux script (scripted failures never fail over).
    pub(crate) failover: bool,
    /// The longest quota park the run can wait out (its wake limit).
    pub(crate) max_park: std::time::Duration,
}

/// The print loop's turn-boundary state: the one-attempt overflow machine
/// plus the json/text output mode the surfaces depend on.
pub(crate) struct TurnBoundary {
    recovery: OverflowRecovery,
    /// json mode streams the TS session events on stdout; text mode reads
    /// the durable rows through the headless terminal result.
    json_mode: bool,
    sink: EventSink,
    /// The provider retry of a failed turn (`None`: the turn settles as is).
    retry: Option<PrintRetry>,
    /// Set when the user stops the turn (the interactive chat's Ctrl+C):
    /// the provider retry gives up instead of re-issuing the turn.
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl TurnBoundary {
    pub(crate) fn new(json_mode: bool) -> Self {
        Self {
            recovery: OverflowRecovery::Idle,
            json_mode,
            sink: std::sync::Arc::new(|event| println!("{event}")),
            retry: None,
            cancel: None,
        }
    }

    /// Stop the provider retry once `cancel` is set (the interactive chat
    /// sets it on Ctrl+C; `-p` has nobody to press it).
    pub(crate) fn with_cancel(
        mut self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Re-issue failed turns under the provider retry/failover policy.
    pub(crate) fn with_retry(mut self, retry: PrintRetry) -> Self {
        self.retry = Some(retry);
        self
    }

    /// A boundary with an explicit event sink (json-mode event-capture
    /// verifiers; the product path always uses [`TurnBoundary::new`]).
    #[cfg(test)]
    pub(crate) fn with_sink(json_mode: bool, sink: EventSink) -> Self {
        Self {
            recovery: OverflowRecovery::Idle,
            json_mode,
            sink,
            retry: None,
            cancel: None,
        }
    }

    /// The pre-turn check before an admitted prompt: the full TS
    /// `_runPreTurnCompaction` -> `_checkCompaction(lastAssistant,
    /// skipAbortedCheck=false, queueAutonomousContinuation=false)`: the
    /// same arm order as the settled boundary — the overflow recovery
    /// first (a stale overflow error from a previous run gets its
    /// compact-and-retry attempt here), and — only when Case 1 stayed
    /// silent — the threshold arm (a resumed session above the reserve
    /// headroom compacts before its first admitted prompt). A
    /// pre-turn compaction never re-issues (TS
    /// `resumeAfterFailure`/`_runPreTurnCompaction` leave the loop to the
    /// admitted prompt, which continues on the compacted context). The
    /// admitted prompt resets the recovery state right after the check
    /// (TS resets at the agent run's message start).
    pub(crate) async fn run_pre_turn(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
    ) -> Result<(), String> {
        // Case 1 (overflow): when it fires, TS returns from the check and
        // the threshold arm never runs in the same pass.
        let outcome = self
            .overflow_recovery_attempt(engine, model, api_key.clone(), OverflowBoundary::PreTurn)
            .await?;
        if matches!(outcome, OverflowOutcome::NotApplicable) {
            self.threshold_arm(engine, model, api_key).await?;
        }
        self.reset();
        Ok(())
    }

    /// The settled-turn boundary (TS `agent_end`): the overflow arm with
    /// its retry loop (a retried turn settles before the arm re-checks it),
    /// then — when the arm did not fire — the threshold arm (the order TS
    /// keeps inside `_checkCompaction`).
    pub(crate) async fn run_at_settled_turn(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
    ) -> Result<(), String> {
        loop {
            self.retry_failed_turn(engine).await?;
            let arm_finished = loop {
                let outcome = self
                    .overflow_recovery_attempt(
                        engine,
                        model,
                        api_key.clone(),
                        OverflowBoundary::SettledTurn,
                    )
                    .await?;
                if !matches!(outcome, OverflowOutcome::RetryTurn) {
                    break matches!(outcome, OverflowOutcome::Finished);
                }
            };
            if !arm_finished {
                self.threshold_arm(engine, model, api_key.clone()).await?;
            }
            // The threshold stopped the run mid tool chain: the model
            // answers the tool results on the compacted context (TS
            // post-compaction `agent.continue()`), and that run settles
            // through the same checks.
            if !engine.session.take_threshold_stop().await {
                return Ok(());
            }
            engine
                .session
                .agent()
                .continue_run()
                .await
                .map_err(|error| format!("{error:#}"))?;
            engine.session.agent().wait_for_idle().await;
        }
    }

    /// The provider retry of a turn that settled with an error: the
    /// chain re-issues it until it succeeds or the policy gives up, and
    /// the compaction arms then read whatever the turn settled on.
    async fn retry_failed_turn(&self, engine: &SessionEngine) -> Result<(), String> {
        let Some(retry) = &self.retry else {
            return Ok(());
        };
        let failed = engine
            .session
            .agent()
            .state()
            .await
            .messages
            .into_iter()
            .rev()
            .find_map(|message| match message {
                uh_agent::types::AgentMessage::Standard(uh_agent::types::Message::Assistant(
                    assistant,
                )) => Some(assistant),
                _ => None,
            })
            .filter(|assistant| assistant.stop_reason == uh_agent::types::StopReason::Error);
        let Some(failed) = failed else {
            return Ok(());
        };
        let host = TurnRetryHost {
            cwd: &retry.cwd,
            agent_dir: &retry.agent_dir,
            provider_target: &retry.provider_target,
            failover: retry.failover,
            max_park: Some(retry.max_park),
        };
        let cancel = self.cancel.clone();
        let cancelled = move || {
            cancel
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        };
        let mut emit = |event: &uh_core::session_engine::auto_retry::AutoRetryEvent| {
            if self.json_mode {
                (self.sink)(&auto_retry_event_json(event));
            }
        };
        engine
            .retry_failed_turn(&host, failed, &cancelled, &mut emit)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok(())
    }

    /// Admit one autonomous continuation turn through the same boundary
    /// pair the CLI prompts run (TS: the session admits an owed
    /// continuation through `_createPreparedTurnAction("followUp", ...)`,
    /// so it crosses `_prepareForCommit` -> `_runPreTurnCompaction` before
    /// the prompt and the `agent_end` checks after it — the arms are part
    /// of the session loop, not the CLI prompt loop). The `followUp`
    /// streaming behavior and the queue-if-busy admission match the
    /// autonomous driver seam the print loop calls.
    pub(crate) async fn admit_continuation(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
        prompt: &str,
    ) -> Result<(), String> {
        self.run_pre_turn(engine, model, api_key.clone()).await?;
        engine
            .session
            .prompt(
                prompt,
                uh_core::session_engine::PromptOptions {
                    streaming_behavior: Some(uh_core::session_engine::StreamingBehavior::FollowUp),
                    queue_if_busy: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| format!("{error:#}"))?;
        engine.session.agent().wait_for_idle().await;
        self.run_at_settled_turn(engine, model, api_key).await
    }
}

// The test mass (the faux harness and the boundary battery) moved to the
// child module at the same tree position (print_boundary::tests); the
// facade's cfg(test) re-export above keeps the failure-text const in the
// tests glob scope.
#[cfg(test)]
mod tests;
