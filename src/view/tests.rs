use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Position};

use crate::{
    agent::AgentState,
    auto_flow::{
        AutoImplementationSource, AutoOutputKind, AutoOutputLine, AutoRun, AutoRunMode,
        AutoRunStatus, AutoStepKey, AutoStepRun, AutoStepStatus, PersistedAutoRun,
        stabilization_model::{
            PendingPushGuard, RepairKind, StabilizationBlocker, StabilizationWorkKind,
        },
    },
    config::Config,
    github::{PrCache, PrDetails, PrReviewComment, PrSummary},
    opencode::{OpencodeState, OpencodeStatus},
    plan_run::{
        PersistedPlanRun, PlanOutputKind, PlanOutputLine, PlanRun, PlanRunMode, PlanRunStatus,
        PlanStepRun, PlanStepStatus,
    },
    session::Session,
    view::{
        AutoDashboard, AutoOutputViewerState, ChoiceList, DialogLine, DialogModel, FrameModel,
        KeyChoice, PlanDashboard, PlanOutputViewerState, RepoMainView, RepoRow, StatusRow,
        WorktreeKind, WorktreeMainView, WorktreeRow,
    },
};

use crate::tui::WorktreeListMode;

use super::*;

#[test]
fn renders_wide_shell_with_sidebar_main_and_footer() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_buffer(&model, 120, 30);

    assert_region_contains(&buffer, 0..56, 0..30, "[1] Status");
    assert_region_contains(&buffer, 0..56, 0..30, "[2] Repos");
    assert_region_contains(&buffer, 0..56, 0..30, "[3] Worktrees");
    let (_, row) = sidebar_cell_containing(&buffer, "feature");
    assert_cell_style(
        &buffer,
        0,
        row,
        highlight_style()
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
    assert_region_contains(&buffer, 56..120, 0..29, "Main");
    assert_region_contains(&buffer, 0..56, 0..30, "feature");
    assert!(!line_text(&buffer, 29).contains("normal"));
}

#[test]
fn renders_narrow_shell_without_panicking() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
    let buffer = render_to_buffer(&model, 48, 12);

    assert_region_contains(&buffer, 0..48, 0..11, "[2] Repos");
    let row = find_line(&buffer, "repo  ok");
    assert_cell_style(
        &buffer,
        0,
        row,
        highlight_style()
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
    assert_region_contains(&buffer, 0..48, 0..11, "repo");
    assert!(!line_text(&buffer, 11).contains("normal"));
}

#[test]
fn renders_selected_sidebar_rows_with_focused_style() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
    let buffer = render_to_buffer(&model, 120, 30);
    let (repo_x, row) = sidebar_cell_containing(&buffer, "repo  ok");

    assert_cell_style(
        &buffer,
        0,
        row,
        highlight_style()
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        repo_x,
        row,
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(0, 64, 64))
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        55,
        row,
        highlight_style()
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );

    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_buffer(&model, 120, 30);
    let (_, row) = sidebar_cell_containing(&buffer, "feature");

    assert_cell_style(
        &buffer,
        0,
        row,
        highlight_style()
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        8,
        row,
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(0, 64, 64))
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        23,
        row,
        Style::default()
            .fg(Color::Green)
            .bg(Color::Rgb(0, 64, 64))
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        55,
        row,
        highlight_style()
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
}

#[test]
fn renders_selected_sidebar_rows_with_unfocused_style() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];

    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_buffer(&model, 120, 30);
    let (label_x, row) = sidebar_cell_containing(&buffer, "repo  ok");

    assert_cell_style(
        &buffer,
        0,
        row,
        Style::default().fg(Color::DarkGray).bg(Color::Reset),
    );
    assert_cell_style(
        &buffer,
        label_x,
        row,
        Style::default()
            .fg(Color::Reset)
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        55,
        row,
        Style::default().fg(Color::DarkGray).bg(Color::Reset),
    );

    let model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
    let buffer = render_to_buffer(&model, 120, 30);
    let (branch_x, row) = sidebar_cell_containing(&buffer, "feature");

    assert_cell_style(
        &buffer,
        0,
        row,
        Style::default().fg(Color::DarkGray).bg(Color::Reset),
    );
    assert_cell_style(
        &buffer,
        branch_x,
        row,
        Style::default()
            .fg(Color::Reset)
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    );
    assert_cell_style(
        &buffer,
        55,
        row,
        Style::default().fg(Color::DarkGray).bg(Color::Reset),
    );
}

#[test]
fn status_and_repo_sidebar_columns_align() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let mut model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
    model.status = vec![
        StatusRow {
            label: "short".to_string(),
            value: "7".to_string(),
            attention: false,
        },
        StatusRow {
            label: "long label".to_string(),
            value: "13".to_string(),
            attention: false,
        },
    ];
    model.repos = vec![
        RepoRow {
            label: "alpha".to_string(),
            root: "/repo/alpha".to_string(),
            key: None,
            health: "ok".to_string(),
            selected: true,
        },
        RepoRow {
            label: "long-repo".to_string(),
            root: "/repo/long".to_string(),
            key: Some('2'),
            health: "CI!".to_string(),
            selected: false,
        },
    ];
    let buffer = render_to_buffer(&model, 120, 30);
    let short_status_y = find_line(&buffer, "short");
    let long_status_y = find_line(&buffer, "long label");
    let alpha_y = find_line(&buffer, "alpha");
    let long_repo_y = find_line(&buffer, "long-repo");

    assert_eq!(
        line_column(&buffer, short_status_y, "7"),
        line_column(&buffer, long_status_y, "13"),
        "status values should start in the same column",
    );
    assert_eq!(
        line_column(&buffer, alpha_y, "alpha"),
        line_column(&buffer, long_repo_y, "long-repo"),
        "repo labels should not shift when the key column is empty",
    );
    assert_eq!(
        line_column(&buffer, alpha_y, "ok"),
        line_column(&buffer, long_repo_y, "CI!"),
        "repo health values should start in the same column",
    );
}

