use super::*;

impl Tui {
    pub(crate) async fn open_selected_repo_lazygit(
        &mut self,
        raw: &mut dyn crate::tui_runtime::TerminalDriver,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let command = Command::new(context.config.tool("lazygit")).current_dir(&context.repo.root);
        raw.suspend_for_async(crate::process::run_status_inherited(command))
            .await?;
        self.show_message("returned from repository lazygit")?;
        Ok(())
    }

    pub(crate) async fn open_selected_repo_terminal(
        &mut self,
        raw: &mut dyn crate::tui_runtime::TerminalDriver,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let shell = crate::terminal::shell_program_from_env();
        let command = Command::new(&shell).current_dir(&context.repo.root);
        raw.suspend_for_async(crate::process::run_status_inherited(command))
            .await?;
        self.show_message("returned from repository terminal")?;
        Ok(())
    }
}
