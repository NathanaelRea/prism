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
}