#[test]
fn renders_worktree_sidebar_metadata() {
    let mut config = test_config();
    config.worktree_columns = vec!["todo".to_string(), "owner".to_string()];
    let mut session = test_session("feature", AgentState::Running);
    session.status_label = "dirty 2 ahead 1".to_string();
    session.pr = PrCache::observed(
        test_pr_summary(),
        Some(PrDetails {
            review_comments: vec![
                PrReviewComment {
                    resolved: false,
                    body: "please fix".to_string(),
                    ..PrReviewComment::default()
                },
                PrReviewComment {
                    resolved: true,
                    body: "already handled".to_string(),
                    ..PrReviewComment::default()
                },
            ],
            ..PrDetails::default()
        }),
    );
    session
        .wt_columns
        .insert("todo".to_string(), "3".to_string());
    session
        .wt_columns
        .insert("owner".to_string(), "agent".to_string());
    session.unseen_comments = true;
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_string(&model, 160, 30);
    let style_buffer = render_to_buffer(&model, 160, 30);

    assert!(buffer.contains("branch"));
    assert!(buffer.contains("A P G C @"));
    let header_y = find_line(&style_buffer, "A P G C @");
    let at_x = line_column(&style_buffer, header_y, "@");
    assert_eq!(
        style_buffer[(at_x as u16, header_y)].style().fg,
        muted_style().fg
    );
    assert!(buffer.contains("⇄"));
    assert!(buffer.contains("✗"));
    assert!(buffer.contains("✕"));
    assert!(buffer.contains("1/1"));
    assert!(buffer.contains("todo"));
    assert!(buffer.contains("owner"));
    assert!(buffer.contains("agent"));
}

#[test]
fn worktree_sidebar_keeps_configured_columns_before_prompt_text() {
    let mut config = test_config();
    config.worktree_columns = vec!["todo".to_string(), "owner".to_string()];
    let mut session = test_session("feature", AgentState::Running);
    session
        .wt_columns
        .insert("todo".to_string(), "3".to_string());
    session
        .wt_columns
        .insert("owner".to_string(), "agent".to_string());
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_buffer(&model, 160, 30);
    let (_, row_y) = sidebar_cell_containing(&buffer, "feature");
    let row = line_text(&buffer, row_y);

    assert!(row.contains("3"), "got {row:?}");
    assert!(row.contains("agent"), "got {row:?}");
}

#[test]
fn all_worktree_mode_uses_base_columns_without_configured_columns() {
    let mut config = test_config();
    config.worktree_columns = vec!["owner".to_string()];
    let mut session = test_session("feature", AgentState::Running);
    session
        .wt_columns
        .insert("owner".to_string(), "agent".to_string());
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.worktree_list_mode = WorktreeListMode::Global;
    let buffer = render_to_string(&model, 160, 30);

    assert!(buffer.contains("repo"));
    assert!(buffer.contains("A P G C @"));
    assert!(!buffer.contains("owner"));
    assert!(!buffer.contains("agent  implement feature"));
}

#[test]
fn default_branch_row_hides_git_status_marker_but_keeps_wt_columns() {
    let mut config = test_config();
    config.worktree_columns = vec!["url".to_string()];
    let mut session = test_session("main", AgentState::Idle);
    session.status_label = "clean".to_string();
    session
        .wt_columns
        .insert("url".to_string(), "https://example.test".to_string());
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.worktrees[0].kind = WorktreeKind::DefaultBranch;
    let buffer = render_to_buffer(&model, 140, 30);
    let row = line_text(&buffer, find_line(&buffer, "https://e"));

    assert!(row.contains("https://e"), "got {row:?}");
    assert!(!row.contains("✓"), "got {row:?}");
}

#[test]
fn default_branch_row_preserves_column_alignment() {
    let mut config = test_config();
    config.worktree_columns = vec!["url".to_string()];
    let mut main = test_session("main", AgentState::Idle);
    main.wt_columns
        .insert("url".to_string(), "main-url".to_string());
    let mut feature = test_session("feature", AgentState::Running);
    feature
        .wt_columns
        .insert("url".to_string(), "feature-url".to_string());
    let sessions = vec![main, feature];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.worktrees[0].kind = WorktreeKind::DefaultBranch;
    let buffer = render_to_buffer(&model, 140, 30);
    let (main_x, main_y) = sidebar_cell_containing(&buffer, "main-url");
    let (feature_x, feature_y) = sidebar_cell_containing(&buffer, "feature-u");

    assert_eq!(
        main_x,
        feature_x,
        "default row should keep configured columns aligned\nmain: {main_row:?}\nfeature: {feature_row:?}",
        main_row = line_text(&buffer, main_y),
        feature_row = line_text(&buffer, feature_y),
    );
}

#[test]
fn renders_nerd_font_worktree_icons_when_configured() {
    let mut config = test_config();
    config.icon_style = IconStyle::NerdFont;
    let mut session = test_session("feature", AgentState::Running);
    session.status_label = "dirty 2".to_string();
    let mut summary = test_pr_summary();
    summary.check_status = "passed".to_string();
    session.pr = PrCache::observed(summary, None);
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_string(&model, 160, 30);

    assert!(buffer.contains(""));
    assert!(buffer.contains(""));
    assert!(buffer.contains(""));
}

#[test]
fn nerd_font_status_counts_have_spacing() {
    assert_eq!(
        git_status_indicator("dirty 2 ahead 1 behind 3", IconStyle::NerdFont),
        " 2 ↑1 ↓3"
    );
}

#[test]
fn nerd_font_repo_health_counts_have_spacing() {
    let spans = repo_health_spans("2 0 12", IconStyle::NerdFont);
    let text = spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(text.contains(" 2 "), "got {text:?}");
    assert!(text.contains(" 12"), "got {text:?}");
    assert!(!text.contains("2"), "got {text:?}");
}

#[test]
fn worktree_sidebar_renders_missing_configured_columns_as_placeholders() {
    let mut config = test_config();
    config.worktree_columns = vec!["todo".to_string(), "owner".to_string()];
    let mut session = test_session("feature", AgentState::Running);
    session
        .wt_columns
        .insert("todo".to_string(), "12345678901234567890".to_string());
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_string(&model, 160, 30);

    assert!(buffer.contains("12345678901~"));
    assert!(buffer.contains("·"));
}

