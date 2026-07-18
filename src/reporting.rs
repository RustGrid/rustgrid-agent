use std::{
    cell::{Cell, RefCell},
    io::IsTerminal,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use anyhow::{Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    api::{RustGridClient, is_lease_lost},
    journal::{RecoveryPlan, RunJournal},
    lifecycle::{AgentRunStatus, LifecycleEvent, RunPhase, StepStatus, TicketStatus},
    token_consumption::TokenConsumption,
};

const DARK_BLUE_BACKGROUND: &str = "48;5;17";
const DARK_GREEN_BACKGROUND: &str = "48;5;22";

fn format_console_event(label: &str, message: &str, styled: bool) -> String {
    if styled {
        format!(
            "\x1b[1;97;{DARK_BLUE_BACKGROUND}m {label:>9} \x1b[0m\x1b[97;{DARK_GREEN_BACKGROUND}m {message} \x1b[0m"
        )
    } else {
        format!("[{label:>9}] {message}")
    }
}

pub(crate) fn console_event(label: &str, message: &str, _color: &str) {
    if std::env::var("RUSTGRID_AGENT_LOG").as_deref() == Ok("json") {
        let timestamp_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        println!(
            "{}",
            json!({
                "timestamp_unix_ms": timestamp_unix_ms,
                "component": "rustgrid-agent",
                "event": label.trim(),
                "message": message
            })
        );
        return;
    }
    let styled = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    println!("{}", format_console_event(label, message, styled));
}

fn feedback_idempotency_key(run_id: &str, message: &str) -> String {
    let digest = hex::encode(Sha256::digest(message.as_bytes()));
    format!("agent-comment-{run_id}-{}", &digest[..16])
}

pub(crate) struct Reporter<'a> {
    api: &'a RustGridClient,
    run_id: &'a str,
    row_version: Arc<AtomicI64>,
    ticket_id: &'a str,
    ticket_row_version: Cell<i64>,
    phase: Cell<RunPhase>,
    sequence: Cell<u64>,
    journal: RefCell<RunJournal>,
    progress_sequence: Cell<u64>,
    run_started: std::time::Instant,
    phase_started: RefCell<std::time::Instant>,
    token_consumption: Cell<TokenConsumption>,
}

impl<'a> Reporter<'a> {
    pub(crate) fn new(
        api: &'a RustGridClient,
        run_id: &'a str,
        row_version: Arc<AtomicI64>,
        ticket_id: &'a str,
        ticket_row_version: i64,
        journal: RunJournal,
    ) -> Self {
        let progress_sequence = journal.progress_sequence;
        let sequence = journal.last_sequence;
        let phase = journal.phase;
        let token_consumption = journal.token_consumption;
        Self {
            api,
            run_id,
            row_version,
            ticket_id,
            ticket_row_version: Cell::new(ticket_row_version),
            phase: Cell::new(phase),
            sequence: Cell::new(sequence),
            journal: RefCell::new(journal),
            progress_sequence: Cell::new(progress_sequence),
            run_started: std::time::Instant::now(),
            phase_started: RefCell::new(std::time::Instant::now()),
            token_consumption: Cell::new(token_consumption),
        }
    }

    fn enrich_event(&self, event: &mut LifecycleEvent) {
        if let Some(data) = event.data.as_object_mut() {
            data.insert(
                "run_elapsed_ms".into(),
                json!(self.run_started.elapsed().as_millis()),
            );
            data.insert(
                "phase_elapsed_ms".into(),
                json!(self.phase_started.borrow().elapsed().as_millis()),
            );
        }
    }

    fn publish_event(&self, event_kind: &str, event: &LifecycleEvent) -> Result<()> {
        let published_sequence = match self.api.publish_run_event(self.run_id, event_kind, event) {
            Ok(published) => published.sequence,
            Err(first_error) => {
                if is_lease_lost(&first_error) {
                    return Err(first_error);
                }
                if let Some(accepted_sequence) = self.api.find_event_by_client_sequence(
                    self.run_id,
                    self.progress_sequence.get(),
                    event.sequence,
                )? {
                    accepted_sequence
                } else {
                    eprintln!(
                        "[warning] event publish outcome was ambiguous; retrying once: {first_error:#}"
                    );
                    self.api
                        .publish_run_event(self.run_id, event_kind, event)?
                        .sequence
                }
            }
        };
        self.progress_sequence.set(published_sequence);
        self.journal
            .borrow_mut()
            .record_progress_sequence(published_sequence)
    }

