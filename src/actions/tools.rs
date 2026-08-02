use super::*;

impl Tui {
    pub(crate) fn open_selected_repo_lazygit(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        raw.suspend_for(|| {
            crate::process::run_status_inherited(
                Command::new(context.config.tool("lazygit")).current_dir(&context.repo.root),
            )
        })?;
        self.show_message("returned from repository lazygit")?;
        Ok(())
    }

    pub(crate) fn open_selected_repo_terminal(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let shell = crate::terminal::shell_program_from_env();
        raw.suspend_for(|| {
            crate::process::run_status_inherited(
                Command::new(&shell).current_dir(&context.repo.root),
            )
        })?;
        self.show_message("returned from repository terminal")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn open_selected_repo_plan_mode(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let root = context.repo.root.clone();
        let config = context.config.clone();
        let navigation = self.navigation_snapshot();
        let result = raw.suspend_for(|| open_plan_mode(&config, &root));
        self.refresh_sessions_after_tmux()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        result?;
        self.show_message("returned from plan mode")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn open_selected_worktree_plan_mode(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let path = self.sessions[context.session_index].path.clone();
        let config = context.config.clone();
        let navigation = self.navigation_snapshot();
        let result = raw.suspend_for(|| open_plan_mode(&config, &path));
        self.refresh_sessions_after_tmux()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        result?;
        self.show_message("returned from plan mode")?;
        Ok(())
    }
}
