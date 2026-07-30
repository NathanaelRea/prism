use super::*;
use std::time::Instant;

use ansi_to_tui::IntoText as _;
use ratatui::text::Line;

use crate::tui::{TmuxPortalCapture, TmuxPortalResult, TmuxPortalSnapshot, TmuxPortalTarget};

const TMUX_PORTAL_POLL_INTERVAL: Duration = Duration::from_millis(300);
const TMUX_PORTAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);
// A resize and capture can each consume the tmux command timeout.
const TMUX_PORTAL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

impl Tui {
    pub(crate) fn poll_tmux_portal(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let target = self.selected_tmux_portal_target();
        let target_key = target.as_ref().map(|target| match target {
            Ok(target) => &target.key,
            Err(key) => key,
        });
        let mut changed = false;

        while let Ok(result) = self.tmux_portal_rx.try_recv() {
            crate::flight_recorder::record(
                "tmux",
                "portal_result",
                Some(result.started_at.elapsed()),
                vec![
                    crate::flight_recorder::text("target", &result.key.slot.worktree.branch),
                    crate::flight_recorder::text("window", "agent"),
                    crate::flight_recorder::unsigned("generation", result.key.generation),
                    crate::flight_recorder::boolean("success", result.capture.is_ok()),
                ],
            );
            let is_current = self
                .tmux_portal_polls_in_flight
                .get(&result.key)
                .map(|started_at| *started_at == result.started_at)
                .unwrap_or_else(|| target_key == Some(&result.key));
            if self.tmux_portal_polls_in_flight.contains_key(&result.key) && is_current {
                self.tmux_portal_polls_in_flight.remove(&result.key);
            }
            if is_current && let Some(size) = result.resized_size {
                self.tmux_portal_resized = Some((result.key.clone(), size));
            }
            if is_current && target_key == Some(&result.key) {
                let key = result.key;
                let snapshot = TmuxPortalSnapshot {
                    key: key.clone(),
                    capture: Some(TmuxPortalCapture {
                        key,
                        result: result.capture,
                    }),
                };
                if self.tmux_portal.as_ref() != Some(&snapshot) {
                    self.tmux_portal = Some(snapshot);
                    changed = true;
                }
            }
        }
        let Some(target) = target else {
            self.tmux_portal_last_polled.clear();
            if self.tmux_portal.take().is_some() {
                changed = true;
            }
            return changed;
        };
        let target = match target {
            Ok(target) => target,
            Err(key) => {
                self.tmux_portal_last_polled.clear();
                let snapshot = TmuxPortalSnapshot {
                    key: key.clone(),
                    capture: Some(TmuxPortalCapture {
                        key,
                        result: Err("harness unavailable".to_string()),
                    }),
                };
                if self.tmux_portal.as_ref() != Some(&snapshot) {
                    self.tmux_portal = Some(snapshot);
                    changed = true;
                }
                return changed;
            }
        };
        self.tmux_portal_last_polled
            .retain(|key, _| key == &target.key);
        let target_changed =
            self.tmux_portal.as_ref().map(|portal| &portal.key) != Some(&target.key);
        if target_changed {
            let previous_capture = self
                .tmux_portal
                .as_ref()
                .and_then(|portal| portal.capture.as_ref())
                .filter(|capture| capture.result.is_ok())
                .cloned();
            self.tmux_portal = Some(TmuxPortalSnapshot {
                key: target.key.clone(),
                capture: previous_capture,
            });
            self.tmux_portal_last_polled
                .entry(target.key.clone())
                .or_insert_with(Instant::now);
            changed = true;
        }

        let capture = self
            .tmux_portal
            .as_ref()
            .and_then(|portal| portal.capture.as_ref())
            .map(|capture| &capture.result);
        let interval = match (target_changed, capture) {
            (true, _) => Duration::ZERO,
            (false, None) => Duration::ZERO,
            (false, Some(Err(_))) => TMUX_PORTAL_RETRY_INTERVAL,
            (false, Some(Ok(_))) => TMUX_PORTAL_POLL_INTERVAL,
        };
        let due = self
            .tmux_portal_last_polled
            .get(&target.key)
            .is_none_or(|last| last.elapsed() >= interval);
        if due && self.tmux_portal_polls_in_flight.is_empty() {
            let reason = match (target_changed, capture) {
                (true, _) => "target_changed",
                (false, None) => "initial",
                (false, Some(Err(_))) => "capture_error",
                (false, Some(Ok(_))) => "periodic",
            };
            let target_session = crate::tmux::TmuxAgentSession::for_worktree_session(
                &target.repo,
                &target.key.slot.worktree.branch,
                target.key.generation,
            )
            .name()
            .to_string();
            crate::flight_recorder::record(
                "tmux",
                "portal_poll",
                None,
                vec![
                    crate::flight_recorder::text("target_session", target_session),
                    crate::flight_recorder::text("window", "agent"),
                    crate::flight_recorder::unsigned("generation", target.key.generation),
                    crate::flight_recorder::unsigned("poll_interval_ms", interval.as_millis()),
                    crate::flight_recorder::text("retry_reason", reason),
                ],
            );
            let started_at = Instant::now();
            self.tmux_portal_last_polled
                .insert(target.key.clone(), started_at);
            self.tmux_portal_polls_in_flight
                .insert(target.key.clone(), started_at);
            let resize =
                self.tmux_portal_resized.as_ref() != Some(&(target.key.clone(), target.size));
            let key = target.key.clone();
            self.spawn_tui_job(
                TuiJobKind::TmuxPortal,
                TuiJobKey::Tmux(key.clone()),
                key.generation,
                Some(TMUX_PORTAL_CAPTURE_TIMEOUT),
                format!("prism-tmux-portal-{}", key.slot.worktree.branch),
                move |_| {
                    let (capture, resized_size) = (|| {
                        if resize {
                            crate::tmux::resize_agent_pane(
                                &target.repo,
                                &target.config,
                                &target.key.slot.worktree.branch,
                                target.key.generation,
                                target.size.0,
                                target.size.1,
                            )?;
                        }
                        Ok((
                            crate::tmux::capture_agent_pane(
                                &target.repo,
                                &target.config,
                                &target.key.slot.worktree.branch,
                                target.key.generation,
                            )
                            .map(normalize_capture),
                            resize.then_some(target.size),
                        ))
                    })()
                    .unwrap_or_else(|error| (Err(error), None));
                    Ok(Some(TuiJobPayload::TmuxPortal(TmuxPortalResult {
                        key,
                        started_at,
                        capture,
                        resized_size,
                    })))
                },
            );
        }
        changed
    }

