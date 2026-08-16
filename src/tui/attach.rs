use std::collections::BTreeSet;
use std::time::Instant;

use crate::config::Config;
use crate::session::Session;
use crate::tmux::TmuxWindow;
use crate::tui_runtime::TerminalDriver;
use crate::view;

use super::Tui;

#[cfg(test)]
fn test_default_worktree_harness_config(config: &Config) -> Option<Config> {
    config.for_harness("opencode").ok()
}

#[cfg(not(test))]
fn test_default_worktree_harness_config(_config: &Config) -> Option<Config> {
    None
}

impl Tui {
    pub(crate) fn refresh_worktree_harness_configs(&mut self) {
        let live = self
            .sessions
            .iter()
            .filter_map(|session| {
                let managed = self.repos.get(session.repo_index)?;
                Some(session.identity_key(&managed.identity))
            })
            .collect::<BTreeSet<_>>();
        self.worktree_harness_configs
            .retain(|key, _| live.contains(key));
        for session in &self.sessions {
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            let key = session.identity_key(&managed.identity);
            if self.worktree_harness_configs.contains_key(&key) {
                continue;
            }
            let config = crate::session::worktree_harness(&managed.repo, session)
                .ok()
                .and_then(|association| managed.config.for_harness(&association.harness_id).ok())
                .or_else(|| test_default_worktree_harness_config(&managed.config));
            let Some(config) = config else { continue };
            self.worktree_harness_configs.insert(key, config);
        }
    }

    pub(crate) fn reload_worktree_harness_config(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let key = session.identity_key(&managed.identity);
        self.worktree_harness_configs.remove(&key);
        self.refresh_worktree_harness_configs();
        self.session_inventory_generation = self.session_inventory_generation.saturating_add(1);
        if self.session_refresh_in_flight {
            self.session_refresh_pending = true;
        }
    }

    pub(super) fn enter_agent_mode(
        &mut self,
        runtime: &mut dyn TerminalDriver,
    ) -> Result<(), String> {
        if self.selected_worktree_context().is_none() {
            return Ok(());
        }
        let Some(index) = self.selected_worktree_index() else {
            return Ok(());
        };
        self.enter_agent_mode_for_index(runtime, index)
    }

    pub(crate) fn enter_agent_mode_for_index(
        &mut self,
        runtime: &mut dyn TerminalDriver,
        index: usize,
    ) -> Result<(), String> {
        self.prepare_worktree_harness_for_open(runtime, index)?;
        let navigation = self.navigation_snapshot();
        let terminal_area = runtime.area()?;
        self.prepare_tmux_session_for_attach(
            index,
            (terminal_area.width, terminal_area.height.saturating_sub(1)),
        )?;
        let result =
            crate::tui_runtime::suspend_for(runtime, || self.attach_tmux_session_for_index(index));
        let refresh_started = Instant::now();
        self.refresh_sessions_after_tmux()?;
        crate::flight_recorder::record(
            "attach",
            "post_resume_refresh",
            Some(refresh_started.elapsed()),
            Vec::new(),
        );
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        if let Err(error) = result {
            self.show_error("tmux session failed", &error)?;
        }
        Ok(())
    }

    pub(super) fn prepare_worktree_harness_for_open(
        &mut self,
        runtime: &mut dyn TerminalDriver,
        index: usize,
    ) -> Result<(), String> {
        let Some(session) = self
            .sessions
            .get(index)
            .map(Session::background_job_snapshot)
        else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let target = managed.config.default_harness.clone();
        let association = crate::session::worktree_harness(&repo, &session)?;
        if association.harness_id == target || association.keep {
            return Ok(());
        }
        let choices = view::ChoiceList {
            title: "Worktree Harness Changed".to_string(),
            choices: vec![
                view::KeyChoice::new("m", format!("Migrate to {target}")),
                view::KeyChoice::new(
                    "l",
                    format!("Later; open {} and ask next time", association.harness_id),
                ),
                view::KeyChoice::new("k", format!("Keep {}; stop asking", association.harness_id)),
            ],
        };
        match self.prompt_choice_dialog(runtime, choices)?.as_deref() {
            Some("m") => {
                let use_ = crate::agent_session::session_use(
                    &self.repos,
                    &mut self.tmux_generations,
                    &session,
                );
                self.finish_tmux_warmup_for_key(&use_.warmup_key);
                if let Some(managed) = self.repos.get(session.repo_index) {
                    crate::agent_session::retire_generation(
                        &repo,
                        &managed.config,
                        &session.branch,
                        use_.generation,
                    );
                }
                crate::session::set_worktree_harness(&repo, &session, &target, false)?;
                self.reload_worktree_harness_config(index);
                crate::agent_session::rotate_generation(
                    &self.repos,
                    &mut self.tmux_generations,
                    use_.slot,
                );
            }
            Some("k") => crate::session::set_worktree_harness(
                &repo,
                &session,
                &association.harness_id,
                true,
            )?,
            _ => {}
        }
        Ok(())
    }

    pub(super) fn migrate_worktree_harness(&mut self, index: usize) -> Result<(), String> {
        let Some(session) = self
            .sessions
            .get(index)
            .map(Session::background_job_snapshot)
        else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let target = managed.config.default_harness.clone();
        let repository_config = managed.config.clone();
        let association = crate::session::worktree_harness(&repo, &session)?;
        if association.harness_id == target && !association.keep {
            self.show_message(&format!("worktree already uses harness '{target}'"))?;
            return Ok(());
        }
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        crate::agent_session::retire_generation(
            &repo,
            &repository_config,
            &session.branch,
            use_.generation,
        );
        crate::session::set_worktree_harness(&repo, &session, &target, false)?;
        self.reload_worktree_harness_config(index);
        crate::agent_session::rotate_generation(&self.repos, &mut self.tmux_generations, use_.slot);
        self.show_message(&format!("migrated worktree to harness '{target}'"))?;
        Ok(())
    }

    pub(super) fn open_tmux_window(
        &mut self,
        runtime: &mut dyn TerminalDriver,
        window: TmuxWindow,
    ) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let navigation = self.navigation_snapshot();
        let result =
            crate::tui_runtime::suspend_for(runtime, || self.attach_selected_tmux_window(window));
        self.refresh_sessions_after_tmux()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        result
    }
}
