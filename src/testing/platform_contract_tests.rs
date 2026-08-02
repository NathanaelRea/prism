use crate::platform::{CommandCandidate, SupportedOs, browser_candidates};

#[test]
fn platform_contract_browser_candidates_are_explicit_for_every_supported_os() {
    assert_eq!(
        browser_candidates(SupportedOs::Linux),
        [
            CommandCandidate {
                program: "xdg-open",
                args: &[],
            },
            CommandCandidate {
                program: "gio",
                args: &["open"],
            },
            CommandCandidate {
                program: "wslview",
                args: &[],
            },
        ]
    );
    assert_eq!(
        browser_candidates(SupportedOs::MacOs),
        [CommandCandidate {
            program: "open",
            args: &[],
        }]
    );
}

#[test]
fn platform_contract_shell_selection_is_shared_and_posix() {
    assert_eq!(crate::terminal::shell_program(Some("/bin/zsh")), "/bin/zsh");
    assert_eq!(crate::terminal::shell_program(Some("  ")), "/bin/sh");
    assert_eq!(crate::terminal::shell_program(None), "/bin/sh");
}

#[test]
fn platform_contract_editor_values_preserve_argv_for_both_supported_os_policies() {
    for os in [SupportedOs::Linux, SupportedOs::MacOs] {
        assert!(!browser_candidates(os).is_empty());
        assert_eq!(
            crate::terminal::editor_argv(
                Some(r#"code --wait --profile "Prism Work""#),
                Some("vim"),
                |_| false,
            )
            .unwrap(),
            Some(vec![
                "code".to_string(),
                "--wait".to_string(),
                "--profile".to_string(),
                "Prism Work".to_string(),
            ])
        );
    }
}

#[test]
fn platform_contract_editor_precedence_fallback_and_errors_are_deterministic() {
    assert_eq!(
        crate::terminal::editor_argv(Some("  "), Some("vim -f"), |_| false).unwrap(),
        Some(vec!["vim".to_string(), "-f".to_string()])
    );
    assert_eq!(
        crate::terminal::editor_argv(None, None, |candidate| candidate == "vim").unwrap(),
        Some(vec!["vim".to_string()])
    );
    assert!(crate::terminal::editor_argv(Some("code '"), None, |_| false).is_err());
}

#[test]
fn platform_contract_posix_quoting_preserves_shell_argument_boundaries() {
    assert_eq!(crate::terminal::posix_shell_quote(""), "''");
    assert_eq!(
        crate::terminal::posix_shell_quote("two words"),
        "'two words'"
    );
    assert_eq!(
        crate::terminal::posix_shell_quote("that's"),
        "'that'\"'\"'s'"
    );
}