    pub(crate) fn step(
        &self,
        name: &str,
        status: StepStatus,
        message: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        let status_value = status.as_str();
        console_event(status_value, message, status.console_color());
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        let mut event = LifecycleEvent::new(
            sequence,
            self.phase.get(),
            format!("step.{name}.{status_value}"),
            status.severity(),
            message,
            metadata,
        );
        self.enrich_event(&mut event);
        if let Err(error) = self
            .journal
            .borrow_mut()
            .checkpoint(self.phase.get(), sequence)
        {
            eprintln!("[warning] could not persist run checkpoint: {error:#}");
        }
        self.publish_event("progress", &event)?;
        self.api
            .append_step(
                self.run_id,
                sequence,
                name,
                status,
                message,
                Some(event.metadata()),
            )
            .with_context(|| format!("could not report step {name} to RustGrid"))
    }

    pub(crate) fn set_phase(&self, phase: RunPhase) {
        if !self.phase.get().can_transition_to(phase) {
            eprintln!(
                "[warning] ignored invalid run phase transition from {} to {}",
                self.phase.get().as_str(),
                phase.as_str()
            );
            return;
        }
        self.phase.set(phase);
        self.phase_started.replace(std::time::Instant::now());
        console_event("phase", phase.as_str(), "35");
        if let Err(error) = self
            .journal
            .borrow_mut()
            .checkpoint(phase, self.sequence.get())
        {
            eprintln!("[warning] could not persist run phase: {error:#}");
        }
    }

    pub(crate) fn phase(&self) -> RunPhase {
        self.phase.get()
    }

    pub(crate) fn recovery_plan(&self) -> Result<RecoveryPlan> {
        self.journal.borrow().recovery_plan()
    }

    pub(crate) fn record_error(&self, message: &str) -> Result<()> {
        self.journal.borrow_mut().record_error(message)
    }

    pub(crate) fn observe_token_consumption(&self, line: &str) -> Result<()> {
        let mut consumption = self.token_consumption.get();
        if consumption.observe_codex_jsonl(line)? {
            self.token_consumption.set(consumption);
            self.journal
                .borrow_mut()
                .record_token_consumption(consumption)?;
        }
        Ok(())
    }

    pub(crate) fn report_token_consumption(&self) -> Result<()> {
        self.api
            .report_token_consumption(self.run_id, self.token_consumption.get())
            .context("could not report final token consumption to RustGrid")
    }

    pub(crate) fn record_executor(&self, kind: &str, id: &str, state: &str) -> Result<()> {
        self.journal.borrow_mut().record_executor(kind, id, state)
    }

    pub(crate) fn record_branch(&self, branch: &str) -> Result<()> {
        self.journal.borrow_mut().record_branch(branch)
    }

    pub(crate) fn record_commit(&self, commit: &str) -> Result<()> {
        self.journal.borrow_mut().record_commit(commit)
    }

    pub(crate) fn record_pull_request(&self, url: &str, number: u64) -> Result<()> {
        self.journal.borrow_mut().record_pull_request(url, number)
    }

