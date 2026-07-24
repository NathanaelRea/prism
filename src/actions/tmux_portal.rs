use super::*;
use std::time::Instant;

use ansi_to_tui::IntoText as _;
use ratatui::text::Line;

use crate::tui::{TmuxPortalCapture, TmuxPortalResult, TmuxPortalSnapshot, TmuxPortalTarget};

const TMUX_PORTAL_POLL_INTERVAL: Duration = Duration::from_millis(150);
const TMUX_PORTAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const TMUX_PORTAL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

impl Tui {
    pub(crate) fn poll_tmux_portal(&mut self) -> bool {
        let target = self.selected_tmux_portal_target();
        let target_key = target.as_ref().map(|target| match target {
            Ok(target) => &target.key,
            Err(key) => key,
        });
        let mut changed = false;

        while let Ok(result) = self.tmux_portal_rx.try_recv() {
            let is_current =
                self.tmux_portal_polls_in_flight.get(&result.key) == Some(&result.started_at);
            if is_current {
                self.tmux_portal_polls_in_flight.remove(&result.key);
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
            self.tmux_portal_polls_in_flight.clear();
            if self.tmux_portal.take().is_some() {
                changed = true;
            }
            return changed;
        };
        let target = match target {
            Ok(target) => target,
            Err(key) => {
                self.tmux_portal_last_polled.clear();
                self.tmux_portal_polls_in_flight.clear();
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
        self.tmux_portal_polls_in_flight.retain(|key, started_at| {
            key == &target.key && started_at.elapsed() < TMUX_PORTAL_CAPTURE_TIMEOUT
        });
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
        if due && !self.tmux_portal_polls_in_flight.contains_key(&target.key) {
            let started_at = Instant::now();
            self.tmux_portal_last_polled
                .insert(target.key.clone(), started_at);
            self.tmux_portal_polls_in_flight
                .insert(target.key.clone(), started_at);
            let tx = self.tmux_portal_tx.clone();
            std::thread::spawn(move || {
                let capture = crate::tmux::capture_agent_pane(
                    &target.repo,
                    &target.config,
                    &target.key.slot.branch,
                    target.key.generation,
                    target.size.0,
                    target.size.1,
                )
                .map(normalize_capture);
                let _ = tx.send(TmuxPortalResult {
                    key: target.key,
                    started_at,
                    capture,
                });
            });
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
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        let key = use_.warmup_key;
        let association = match crate::session::worktree_harness(&managed.repo, &session) {
            Ok(association) => association,
            Err(_) => return Some(Err(key)),
        };
        let config = match managed.config.for_harness(&association.harness_id) {
            Ok(config) => config,
            Err(_) => return Some(Err(key)),
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