    fn selected_tmux_portal_target(
        &mut self,
    ) -> Option<Result<TmuxPortalTarget, AgentSessionWarmupKey>> {
        if self.focused_panel != crate::tui::PanelFocus::Worktrees {
            return None;
        }
        let context = self.selected_worktree_context()?;
        let size = self.tmux_portal_size?;
        let session = self
            .sessions
            .get(context.session_index)?
            .background_job_snapshot();
        let managed = self.repos.get(session.repo_index)?;
        let repo = managed.repo.clone();
        let slot = crate::agent_session::AgentSessionSlot::for_repository_session(
            &managed.identity,
            &session,
        );
        let generation = self.tmux_generations.get(&slot).copied()?;
        let key = AgentSessionWarmupKey::new(slot, generation);
        let session_key = session.identity_key(&managed.identity);
        let Some(config) = self.worktree_harness_configs.get(&session_key).cloned() else {
            return Some(Err(key));
        };
        Some(Ok(TmuxPortalTarget {
            key,
            repo,
            config,
            size,
        }))
    }
}

fn normalize_capture(capture: String) -> Vec<Line<'static>> {
    capture
        .into_text()
        .map(|text| text.lines)
        .unwrap_or_else(|_| {
            crate::util::strip_ansi(&capture)
                .lines()
                .map(|line| Line::from(line.to_string()))
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::normalize_capture;

    #[test]
    fn normalize_capture_preserves_ansi_colors_and_modifiers() {
        let lines = normalize_capture(
            "\x1b[31;1mred\x1b[0m \x1b[38;2;10;20;30;48;5;42mcolor\x1b[0m".to_string(),
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "red");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[2].content, "color");
        assert_eq!(lines[0].spans[2].style.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(lines[0].spans[2].style.bg, Some(Color::Indexed(42)));
    }

    #[test]
    fn normalize_capture_preserves_trailing_styled_spaces() {
        let lines = normalize_capture("text\x1b[41m   \x1b[0m".to_string());

        assert_eq!(lines[0].spans[1].content, "   ");
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Red));
    }
}