#[test]
fn clean_worktree_git_check_is_green() {
    let session = test_session("feature", AgentState::Running);
    let worktree = WorktreeRow {
        session_index: 0,
        repo_label: "repo".to_string(),
        repo_root: "/repo".to_string(),
        worktree_path: "/repo/feature".to_string(),
        branch: session.branch.clone(),
        visibility: session.visibility,
        kind: WorktreeKind::FeatureWorktree,
        agent_state: session.agent_state,
        status_label: session.status_label.clone(),
        pr: session.pr.clone(),
        wt_columns: session.wt_columns.clone(),
        auto_status: None,
        plan_status: None,
        updated_label: "-".to_string(),
        unseen_comments: session.unseen_comments,
        prompt_summary: session.prompt_summary.clone(),
        classification: session.classification,
        selected: true,
    };

    let (label, style) = worktree_git_column(&worktree, IconStyle::Unicode);

    assert_eq!(label, "✓");
    assert_eq!(style.fg, Some(Color::Green));
}

#[test]
fn pr_merge_conflict_uses_conflict_icon() {
    let mut summary = test_pr_summary();
    summary.merge_state_status = "DIRTY".to_string();

    assert_eq!(pr_state_label(&summary), "conflict");
    assert_eq!(pr_state_icon(&summary, IconStyle::Unicode), "⚔");
    assert_eq!(pr_state_style(&summary).fg, Some(Color::Red));
}

#[test]
fn worktree_detail_omits_loaded_wt_columns() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Running);
    session
        .wt_columns
        .insert("ci.status".to_string(), "success".to_string());
    session
        .wt_columns
        .insert("vars.localdev".to_string(), "on".to_string());
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_string(&model, 140, 30);

    assert!(!buffer.contains("wt columns"));
    assert!(!buffer.contains("ci.status"));
    assert!(!buffer.contains("vars.localdev"));
}

#[test]
fn main_panel_switches_by_focus() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let status_model = test_model(&config, &sessions, PanelFocus::Status, None, None);
    let repo_model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
    let worktree_model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);

    let status_buffer = render_to_buffer(&status_model, 120, 30);
    let repo_buffer = render_to_buffer(&repo_model, 120, 30);
    let worktree_buffer = render_to_buffer(&worktree_model, 120, 30);

    assert_region_contains(&status_buffer, 56..120, 0..29, "Documentation");
    assert_region_contains(&repo_buffer, 56..120, 0..29, "view github");
    assert!(!region_text(&repo_buffer, 56..120, 0..29).contains("Preview"));
    assert_region_contains(&worktree_buffer, 56..120, 0..29, "prompt implement feature");
}

#[test]
fn sidebar_width_preserves_defaults_and_clamps_configured_width() {
    assert_eq!(sidebar_width(48, None), 20);
    assert_eq!(sidebar_width(120, None), 56);
    assert_eq!(sidebar_width(160, None), 72);
    assert_eq!(sidebar_width(120, Some(64)), 64);
    assert_eq!(sidebar_width(70, Some(64)), 50);
}

#[test]
fn renders_configured_sidebar_width() {
    let mut config = test_config();
    config.layout.sidebar_width = Some(64);
    let sessions = vec![test_session("feature", AgentState::Running)];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_buffer(&model, 120, 30);

    assert_region_contains(&buffer, 0..64, 0..30, "[3] Worktrees");
    assert_region_contains(&buffer, 0..64, 0..30, "all | repo");
    assert_region_contains(&buffer, 64..120, 0..29, "Main");
}

#[test]
fn renders_footer_status_message_and_leader_overlay() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let model = test_model(
        &config,
        &sessions,
        PanelFocus::Status,
        Some("saved config"),
        Some(ChoiceList {
            title: "Shortcuts".to_string(),
            choices: vec![
                KeyChoice::new("a", "one"),
                KeyChoice::new("b", "two"),
                KeyChoice::new("c", "three"),
            ],
        }),
    );
    let buffer = render_to_buffer(&model, 100, 20);

    assert_line_contains(&buffer, 19, "saved config");
    assert_region_contains(&buffer, 0..100, 0..20, "Shortcuts");
    assert_region_contains(&buffer, 0..100, 0..20, "[a] one");
    assert_region_contains(&buffer, 0..100, 0..20, "[b] two");
    assert_region_contains(&buffer, 0..100, 0..20, "[c] three");
    assert_ne!(find_line(&buffer, "[a]"), find_line(&buffer, "[b]"));
    assert_ne!(find_line(&buffer, "[b]"), find_line(&buffer, "[c]"));
}

#[test]
fn renders_dialog_overlays() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let mut model = test_model(&config, &sessions, PanelFocus::Status, None, None);
    model.dialog = Some(DialogModel::Prompt {
        title: "Search Repositories".to_string(),
        prompt: "Filter: ".to_string(),
        input: "api".to_string(),
    });
    let buffer = render_to_string(&model, 80, 20);

    assert!(buffer.contains("Search Repositories"));
    assert!(buffer.contains("Filter: api"));
    assert!(buffer.contains("Enter to continue"));

    model.dialog = Some(DialogModel::Choice {
        choices: ChoiceList {
            title: "Plan Actions".to_string(),
            choices: vec![
                KeyChoice::new("u", "pause/resume"),
                KeyChoice::disabled("f", "retry failed"),
            ],
        },
    });
    let buffer = render_to_buffer(&model, 80, 20);
    let buffer_text = buffer_to_string(&buffer);

    assert!(buffer_text.contains("Plan Actions"));
    assert!(buffer_text.contains("[u] pause/resume"));
    assert!(buffer_text.contains("[f] retry failed"));
    assert_ne!(find_line(&buffer, "[u]"), find_line(&buffer, "[f]"));

    let lines = dialog_lines(model.dialog.as_ref().unwrap());
    assert_eq!(lines[0].spans[0].content.as_ref(), "[u]");
    assert_eq!(
        lines[0].spans[0].style,
        Style::default()
            .fg(Color::Black)
            .bg(highlight_color())
            .add_modifier(Modifier::BOLD)
    );
    assert_eq!(lines[1].spans[0].style.fg, Some(Color::DarkGray));
    assert_eq!(lines[1].spans[1].style.fg, Some(Color::DarkGray));

    model.dialog = Some(DialogModel::Confirm {
        title: "Delete Session".to_string(),
        lines: vec![DialogLine {
            text: "dirty worktree\nremove local state".to_string(),
            attention: true,
        }],
        prompt: "Delete this session?".to_string(),
        input: "n".to_string(),
        default: false,
        invalid: false,
    });
    let buffer = render_to_string(&model, 80, 20);

    assert!(buffer.contains("Delete Session"));
    assert!(buffer.contains("dirty worktree"));
    assert!(buffer.contains("remove local state"));
    assert!(buffer.contains("Delete this session? [y/N]: n"));
    let lines = dialog_lines(model.dialog.as_ref().unwrap());
    assert_eq!(
        lines.last().unwrap().to_string(),
        "Delete this session? [y/N]: n"
    );

    model.dialog = Some(DialogModel::Confirm {
        title: "Plan Run: Execution".to_string(),
        lines: vec![],
        prompt: "Run steps in parallel?".to_string(),
        input: String::new(),
        default: false,
        invalid: false,
    });
    let buffer = render_to_string(&model, 80, 20);

    assert!(buffer.contains("Plan Run: Execution"));
    assert!(buffer.contains("Run steps in parallel? [y/N]:"));

    model.dialog = Some(DialogModel::Confirm {
        title: "Pull Default Branch".to_string(),
        lines: vec![],
        prompt: "Pull first?".to_string(),
        input: String::new(),
        default: true,
        invalid: true,
    });
    let buffer = render_to_string(&model, 80, 20);

    assert!(buffer.contains("Pull first? [Y/n]:"));
    assert!(buffer.contains("Please enter y or n."));

    let lines = dialog_lines(model.dialog.as_ref().unwrap());
    assert_eq!(lines[0].to_string(), "Pull first? [Y/n]: ");
    assert_eq!(lines[1].to_string(), "Please enter y or n.");
    assert_eq!(lines[1].spans[0].style, attention_style());

    model.dialog = Some(DialogModel::OrderedToggle {
        title: "Visible Fields".to_string(),
        items: vec![OrderedToggleItem {
            id: "internal-id".to_string(),
            label: "Display label".to_string(),
            enabled: true,
        }],
        selected: 0,
    });
    let buffer = render_to_string(&model, 80, 20);

    assert!(buffer.contains("Visible Fields"));
    assert!(buffer.contains("[x]"));
    assert!(buffer.contains("Display label"));
    assert!(buffer.contains("J/K move"));
}

