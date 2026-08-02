const DEFAULT_SHELL: &str = "/bin/sh";

pub fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

pub(crate) fn shell_program(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SHELL)
        .to_string()
}

pub(crate) fn shell_program_from_env() -> String {
    shell_program(std::env::var("SHELL").ok().as_deref())
}

pub(crate) fn editor_argv(
    visual: Option<&str>,
    editor: Option<&str>,
    mut command_exists: impl FnMut(&str) -> bool,
) -> Result<Option<Vec<String>>, String> {
    if let Some(value) = visual
        .filter(|value| !value.trim().is_empty())
        .or_else(|| editor.filter(|value| !value.trim().is_empty()))
    {
        return crate::process::parse_command_words(value).map(Some);
    }

    Ok(["nvim", "vim", "vi"]
        .into_iter()
        .find(|candidate| command_exists(candidate))
        .map(|candidate| vec![candidate.to_string()]))
}

pub(crate) fn editor_argv_from_env() -> Result<Option<Vec<String>>, String> {
    editor_argv(
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
        crate::process::command_exists,
    )
}

pub(crate) fn posix_shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
