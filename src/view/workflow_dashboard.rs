use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::run::{RunProjection, StepState};
use crate::util::truncate;

/// Generic graph/history projection used for every generalized Workflow.
/// Bodies are already bounded by the Run Ledger; rendering performs no I/O.
#[derive(Clone, Debug)]
pub(crate) struct WorkflowDashboard {
    pub projection: RunProjection,
    pub selected_attempt: Option<crate::run::AttemptId>,
}

pub(crate) fn workflow_dashboard_lines(
    dashboard: &WorkflowDashboard,
    width: usize,
) -> Vec<Line<'static>> {
    let projection = &dashboard.projection;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            projection.run.definition.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            projection.run.state.label(),
            state_style(projection.run.state.label()),
        ),
        Span::raw(format!("  {}", projection.run.id.as_str())),
    ])];

    for step in &projection.steps {
        let determining = projection
            .determining_steps
            .iter()
            .any(|candidate| candidate == &step.id);
        let branch = if determining { "●" } else { "○" };
        lines.push(Line::from(vec![
            Span::styled(format!(" {branch} "), state_style(step.state.label())),
            Span::styled(
                truncate(&step.definition_step_id, width.saturating_sub(26)),
                Style::default().add_modifier(if determining {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::raw(format!("  {}", step.state.label())),
            Span::raw(
                step.blocker
                    .as_deref()
                    .map(|blocker| format!("  {blocker}"))
                    .unwrap_or_default(),
            ),
        ]));
        for attempt in projection
            .attempts
            .iter()
            .filter(|attempt| attempt.step_id == step.id)
        {
            let selected = dashboard
                .selected_attempt
                .as_ref()
                .is_some_and(|selected| selected == &attempt.id);
            lines.push(Line::from(vec![
                Span::raw(if selected { "   ▶ " } else { "     " }),
                Span::raw(format!(
                    "attempt {}  {}  {}",
                    attempt.ordinal,
                    attempt.state,
                    attempt.id.as_str()
                )),
                Span::raw(
                    attempt
                        .terminal_reason
                        .as_deref()
                        .map(|reason| format!("  {reason}"))
                        .unwrap_or_default(),
                ),
            ]));
        }
    }

    if !projection.approvals.is_empty() {
        lines.push(Line::from(Span::styled(
            "Approvals",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for approval in &projection.approvals {
            lines.push(Line::raw(format!(
                "  {}  {:?}  {}",
                approval.request.id.as_str(),
                approval.request.mode,
                approval.request.state
            )));
        }
    }
    if !projection.gates.is_empty() {
        lines.push(Line::from(Span::styled(
            "Gates",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for gate in &projection.gates {
            lines.push(Line::raw(format!(
                "  {}  {}  {}",
                gate.status, gate.policy_revision, gate.reason
            )));
        }
    }
    if !projection.effects.is_empty() {
        lines.push(Line::from(Span::styled(
            "Effects",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for effect in &projection.effects {
            lines.push(Line::raw(format!(
                "  {}  {}  {}",
                effect.kind, effect.state, effect.reconciliation_key
            )));
        }
    }
    if !projection.artifacts.is_empty() {
        lines.push(Line::from(Span::styled(
            "Artifacts",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for artifact in &projection.artifacts {
            lines.push(Line::raw(format!(
                "  {}  {}#{}  {} bytes",
                artifact.port,
                artifact.artifact.artifact_type,
                artifact.artifact.revision,
                artifact.size
            )));
        }
    }

    let selected = dashboard.selected_attempt.as_ref().or_else(|| {
        projection
            .attempts
            .iter()
            .rev()
            .map(|attempt| &attempt.id)
            .next()
    });
    if let Some(selected) = selected {
        lines.push(Line::from(Span::styled(
            "Bounded output",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for output in projection
            .output
            .iter()
            .filter(|output| &output.attempt_id == selected)
        {
            let text = String::from_utf8_lossy(&output.bytes);
            for line in text.lines() {
                lines.push(Line::raw(format!("  {} │ {line}", output.stream)));
            }
            if output.truncated {
                lines.push(Line::styled(
                    "  … output truncated",
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
    }
    lines
}

fn state_style(state: &str) -> Style {
    let color = match state {
        "completed" | "satisfied" => Color::Green,
        "failed" | "cancelled" | "unsatisfied" => Color::Red,
        "input_required" | "recovery_required" => Color::Yellow,
        "active" | "runnable" => Color::Cyan,
        _ => Color::DarkGray,
    };
    Style::default().fg(color)
}

#[allow(dead_code)]
fn _assert_step_state_is_generic(_: StepState) {}