#[test]
fn worktree_info_dialog_groups_agent_with_dense_columns() {
    let rows = keybinding_info_lines(PanelFocus::Worktrees, IconStyle::Unicode)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let compact_header = rows
        .iter()
        .position(|line| line.contains("visibility") && line.contains("kind"))
        .expect("compact worktree header");
    let dense_header = rows
        .iter()
        .position(|line| {
            line.contains("agent")
                && line.contains("PR")
                && line.contains("git")
                && line.contains("CI")
        })
        .expect("dense worktree header");

    assert!(!rows[compact_header].contains("agent"));
    assert!(dense_header > compact_header);
    assert!(rows[dense_header + 1].contains("idle"));
    assert!(rows[dense_header + 1].contains("open"));
}

#[test]
fn prompt_dialog_sets_cursor_at_end_of_input() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let mut model = test_model(&config, &sessions, PanelFocus::Status, None, None);
    model.dialog = Some(DialogModel::Prompt {
        title: "Search Repositories".to_string(),
        prompt: "Filter: ".to_string(),
        input: "api".to_string(),
    });
    let backend = render_to_backend(&model, 80, 20);

    assert!(backend.cursor_visible());
    assert_eq!(backend.cursor_position(), Position::new(26, 8));

    model.dialog = None;
    let backend = render_to_backend(&model, 80, 20);

    assert!(!backend.cursor_visible());
}

#[test]
fn prompt_dialog_geometry_is_stable_and_tail_truncates_input() {
    let area = Rect::new(0, 0, 80, 20);
    let short = DialogModel::Prompt {
        title: "Search Repositories".to_string(),
        prompt: "Filter: ".to_string(),
        input: String::new(),
    };
    let long = DialogModel::Prompt {
        title: "Search Repositories".to_string(),
        prompt: "Filter: ".to_string(),
        input: "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ".to_string(),
    };

    assert_eq!(dialog_geometry(area, &short), dialog_geometry(area, &long));
    let visible = dialog_lines(&long)[0].to_string();

    assert!(visible.contains("ghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"));
    assert!(!visible.contains("abcdef"));
}

#[test]
fn renders_plan_dashboard_compact_step_tails() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.plan_dashboard = Some(test_plan_dashboard(false));
    let buffer = render_to_string(&model, 120, 37);

    assert!(buffer.contains("Plan Run"));
    assert!(buffer.contains("current"));
    assert!(buffer.contains("Steps"));
    assert!(buffer.contains("[-] Step 1"));
    assert!(buffer.contains("command output"));
    assert!(!buffer.contains("Output"));
    assert!(!buffer.contains("[+]"));
    assert!(!buffer.contains("L2"));
}

#[test]
fn renders_plan_dashboard_ignores_output_block_expansion() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.plan_dashboard = Some(test_plan_dashboard(true));
    let buffer = render_to_string(&model, 120, 32);

    assert!(buffer.contains("command output"));
    assert!(!buffer.contains("running command"));
}

#[test]
fn renders_plan_run_window_around_selected_run() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let mut dashboard = test_plan_dashboard(false);
    dashboard.runs = (1..=8)
        .map(|index| crate::view::PlanRunSummary {
            id: format!("plan-run-{index}"),
            plan_display: format!("plan-{index}.md"),
            scope_path: "/repo".to_string(),
            status: PlanRunStatus::Done,
            updated_unix_ms: 4_000 + index,
            selected: index == 7,
        })
        .collect();
    model.plan_dashboard = Some(dashboard);

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("plan-7.md"));
    assert!(buffer.contains("▶ done"));
    assert!(!buffer.contains("plan-1.md"));
}

#[test]
fn renders_auto_dashboard_steps_and_output_cursor() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(test_auto_dashboard());
    let buffer = render_to_string(&model, 120, 32);

    assert!(!buffer.contains("Auto Flow"));
    assert!(!buffer.contains("auto output"));
}

