use super::*;

impl Tui {
    pub(crate) fn open_selected_repo_lazygit(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        raw.suspend()?;
        let result = Command::new(context.config.tool("lazygit"))
            .current_dir(&context.repo.root)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("lazygit: {error}"))?;
        if !status.success() {
            return Err(format!("lazygit exited with {status}"));
        }
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
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        raw.suspend()?;
        let result = Command::new(&shell)
            .current_dir(&context.repo.root)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{shell}: {error}"))?;
        if !status.success() {
            return Err(format!("{shell} exited with {status}"));
        }
        self.show_message("returned from repository terminal")?;
        Ok(())
    }

    pub(crate) fn open_home_terminal(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let home = crate::util::home_dir().ok_or_else(|| "HOME is not set".to_string())?;
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        raw.suspend()?;
        let result = Command::new(&shell).current_dir(&home).status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{shell}: {error}"))?;
        if !status.success() {
            return Err(format!("{shell} exited with {status}"));
        }
        self.show_message("returned from home terminal")?;
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
        raw.suspend()?;
        let result = open_plan_mode(&config, &root);
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        resume_result?;
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
        raw.suspend()?;
        let result = open_plan_mode(&config, &path);
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        resume_result?;
        result?;
        self.show_message("returned from plan mode")?;
        Ok(())
    }
}
