use crate::platform::{
    CommandCandidate, DesktopNotificationPolicy, SupportedOs, browser_candidates,
    default_session_runtime, default_worktrunk_command, desktop_notification_policy,
};

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
    assert_eq!(
        browser_candidates(SupportedOs::Windows),
        [CommandCandidate {
            program: "explorer.exe",
            args: &[],
        }]
    );
}

#[test]
fn platform_contract_windows_native_tool_names_avoid_system_aliases() {
    assert_eq!(default_session_runtime(SupportedOs::Linux), "tmux");
    assert_eq!(default_session_runtime(SupportedOs::MacOs), "tmux");
    assert_eq!(default_session_runtime(SupportedOs::Windows), "psmux.exe");
    assert_eq!(default_worktrunk_command(SupportedOs::Linux), "wt");
    assert_eq!(
        default_worktrunk_command(SupportedOs::Windows),
        "git-wt.exe"
    );
}

#[test]
fn platform_contract_notification_policy_is_explicit() {
    assert_eq!(
        desktop_notification_policy(SupportedOs::Linux),
        DesktopNotificationPolicy::NativeWorker
    );
    assert_eq!(
        desktop_notification_policy(SupportedOs::MacOs),
        DesktopNotificationPolicy::TerminalSubscriber
    );
    assert_eq!(
        desktop_notification_policy(SupportedOs::Windows),
        DesktopNotificationPolicy::Unavailable
    );
}

#[test]
fn platform_contract_shell_selection_is_explicit() {
    assert_eq!(
        crate::terminal::shell_program_for(SupportedOs::Linux, Some("/bin/zsh")),
        "/bin/zsh"
    );
    assert_eq!(
        crate::terminal::shell_program_for(SupportedOs::MacOs, Some("  ")),
        "/bin/sh"
    );
    assert_eq!(
        crate::terminal::shell_program_for(SupportedOs::Windows, None),
        "pwsh.exe"
    );
}

#[test]
fn platform_contract_editor_values_preserve_argv_for_both_supported_os_policies() {
    for os in [SupportedOs::Linux, SupportedOs::MacOs, SupportedOs::Windows] {
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

#[test]
fn platform_contract_powershell_serialization_is_literal_and_injection_safe() {
    let environment = std::collections::BTreeMap::from([(
        "SAFE".to_string(),
        r"value'; Remove-Item C:\data".to_string(),
    )]);
    let command = crate::terminal::shell_command_for(
        SupportedOs::Windows,
        &[
            r"C:\Program Files\Agent\agent.exe".to_string(),
            "line one\n'line two'; $HOME".to_string(),
            r"\\server\share\Unicode δ file.txt".to_string(),
        ],
        &environment,
        Some(std::path::Path::new(r"C:\Temp\prompt ' one.txt")),
    )
    .unwrap();

    assert!(command.starts_with("$env:SAFE = 'value''; Remove-Item C:\\data'; & '"));
    assert!(command.contains("'line one\n''line two''; $HOME'"));
    assert!(command.contains(r"'\\server\share\Unicode δ file.txt'"));
    assert!(command.contains("Remove-Item -LiteralPath 'C:\\Temp\\prompt '' one.txt'"));
    assert!(command.ends_with("exit $prismStatus"));

    let invalid_environment = std::collections::BTreeMap::from([(
        "BAD; Remove-Item C:\\data".to_string(),
        "value".to_string(),
    )]);
    assert!(
        crate::terminal::shell_command_for(
            SupportedOs::Windows,
            &["agent.exe".to_string()],
            &invalid_environment,
            None,
        )
        .is_err()
    );
}