#[test]
fn worktree_main_panel_renders_five_agent_messages_without_indenting_user_message() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Running);
    session.opencode_status = Some(OpencodeStatus {
        server_url: None,
        session_id: None,
        title: None,
        state: OpencodeState::Busy,
        detail: None,
        latest_message: Some("implementing the panel".to_string()),
        latest_user_message: Some("please update the panel".to_string()),
        recent_messages: vec![
            "third message".to_string(),
            "second message".to_string(),
            "first message".to_string(),
            "older message".to_string(),
            "oldest message".to_string(),
        ],
        active_tool: Some("bash".to_string()),
        todos: Vec::new(),
        last_updated_unix_ms: None,
    });
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let lines = worktree_detail_lines(&model);
    let agent = lines
        .iter()
        .position(|line| line.to_string().contains("Agent"))
        .unwrap();

    assert!(lines[agent + 1].to_string().contains("● busy  bash"));
    assert!(lines[agent + 2].to_string().contains("user"));
    assert!(lines[agent + 2].to_string().starts_with("user please"));
    assert!(
        lines[agent + 2]
            .to_string()
            .contains("please update the panel")
    );
    assert_eq!(lines[agent + 2].spans[1].style.fg, Some(Color::White));
    assert_eq!(lines[agent + 3].to_string(), "third message");
    assert_eq!(lines[agent + 4].to_string(), "second message");
    assert_eq!(lines[agent + 5].to_string(), "first message");
    assert_eq!(lines[agent + 6].to_string(), "older message");
    assert_eq!(lines[agent + 7].to_string(), "oldest message");
    assert!(lines[agent + 8].to_string().is_empty());
}

#[test]
fn worktree_main_panel_renders_idle_status_and_reserves_five_message_lines() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let lines = worktree_detail_lines(&model);
    let agent = lines
        .iter()
        .position(|line| line.to_string().contains("Agent"))
        .unwrap();

    assert!(lines[agent + 1].to_string().contains("○ idle"));
    assert!(lines[agent + 2].to_string().contains("user"));
    assert!(lines[agent + 3].to_string().is_empty());
    assert!(lines[agent + 4].to_string().is_empty());
    assert!(lines[agent + 5].to_string().is_empty());
    assert!(lines[agent + 6].to_string().is_empty());
    assert!(lines[agent + 7].to_string().is_empty());
}

#[test]
fn worktree_main_panel_renders_unknown_opencode_status_as_needing_restart() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Running);
    session.opencode_status = Some(OpencodeStatus {
        server_url: None,
        session_id: None,
        title: None,
        state: OpencodeState::Unknown,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: None,
        todos: Vec::new(),
        last_updated_unix_ms: None,
    });
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let lines = worktree_detail_lines(&model);
    let agent = lines
        .iter()
        .position(|line| line.to_string().contains("Agent"))
        .unwrap();

    assert!(lines[agent + 1].to_string().contains("↻ needs restart"));
}

#[test]
fn worktree_main_panel_renders_unknown_status_with_active_tool_as_running() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Running);
    session.opencode_status = Some(OpencodeStatus {
        server_url: None,
        session_id: None,
        title: None,
        state: OpencodeState::Unknown,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: Some("bash running".to_string()),
        todos: Vec::new(),
        last_updated_unix_ms: None,
    });
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let lines = worktree_detail_lines(&model);
    let agent = lines
        .iter()
        .position(|line| line.to_string().contains("Agent"))
        .unwrap();

    assert!(
        lines[agent + 1]
            .to_string()
            .contains("● running  bash running")
    );
}

#[test]
fn worktree_main_panel_renders_idle_status_with_active_tool_as_running() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::ExitedOk);
    session.opencode_status = Some(OpencodeStatus {
        server_url: None,
        session_id: None,
        title: None,
        state: OpencodeState::Idle,
        detail: None,
        latest_message: None,
        latest_user_message: None,
        recent_messages: Vec::new(),
        active_tool: Some("task running".to_string()),
        todos: Vec::new(),
        last_updated_unix_ms: None,
    });
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let lines = worktree_detail_lines(&model);
    let agent = lines
        .iter()
        .position(|line| line.to_string().contains("Agent"))
        .unwrap();

    assert!(
        lines[agent + 1]
            .to_string()
            .contains("● running  task running")
    );
}

#[test]
fn worktree_main_panel_renders_opencode_workflow_states() {
    let config = test_config();
    for (state, expected) in [
        (OpencodeState::Retry, "↻ retrying"),
        (OpencodeState::Idle, "○ ready"),
        (OpencodeState::Done, "✓ done"),
        (OpencodeState::NeedsInput, "! needs input"),
        (OpencodeState::Error, "✕ failed"),
    ] {
        let mut session = test_session("feature", AgentState::Running);
        session.opencode_status = Some(OpencodeStatus {
            server_url: None,
            session_id: None,
            title: None,
            state,
            detail: None,
            latest_message: None,
            latest_user_message: None,
            recent_messages: Vec::new(),
            active_tool: None,
            todos: Vec::new(),
            last_updated_unix_ms: None,
        });
        let sessions = vec![session];
        let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        let lines = worktree_detail_lines(&model);
        let status = lines
            .iter()
            .find(|line| line.to_string().starts_with("status "))
            .unwrap();
        assert!(status.to_string().contains(expected));
    }
}

#[test]
fn worktree_main_panel_renders_pr_comments_table() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    session.pr = PrCache::observed(
        test_pr_summary(),
        Some(PrDetails {
            review_comments: vec![PrReviewComment {
                author: "reviewer".to_string(),
                body: "please fix".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        }),
    );
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);

    let buffer = render_to_string(&model, 120, 30);

    assert!(buffer.contains("kind   res author       text"));
    assert!(buffer.contains("inline"));
    assert!(buffer.contains("no"));
    assert!(buffer.contains("please fix"));
}

#[test]
fn worktree_main_panel_styles_open_and_merged_pr_numbers() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    session.pr = PrCache::observed(test_pr_summary(), None);
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let open = render_to_buffer(&model, 120, 30);
    let open_row = find_line(&open, "⇄ #42");
    let open_x = (open.area.x..open.area.x + open.area.width)
        .find(|x| open[(*x, open_row)].symbol() == "⇄")
        .unwrap();
    assert_eq!(open[(open_x, open_row)].style().fg, Some(Color::Green));

    let mut session = test_session("feature", AgentState::Idle);
    let mut summary = test_pr_summary();
    summary.merged = true;
    summary.state = "MERGED".to_string();
    session.pr = PrCache::observed(summary, None);
    let sessions = vec![session];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let merged = render_to_buffer(&model, 120, 30);
    let merged_row = find_line(&merged, "⋈ #42");
    let merged_x = (merged.area.x..merged.area.x + merged.area.width)
        .find(|x| merged[(*x, merged_row)].symbol() == "⋈")
        .unwrap();
    assert_eq!(
        merged[(merged_x, merged_row)].style().fg,
        Some(Color::Magenta)
    );
}