    pub(crate) fn update_run(&self, status: AgentRunStatus, message: Option<&str>) -> Result<()> {
        let run = self.api.update_run(
            self.run_id,
            self.row_version.load(Ordering::SeqCst),
            status,
            message,
        )?;
        self.row_version.store(run.row_version, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) fn set_ticket_status(&self, status: TicketStatus) -> Result<()> {
        let fresh = self
            .api
            .fetch_ticket(self.ticket_id)
            .context("could not refresh ticket ETag before status update")?;
        self.ticket_row_version.set(fresh.row_version);
        let row_version = self.ticket_row_version.get();
        let version = self.api.update_ticket_status(
            self.ticket_id,
            row_version,
            status,
            &format!(
                "ticket-status-{}-{}-{row_version}",
                self.run_id,
                status.as_str()
            ),
        )?;
        self.ticket_row_version.set(version);
        console_event(
            "status",
            &format!("Ticket is now {}", status.as_str()),
            "34",
        );
        Ok(())
    }

    pub(crate) fn feedback(&self, message: &str) -> Result<()> {
        console_event("feedback", message, "36");
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        let mut event = LifecycleEvent::new(
            sequence,
            self.phase.get(),
            "agent.message",
            "info",
            message,
            None,
        );
        self.enrich_event(&mut event);
        self.journal
            .borrow_mut()
            .checkpoint(self.phase.get(), sequence)?;
        self.publish_event("message", &event)?;
        self.api.create_comment(
            self.ticket_id,
            &format!("🤖 **RustGrid Agent update**\n\n{message}"),
            &feedback_idempotency_key(self.run_id, message),
        )
    }

    pub(crate) fn log(&self, message: &str) -> Result<()> {
        if message.trim().is_empty() {
            return Ok(());
        }
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        let bounded = truncate(message, 16_000);
        let mut event = LifecycleEvent::new(
            sequence,
            self.phase.get(),
            "quality_gate.output",
            "info",
            "Quality gate produced output",
            Some(json!({"output": bounded})),
        );
        self.enrich_event(&mut event);
        self.journal
            .borrow_mut()
            .checkpoint(self.phase.get(), sequence)?;
        self.publish_event("log", &event)
    }

    pub(crate) fn fail(&self, error: &anyhow::Error) -> Result<()> {
        let message = format!("{error:#}");
        if let Err(journal_error) = self.record_error(&message) {
            eprintln!("[warning] could not retain failure diagnostic: {journal_error:#}");
        }
        if self.phase() != RunPhase::TimedOut {
            self.set_phase(RunPhase::Blocked);
        }
        let step_result = self.step("run_failed", StepStatus::Failed, &message, None);
        let comment_result = self.api.create_comment(
            self.ticket_id,
            &format!(
                "⛔ **RustGrid Agent blocked**\n\n{message}\n\nHuman intervention is required before the agent can continue."
            ),
            &format!("agent-comment-{}-blocked", self.run_id),
        );
        let ticket_result = self.set_ticket_status(TicketStatus::Blocked);
        let update_result = self.update_run(AgentRunStatus::Failed, Some(&message));
        for (context, result) in [
            ("report failed step", step_result),
            ("mark RustGrid run failed", update_result),
            ("append blocked ticket comment", comment_result),
            ("mark ticket blocked", ticket_result),
        ] {
            if let Err(error) = result {
                eprintln!("[warning] could not {context}: {error:#}");
            }
        }
        Ok(())
    }

    pub(crate) fn fail_retryable(&self, error: &anyhow::Error) -> Result<()> {
        let message = format!("{error:#}");
        if let Err(journal_error) = self.record_error(&message) {
            eprintln!("[warning] could not retain failure diagnostic: {journal_error:#}");
        }
        self.set_phase(RunPhase::Failed);
        let step_result = self.step("run_failed", StepStatus::Failed, &message, None);
        let comment_result = self.api.create_comment(
            self.ticket_id,
            &format!(
                "⚠️ **RustGrid Agent run failed**\n\n{message}\n\nThis was classified as a temporary or internal failure. The ticket has been returned to todo and can be retried."
            ),
            &format!("agent-comment-{}-failed", self.run_id),
        );
        let ticket_result = self.set_ticket_status(TicketStatus::Todo);
        let update_result = self.update_run(AgentRunStatus::Failed, Some(&message));
        for (context, result) in [
            ("report failed step", step_result),
            ("mark RustGrid run failed", update_result),
            ("append retryable failure comment", comment_result),
            ("return ticket to todo", ticket_result),
        ] {
            if let Err(error) = result {
                eprintln!("[warning] could not {context}: {error:#}");
            }
        }
        Ok(())
    }

    pub(crate) fn cancel(&self) -> Result<()> {
        self.record_error("cancelled by operator")?;
        self.set_phase(RunPhase::Cancelled);
        self.step(
            "run_cancelled",
            StepStatus::Cancelled,
            "Agent run cancelled by operator",
            None,
        )?;
        self.api.create_comment(
            self.ticket_id,
            "🛑 **RustGrid Agent stopped**\n\nThe run was cancelled by the worker operator and can be retried.",
            &format!("agent-comment-{}-cancelled", self.run_id),
        )?;
        self.set_ticket_status(TicketStatus::Todo)?;
        self.update_run(AgentRunStatus::Cancelled, Some("cancelled by operator"))
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_events_use_dark_blue_and_green_panels_with_white_details() {
        let event = format_console_event("phase", "executing", true);

        assert_eq!(
            event,
            "\x1b[1;97;48;5;17m     phase \x1b[0m\x1b[97;48;5;22m executing \x1b[0m"
        );
    }

    #[test]
    fn plain_events_remain_readable_without_ansi_styling() {
        assert_eq!(
            format_console_event("phase", "executing", false),
            "[    phase] executing"
        );
    }

    #[test]
    fn feedback_keys_are_stable_and_message_specific() {
        assert_eq!(
            feedback_idempotency_key("run-1", "blocked"),
            feedback_idempotency_key("run-1", "blocked")
        );
        assert_ne!(
            feedback_idempotency_key("run-1", "blocked"),
            feedback_idempotency_key("run-1", "running")
        );
    }
}