#[test]
fn renders_stabilization_pending_push_in_worktree_main_panel() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    session.pr = PrCache::observed(test_pr_summary(), None);
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::PendingPush,
        StabilizationWorkKind::PushPendingRepair,
        Some(test_pending_push_guard()),
    ));

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("PR"));
    assert!(buffer.contains("pr #"));
    assert!(buffer.contains("42"));
    assert!(buffer.contains("name"));
    assert!(buffer.contains("Feature PR"));
    assert!(buffer.contains("merge"));
    assert!(buffer.contains("ci"));
    assert!(buffer.contains("review"));
    assert!(!buffer.contains("pending push"));
    assert!(!buffer.contains("guard"));
    assert!(buffer.contains("state"));
    assert!(buffer.contains("PendingPush"));
    assert!(buffer.contains("next"));
    assert!(buffer.contains("PushPendingRepair"));
    assert!(!buffer.contains("state PendingPush"));
    assert!(!buffer.contains("gate"));
}

#[test]
fn worktree_main_panel_renders_review_gate() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    let mut summary = test_pr_summary();
    summary.requested_reviewers = vec!["review-team".to_string()];
    session.pr = PrCache::observed(summary, None);
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::ReadyForManualMerge,
        StabilizationWorkKind::MarkReadyForManualMerge,
        None,
    ));

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("review"));
    assert!(buffer.contains("pending"));
}

#[test]
fn renders_stabilization_ci_failed_in_worktree_main_panel() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    session.pr = PrCache::observed(test_pr_summary(), None);
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::CiFailed,
        StabilizationWorkKind::FixCi,
        None,
    ));

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("state"));
    assert!(buffer.contains("CiFailed"));
    assert!(buffer.contains("next"));
    assert!(buffer.contains("FixCi"));
}

#[test]
fn renders_stabilization_merge_blocked_in_worktree_main_panel() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    let mut summary = test_pr_summary();
    summary.check_status = "passed".to_string();
    summary.merge_state_status = "DIRTY".to_string();
    session.pr = PrCache::observed(summary, None);
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::MergeBlocked,
        StabilizationWorkKind::Escalate,
        None,
    ));

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("merge"));
    assert!(buffer.contains("blocked"));
    assert!(buffer.contains("state"));
    assert!(buffer.contains("MergeBlocked"));
    assert!(buffer.contains("next"));
    assert!(buffer.contains("Escalate"));
}

#[test]
fn renders_stabilization_policy_unknown_in_worktree_main_panel() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    let mut summary = test_pr_summary();
    summary.check_status = "passed".to_string();
    session.pr = PrCache::observed(summary, None);
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::PolicyUnknown,
        StabilizationWorkKind::Escalate,
        None,
    ));

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("policy"));
    assert!(buffer.contains("unknown"));
    assert!(buffer.contains("state"));
    assert!(buffer.contains("PolicyUnknown"));
    assert!(buffer.contains("next"));
    assert!(buffer.contains("Escalate"));
}

#[test]
fn renders_stabilization_ready_for_manual_merge_in_worktree_main_panel() {
    let config = test_config();
    let mut session = test_session("feature", AgentState::Idle);
    let mut summary = test_pr_summary();
    summary.check_status = "failed".to_string();
    summary.review_decision = "APPROVED".to_string();
    session.pr = PrCache::observed(
        summary,
        Some(PrDetails {
            failing_checks: vec!["docs".to_string()],
            ..PrDetails::default()
        }),
    );
    let sessions = vec![session];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::ReadyForManualMerge,
        StabilizationWorkKind::MarkReadyForManualMerge,
        None,
    ));

    let buffer = render_to_string(&model, 120, 40);

    assert!(buffer.contains("state"));
    assert!(buffer.contains("ReadyForManualMerge"));
    assert!(buffer.contains("next"));
    assert!(buffer.contains("MarkReadyForManualMerge"));
}

#[test]
fn worktree_main_panel_hides_pr_section_without_pr() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    let buffer = render_to_string(&model, 120, 30);

    for key in [
        "PR", "pr #", "name", "state", "next", "ci", "review", "merge", "policy",
    ] {
        assert!(!buffer.contains(key), "unexpected PR key: {key}");
    }
    assert!(!buffer.contains("No PR detected"));
    assert!(!buffer.contains("Description"));
    assert!(!buffer.contains("Activity"));
    assert!(!buffer.contains("refreshed"));
}

#[test]
fn worktree_main_panel_hides_stabilization_values_without_pr() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Idle)];
    let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
    model.auto_dashboard = Some(stabilization_dashboard(
        StabilizationBlocker::NeedsPullRequest,
        StabilizationWorkKind::PushInitialAndOpenPr,
        Some(test_pending_push_guard()),
    ));
    let buffer = render_to_string(&model, 120, 30);

    assert!(!buffer.contains("NeedsPullRequest"));
    assert!(!buffer.contains("PushInitialAndOpenPr"));
    assert!(!buffer.contains("review repair"));
}

#[test]
fn main_panel_applies_scroll_for_every_focus() {
    let config = test_config();
    let sessions = vec![test_session("feature", AgentState::Running)];

    for focus in [PanelFocus::Status, PanelFocus::Repos, PanelFocus::Worktrees] {
        let mut model = test_model(&config, &sessions, focus, None, None);
        let unscrolled = render_to_string(&model, 120, 6);
        model.main_scroll = 2;
        let scrolled = render_to_string(&model, 120, 6);
        assert_ne!(
            unscrolled, scrolled,
            "main panel did not scroll for {focus:?}"
        );
    }
}

fn render_to_string(model: &FrameModel<'_>, cols: u16, rows: u16) -> String {
    buffer_to_string(&render_to_buffer(model, cols, rows))
}

fn render_to_buffer(model: &FrameModel<'_>, cols: u16, rows: u16) -> Buffer {
    render_to_backend(model, cols, rows).buffer().clone()
}

fn render_to_backend(model: &FrameModel<'_>, cols: u16, rows: u16) -> TestBackend {
    let backend = TestBackend::new(cols, rows);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|frame| render(frame, model)).expect("draw");
    terminal.backend().clone()
}

fn buffer_to_string(buffer: &Buffer) -> String {
    buffer.content().iter().map(|cell| cell.symbol()).collect()
}

fn line_text(buffer: &Buffer, y: u16) -> String {
    (buffer.area.x..buffer.area.x + buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn region_text(
    buffer: &Buffer,
    x_range: std::ops::Range<u16>,
    y_range: std::ops::Range<u16>,
) -> String {
    y_range
        .flat_map(|y| x_range.clone().map(move |x| buffer[(x, y)].symbol()))
        .collect()
}

fn assert_line_contains(buffer: &Buffer, y: u16, expected: &str) {
    let line = line_text(buffer, y);
    assert!(
        line.contains(expected),
        "expected line {y} to contain {expected:?}, got {line:?}"
    );
}

fn assert_region_contains(
    buffer: &Buffer,
    x_range: std::ops::Range<u16>,
    y_range: std::ops::Range<u16>,
    expected: &str,
) {
    let text = region_text(buffer, x_range, y_range);
    assert!(
        text.contains(expected),
        "expected region to contain {expected:?}, got {text:?}"
    );
}

fn assert_cell_style(buffer: &Buffer, x: u16, y: u16, expected: Style) {
    let actual = buffer[(x, y)].style();
    assert_eq!(actual.fg, expected.fg, "unexpected fg at ({x}, {y})");
    assert_eq!(actual.bg, expected.bg, "unexpected bg at ({x}, {y})");
    assert_eq!(
        actual.add_modifier, expected.add_modifier,
        "unexpected modifiers at ({x}, {y})"
    );
}

fn find_line(buffer: &Buffer, expected: &str) -> u16 {
    (buffer.area.y..buffer.area.y + buffer.area.height)
        .find(|&y| line_text(buffer, y).contains(expected))
        .unwrap_or_else(|| panic!("expected buffer to contain line fragment {expected:?}"))
}

fn line_column(buffer: &Buffer, y: u16, expected: &str) -> usize {
    line_text(buffer, y)
        .find(expected)
        .unwrap_or_else(|| panic!("expected line {y} to contain {expected:?}"))
}

fn sidebar_cell_containing(buffer: &Buffer, expected: &str) -> (u16, u16) {
    let expected = expected.chars().collect::<Vec<_>>();
    for y in buffer.area.y..buffer.area.y + buffer.area.height {
        for x in buffer.area.x..buffer.area.x + buffer.area.width.min(56) {
            if expected.iter().enumerate().all(|(offset, expected)| {
                let x = x + offset as u16;
                x < buffer.area.x + buffer.area.width
                    && buffer[(x, y)].symbol() == expected.to_string()
            }) {
                return (x, y);
            }
        }
    }
    panic!("expected sidebar to contain line fragment {expected:?}")
}

fn test_config() -> Config {
    let mut config = crate::test_support::test_config();
    config.default_agent = "opencode".to_string();
    config.default_base = Some("main".to_string());
    config
}

fn test_session(branch: &str, agent_state: AgentState) -> Session {
    Session {
        repo_index: 0,
        repo_label: "repo".to_string(),
        repo_key: Some('1'),
        path: PathBuf::from(format!("/repo/{branch}")),
        incarnation: String::new(),
        path_display: format!("/repo/{branch}"),
        branch: branch.to_string(),
        prompt_summary: "implement feature".to_string(),
        classification: crate::session::SessionClassification::Work,
        visibility: 0,
        adopted: false,
        hidden: false,
        status_label: "clean".to_string(),
        agent_state,
        opencode_status: None,
        pr: PrCache::default(),
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}

fn test_pr_summary() -> PrSummary {
    PrSummary {
        number: 42,
        title: "Feature PR".to_string(),
        body: String::new(),
        url: "https://example.test/pr/42".to_string(),
        state: "OPEN".to_string(),
        review_decision: "REVIEW_REQUIRED".to_string(),
        requested_reviewers: Vec::new(),
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: "abc123".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        check_status: "failed".to_string(),
        merge_state_status: "CLEAN".to_string(),
        comment_count: 5,
        merged: false,
        draft: false,
    }
}

fn test_model<'a>(
    config: &'a Config,
    sessions: &'a [Session],
    focus: PanelFocus,
    status_message: Option<&'a str>,
    leader_hint: Option<crate::view::LeaderHintModel>,
) -> FrameModel<'a> {
    FrameModel {
        config,
        sessions,
        status: vec![StatusRow {
            label: "repos".to_string(),
            value: "1".to_string(),
            attention: false,
        }],
        repos: vec![RepoRow {
            label: "repo".to_string(),
            root: "/repo".to_string(),
            key: Some('1'),
            health: "ok".to_string(),
            selected: true,
        }],
        worktrees: sessions
            .iter()
            .enumerate()
            .map(|(index, session)| WorktreeRow {
                session_index: index,
                repo_label: "repo".to_string(),
                repo_root: "/repo".to_string(),
                worktree_path: session.path_display.clone(),
                branch: session.branch.clone(),
                visibility: session.visibility,
                kind: WorktreeKind::FeatureWorktree,
                agent_state: session.agent_state,
                status_label: session.status_label.clone(),
                pr: session.pr.clone(),
                wt_columns: session.wt_columns.clone(),
                auto_status: None,
                plan_status: None,
                updated_label: "-".to_string(),
                unseen_comments: session.unseen_comments,
                prompt_summary: session.prompt_summary.clone(),
                classification: session.classification,
                selected: index == 0,
            })
            .collect(),
        current_repo_index: 0,
        selected_repo_label: "repo".to_string(),
        selected_repo_root: "/repo".to_string(),
        selected_session: Some(0),
        selected_comment: 0,
        focus,
        main_focused: false,
        main_scroll: 0,
        repo_main_view: RepoMainView::Github,
        worktree_main_view: WorktreeMainView::Details,
        worktree_list_mode: WorktreeListMode::Repo,
        mode_label: "normal",
        status_message,
        repo_filter: "",
        worktree_filter: "",
        leader_hint,
        auto_dashboard: None,
        plan_dashboard: None,
        dialog: None,
    }
}

fn test_plan_dashboard(expanded: bool) -> PlanDashboard {
    let mut expanded_blocks = BTreeSet::new();
    if expanded {
        expanded_blocks.insert("tool:build".to_string());
    }
    PlanDashboard {
        run: PersistedPlanRun {
            run: PlanRun {
                harness_id: "opencode".to_string(),
                adapter_id: "opencode".to_string(),
                id: "plan-run".to_string(),
                repo_root: "/repo".to_string(),
                scope_path: PathBuf::from("/repo"),
                plan_path: PathBuf::from("plan.md"),
                plan_display: "plan.md".to_string(),
                step_name: "phase".to_string(),
                start_step: 1,
                total_steps: 2,
                mode: PlanRunMode::Sequential,
                status: PlanRunStatus::Running,
                pause_requested: false,
                selected_step: 1,
                created_unix_ms: 1_000,
                updated_unix_ms: 4_000,
                archived_unix_ms: None,
            },
            steps: vec![
                PlanStepRun {
                    run_id: "plan-run".to_string(),
                    step: 1,
                    prompt: "do phase one".to_string(),
                    status: PlanStepStatus::Running,
                    execution: crate::harness::ExecutionRef {
                        state: Some("busy".to_string()),
                        process_id: None,
                        process_start_time_ticks: None,
                    },
                    session: crate::harness::SessionRef {
                        adapter_id: Some("opencode".to_string()),
                        endpoint: None,
                        id: Some("abcdefgh1234".to_string()),
                    },
                    agent_variant: Some("medium".to_string()),
                    started_unix_ms: Some(1_000),
                    finished_unix_ms: None,
                    exit_code: None,
                    latest_message: Some("working".to_string()),
                    active_tool: Some("bash".to_string()),
                    todos: Vec::new(),
                    summary: None,
                    error: None,
                },
                PlanStepRun {
                    run_id: "plan-run".to_string(),
                    step: 2,
                    prompt: "do phase two".to_string(),
                    status: PlanStepStatus::Queued,
                    execution: crate::harness::ExecutionRef::default(),
                    session: crate::harness::SessionRef::default(),
                    agent_variant: None,
                    started_unix_ms: None,
                    finished_unix_ms: None,
                    exit_code: None,
                    latest_message: None,
                    active_tool: None,
                    todos: Vec::new(),
                    summary: None,
                    error: None,
                },
            ],
        },
        runs: vec![crate::view::PlanRunSummary {
            id: "plan-run".to_string(),
            plan_display: "plan.md".to_string(),
            scope_path: "/repo".to_string(),
            status: PlanRunStatus::Running,
            updated_unix_ms: 4_000,
            selected: true,
        }],
        output_lines: vec![
            PlanOutputLine {
                run_id: "plan-run".to_string(),
                step: 1,
                line_number: 1,
                time_unix_ms: 1_000,
                kind: PlanOutputKind::Assistant,
                text: "starting".to_string(),
                block_id: None,
            },
            PlanOutputLine {
                run_id: "plan-run".to_string(),
                step: 1,
                line_number: 2,
                time_unix_ms: 2_000,
                kind: PlanOutputKind::Tool,
                text: "running command".to_string(),
                block_id: Some("build".to_string()),
            },
            PlanOutputLine {
                run_id: "plan-run".to_string(),
                step: 1,
                line_number: 3,
                time_unix_ms: 3_000,
                kind: PlanOutputKind::ToolOutput,
                text: "command output".to_string(),
                block_id: Some("build".to_string()),
            },
        ],
        output_state: PlanOutputViewerState {
            cursor: 1,
            follow: false,
            expanded_blocks,
        },
    }
}

fn test_auto_dashboard() -> AutoDashboard {
    AutoDashboard {
        run: PersistedAutoRun {
            run: AutoRun {
                harness_id: "opencode".to_string(),
                adapter_id: "opencode".to_string(),
                id: "auto-run".to_string(),
                repo_root: "/repo".to_string(),
                worktree_path: PathBuf::from("/repo/feature"),
                worktree_incarnation: None,
                branch: "feature".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::Prompt,
                plan_path: None,
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                prompt_summary: "implement feature".to_string(),
                initial_prompt: "implement feature".to_string(),
                status: AutoRunStatus::Running,
                pause_requested: false,
                selected_step_run_id: Some(10),
                pr_number: Some(42),
                pr_url: None,
                current_head_sha: None,
                review_baseline_json: None,
                stabilization_status: None,
                stabilization_blocker: None,
                stabilization_next_work: None,
                pending_push: None,
                created_unix_ms: 1_000,
                updated_unix_ms: 3_000,
                archived_unix_ms: None,
            },
            steps: vec![AutoStepRun {
                id: Some(10),
                run_id: "auto-run".to_string(),
                sequence: 1,
                step_key: AutoStepKey::Implement,
                reason: None,
                status: AutoStepStatus::Running,
                attempt: 1,
                started_unix_ms: Some(1_000),
                finished_unix_ms: None,
                execution: crate::harness::ExecutionRef::default(),
                session: crate::harness::SessionRef {
                    adapter_id: Some("opencode".to_string()),
                    endpoint: None,
                    id: Some("abcdefgh1234".to_string()),
                },
                plan_run_id: None,
                commit_sha: None,
                head_sha: None,
                work_guard: None,
                blocker: None,
                summary: Some("working".to_string()),
                error: None,
            }],
        },
        linked_plan_dashboard: None,
        output_lines: vec![AutoOutputLine {
            step_run_id: 10,
            line_number: 1,
            time_unix_ms: 2_000,
            kind: AutoOutputKind::Status,
            text: "auto output".to_string(),
            block_id: None,
        }],
        output_state: AutoOutputViewerState {
            cursor: 0,
            follow: true,
        },
    }
}

fn stabilization_dashboard(
    blocker: StabilizationBlocker,
    next_work: StabilizationWorkKind,
    pending_push: Option<PendingPushGuard>,
) -> AutoDashboard {
    let mut dashboard = test_auto_dashboard();
    dashboard.run.run.stabilization_blocker = Some(blocker);
    dashboard.run.run.stabilization_next_work = Some(next_work);
    dashboard.run.run.pending_push = pending_push;
    dashboard
}

fn test_pending_push_guard() -> PendingPushGuard {
    PendingPushGuard {
        repair_kind: RepairKind::Ci,
        commit_sha: "fedcba9876543210".to_string(),
        expected_local_head_sha: "abc1234567890000".to_string(),
        expected_remote_head_sha: Some("1234567890abcdef".to_string()),
        pr_number: Some(42),
        expected_pr_head_sha: Some("9999999999999999".to_string()),
        expected_base_sha: Some("def5678901234567".to_string()),
        guarded_review_thread_ids: Vec::new(),
    }
}
