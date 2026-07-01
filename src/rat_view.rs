use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    agent::AgentState,
    auto_flow::{
        AutoImplementationSource, AutoOutputKind, AutoRunMode, AutoRunStatus, AutoStepKey,
        AutoStepRun, AutoStepStatus,
    },
    opencode::OpencodeStatus,
    plan_run::{
        PlanOutputKind, PlanOutputLine, PlanRunMode, PlanRunStatus, PlanStepRun, PlanStepStatus,
        plan_output_block_key,
    },
    session::Session,
    tui::PanelFocus,
    util::{status_count, truncate},
    view,
};

pub(crate) fn render(frame: &mut Frame<'_>, model: &view::FrameModel<'_>) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(area.width)),
            Constraint::Min(20),
        ])
        .split(vertical[0]);

    render_sidebar(frame, body[0], model);
    render_main(frame, body[1], model);
    render_footer(frame, vertical[1], model);
    if let Some(hint) = &model.leader_hint {
        render_leader_hint(frame, area, hint);
    }
    if let Some(dialog) = &model.dialog {
        render_dialog(frame, area, dialog);
    }
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, model: &view::FrameModel<'_>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Percentage(40),
            Constraint::Percentage(60),
        ])
        .split(area);
    render_status(frame, chunks[0], model);
    render_repos(frame, chunks[1], model);
    render_worktrees(frame, chunks[2], model);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, model: &view::FrameModel<'_>) {
    let rows = if model.status.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "no status",
            muted_style(),
        )))]
    } else {
        model
            .status
            .iter()
            .map(|row| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", row.label), muted_style()),
                    Span::styled(
                        row.value.clone(),
                        if row.attention {
                            attention_style()
                        } else {
                            Style::default()
                        },
                    ),
                ]))
            })
            .collect()
    };
    let focused = model.focus == PanelFocus::Status;
    let title = panel_title("1", "Status", focused);
    frame.render_widget(List::new(rows).block(panel_block(title, focused)), area);
}

fn render_repos(frame: &mut Frame<'_>, area: Rect, model: &view::FrameModel<'_>) {
    let rows = if model.repos.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if model.repo_filter.is_empty() {
                "no repositories"
            } else {
                "no repository matches"
            },
            muted_style(),
        )))]
    } else {
        model
            .repos
            .iter()
            .map(|repo| {
                let key = repo.key.map(|key| format!("{key} ")).unwrap_or_default();
                let line = Line::from(vec![
                    Span::styled(key, muted_style()),
                    Span::raw(repo.label.clone()),
                    Span::styled(format!("  {}", repo.health), health_style(&repo.health)),
                ]);
                ListItem::new(line).style(if repo.selected {
                    selected_style(model.focus == PanelFocus::Repos)
                } else {
                    Style::default()
                })
            })
            .collect()
    };
    let focused = model.focus == PanelFocus::Repos;
    let mut title = panel_title("2", "Repos", focused);
    if !model.repo_filter.is_empty() {
        title.push_span(Span::styled(
            format!(" /{}", model.repo_filter),
            muted_style(),
        ));
    }
    frame.render_widget(List::new(rows).block(panel_block(title, focused)), area);
}

fn render_worktrees(frame: &mut Frame<'_>, area: Rect, model: &view::FrameModel<'_>) {
    let rows = if model.worktrees.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if model.worktree_filter.is_empty() {
                "no worktrees"
            } else {
                "no worktree matches"
            },
            muted_style(),
        )))]
    } else {
        let mut rows = vec![ListItem::new(Line::from(vec![
            Span::styled(format!("{:<12} ", "branch"), muted_style()),
            Span::styled("A ", muted_style()),
            Span::styled("P ", muted_style()),
            Span::styled("G ", muted_style()),
            Span::styled("C ", muted_style()),
            Span::styled(format!("{:<5} ", "@"), muted_style()),
            Span::styled("!", muted_style()),
        ]))];
        rows.extend(model.worktrees.iter().map(|worktree| {
            let (pr_label, pr_style) = worktree_pr_column(worktree);
            let (git_label, git_style) = worktree_git_column(worktree);
            let (ci_label, ci_style) = worktree_ci_column(worktree);
            let (comments_label, comments_style) = worktree_comments_column(worktree);
            let (error_label, error_style) = worktree_error_column(worktree);
            let mut spans = vec![
                Span::raw(format!("{:<12} ", truncate_column(&worktree.branch, 12))),
                Span::styled(
                    format!("{} ", agent_icon(worktree.agent_state)),
                    agent_style(worktree.agent_state),
                ),
                Span::styled(format!("{pr_label} "), pr_style),
                Span::styled(format!("{git_label} "), git_style),
                Span::styled(format!("{ci_label} "), ci_style),
                Span::styled(format!("{comments_label:<5} "), comments_style),
                Span::styled(error_label, error_style),
            ];
            if worktree.pr.summary.is_none() && !worktree.prompt_summary.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", worktree.prompt_summary),
                    muted_style(),
                ));
            }
            if let Some(status) = worktree.auto_status {
                spans.push(Span::styled(
                    format!("  auto:{}", auto_status_label(status)),
                    auto_style(status),
                ));
            }
            for (key, value) in &worktree.wt_columns {
                if !value.is_empty() {
                    spans.push(Span::styled(format!("  {key}:{value}"), muted_style()));
                }
            }
            ListItem::new(Line::from(spans)).style(if worktree.selected {
                selected_style(model.focus == PanelFocus::Worktrees)
            } else {
                Style::default()
            })
        }));
        rows
    };
    let focused = model.focus == PanelFocus::Worktrees;
    let mut title = panel_title("3", "Worktrees", focused);
    if !model.worktree_filter.is_empty() {
        title.push_span(Span::styled(
            format!(" /{}", model.worktree_filter),
            muted_style(),
        ));
    }
    frame.render_widget(List::new(rows).block(panel_block(title, focused)), area);
}

fn render_main(frame: &mut Frame<'_>, area: Rect, model: &view::FrameModel<'_>) {
    let content_area = panel_block(Line::from(Span::styled("Main", title_style(true))), false)
        .inner(area)
        .height
        .saturating_sub(0) as usize;
    let width = area.width.saturating_sub(2) as usize;
    let lines = if let Some(dashboard) = &model.auto_dashboard {
        auto_dashboard_lines(dashboard, width, content_area)
    } else if let Some(dashboard) = &model.plan_dashboard {
        plan_dashboard_lines(dashboard, width, content_area)
    } else {
        match model.focus {
            PanelFocus::Status => status_dashboard_lines(model),
            PanelFocus::Repos => repo_overview_lines(model, width, content_area),
            PanelFocus::Worktrees => worktree_detail_lines(model),
        }
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(
                Line::from(Span::styled("Main", title_style(true))),
                false,
            ))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn status_dashboard_lines(model: &view::FrameModel<'_>) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("░▒▓█▓▒░ P ◤◥◣◢◤◥◣", logo_style())),
        Line::from(Span::styled("▒▓█▓▒░▒ R ◥◣◢◤◥◣◢", logo_style())),
        Line::from(Span::styled("▓█▓▒░▒▓ I ◣◢◤◥◣◢◤", logo_style())),
        Line::from(Span::styled("█▓▒░▒▓█ S ◢◤◥◣◢◤◥", logo_style())),
        Line::from(Span::styled("▓▒░▒▓█▓ M ◤◥◣◢◤◥◣", logo_style())),
        Line::from(""),
        Line::from(format!("version {}", env!("CARGO_PKG_VERSION"))),
        labelled_line("selected repo", model.selected_repo_label.clone()),
        Line::from(Span::styled(
            model.selected_repo_root.clone(),
            muted_style(),
        )),
        Line::from(""),
        heading_line("Navigation"),
        Line::from("1 status  2 repos  3 worktrees"),
        Line::from("Tab cycles focus; repos h/l switches views"),
        Line::from(""),
        heading_line("Documentation"),
        Line::from("GitHub repository  https://github.com/NathanaelRea/prism"),
        Line::from("Keybindings         docs/keybindings.md"),
        Line::from("Configuration       docs/config.md"),
        Line::from("README              README.md"),
        Line::from(""),
        Line::from(Span::styled("Status", title_style(true))),
        Line::from(Span::styled(
            "Local board for repository worktrees and agents",
            muted_style(),
        )),
        Line::from(""),
        Line::from(format!("Repositories: {}", model.repos.len())),
        Line::from(format!("Worktrees: {}", model.worktrees.len())),
    ];
    for row in &model.status {
        lines.push(Line::from(vec![
            Span::styled(format!("{}: ", row.label), muted_style()),
            Span::styled(
                row.value.clone(),
                if row.attention {
                    attention_style()
                } else {
                    Style::default()
                },
            ),
        ]));
    }
    lines
}

fn repo_overview_lines(
    model: &view::FrameModel<'_>,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let indices = model
        .sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            (session.repo_index == model.current_repo_index).then_some(index)
        })
        .collect::<Vec<_>>();
    let summary = repo_github_summary(model.config, model.sessions, &indices);
    let mut lines = vec![
        Line::from(Span::styled(
            model.selected_repo_label.clone(),
            title_style(true),
        )),
        Line::from(Span::styled(
            model.selected_repo_root.clone(),
            muted_style(),
        )),
    ];
    if let Some(row) = model.repos.iter().find(|row| row.selected) {
        lines.push(labelled_line("health", row.health.clone()));
    }
    lines.push(Line::from(vec![
        Span::styled("view ", muted_style()),
        Span::raw(model.repo_main_view.label().to_string()),
        Span::styled("  prs ", muted_style()),
        Span::raw(summary.open_prs.to_string()),
        Span::styled("  review needed ", muted_style()),
        Span::raw(summary.review_needed.to_string()),
        Span::styled("  ci failed ", muted_style()),
        Span::raw(summary.ci_failed.to_string()),
        Span::styled("  local ", muted_style()),
        Span::raw(summary.local_branches.to_string()),
    ]));
    lines.push(Line::from(""));
    let remaining_rows = visible_rows.saturating_sub(lines.len());
    match model.repo_main_view {
        view::RepoMainView::Github => lines.extend(repo_github_panel_lines(
            model.config,
            model.sessions,
            &indices,
            model.selected_session,
            width,
            remaining_rows,
        )),
        view::RepoMainView::Kanban => lines.extend(kanban_panel_lines(
            model.config,
            model.sessions,
            &indices,
            model.selected_session,
            width,
            remaining_rows,
        )),
    }
    lines
}

fn worktree_detail_lines(model: &view::FrameModel<'_>) -> Vec<Line<'static>> {
    let Some(index) = model.selected_session else {
        return vec![Line::from(Span::styled(
            "No worktree selected",
            muted_style(),
        ))];
    };
    let Some(session) = model.sessions.get(index) else {
        return vec![Line::from(Span::styled(
            "Selected worktree is filtered",
            muted_style(),
        ))];
    };
    let mut lines = vec![
        Line::from(Span::styled(session.branch.clone(), title_style(true))),
        Line::from(Span::styled(session.path_display.clone(), muted_style())),
        Line::from(""),
        Line::from(vec![
            Span::styled("status ", muted_style()),
            Span::raw(git_status_indicator(&session.status_label)),
            Span::styled("  agent ", muted_style()),
            Span::styled(
                agent_label(session.agent_state),
                agent_style(session.agent_state),
            ),
            Span::styled("  adopted ", muted_style()),
            Span::raw(if session.adopted { "yes" } else { "no" }),
        ]),
    ];
    if !session.prompt_summary.trim().is_empty() {
        lines.push(labelled_line("prompt", session.prompt_summary.clone()));
    }
    if let Some(status) = &session.opencode_status {
        lines.extend(opencode_status_lines(status));
    }
    lines.push(Line::from(""));
    lines.extend(pr_panel_lines(model.config, Some(session)));
    lines
}

fn plan_dashboard_lines(
    dashboard: &view::PlanDashboard,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let run = &dashboard.run.run;
    let selected_step = dashboard
        .run
        .steps
        .iter()
        .find(|step| step.step == run.selected_step)
        .or_else(|| dashboard.run.steps.first());
    let counts = dashboard.run.status_counts();
    let mut lines = vec![
        heading_line("Plan Run"),
        labelled_line("plan", run.plan_display.clone()),
        labelled_line("scope", run.scope_path.display().to_string()),
        Line::from(vec![
            Span::styled("mode ", muted_style()),
            Span::raw(plan_mode_label(run.mode)),
            Span::styled("  status ", muted_style()),
            Span::styled(
                plan_run_status_label(run.status),
                plan_run_status_style(run.status),
            ),
            Span::styled("  elapsed ", muted_style()),
            Span::raw(elapsed_label(run.created_unix_ms, run.updated_unix_ms)),
        ]),
    ];
    if let Some(step) = selected_step {
        lines.push(Line::from(vec![
            Span::styled("phase ", muted_style()),
            Span::raw(format!("{}/{} ", step.step, run.total_steps)),
            Span::styled(
                plan_step_status_label(step.status),
                plan_step_status_style(step.status),
            ),
        ]));
        if let Some(session_id) = step.opencode_session_id.as_deref() {
            lines.push(labelled_line(
                "opencode session",
                short_id(session_id).to_string(),
            ));
        }
        if let Some(tool) = step.active_tool.as_deref() {
            lines.push(labelled_line("tool", tool.to_string()));
        }
        if let Some(message) = step.latest_message.as_deref() {
            lines.push(labelled_line("latest", message.to_string()));
        }
        let todos = plan_todo_summary(step);
        if !todos.is_empty() {
            lines.push(labelled_line("todos", todos));
        }
        if let Some(error) = step.error.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("error ", muted_style()),
                Span::styled(error.to_string(), error_style()),
            ]));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("counts queued ", muted_style()),
        Span::raw((counts.queued + counts.starting).to_string()),
        Span::styled("  running ", muted_style()),
        Span::raw(counts.running.to_string()),
        Span::styled("  done ", muted_style()),
        Span::raw(counts.done.to_string()),
        Span::styled("  failed ", muted_style()),
        Span::raw(counts.failed.to_string()),
    ]));
    lines.push(Line::from(""));
    lines.push(heading_line("Phases"));
    let rendered_output = render_plan_output_rows(dashboard, width);
    let output_rows_reserved = rendered_output.len().min(8) + 2;
    let phase_rows_available = visible_rows
        .saturating_sub(lines.len())
        .saturating_sub(output_rows_reserved)
        .max(3);
    let selected_index = dashboard
        .run
        .steps
        .iter()
        .position(|step| step.step == run.selected_step)
        .unwrap_or(0);
    let start = scroll_start(selected_index, phase_rows_available);
    for step in dashboard
        .run
        .steps
        .iter()
        .skip(start)
        .take(phase_rows_available)
    {
        lines.push(plan_step_row(step, run.selected_step, run.total_steps));
    }
    lines.push(Line::from(""));
    lines.push(heading_line(if dashboard.output_state.follow {
        "Output (follow)"
    } else {
        "Output"
    }));
    if rendered_output.is_empty() {
        lines.push(Line::from(Span::styled("No output yet", muted_style())));
    } else {
        let output_rows_available = visible_rows.saturating_sub(lines.len()).max(1);
        let cursor = selected_rendered_output_index(dashboard, &rendered_output);
        let start = if dashboard.output_state.follow {
            rendered_output.len().saturating_sub(output_rows_available)
        } else {
            scroll_start(cursor, output_rows_available)
        };
        for (index, row) in rendered_output
            .iter()
            .enumerate()
            .skip(start)
            .take(output_rows_available)
        {
            lines.push(plan_output_row(row, index == cursor));
        }
    }
    lines.truncate(visible_rows);
    lines
}

fn auto_dashboard_lines(
    dashboard: &view::AutoDashboard,
    _width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let run = &dashboard.run.run;
    let selected_step = run
        .selected_step_run_id
        .and_then(|id| dashboard.run.steps.iter().find(|step| step.id == Some(id)))
        .or_else(|| dashboard.run.steps.first());
    let counts = dashboard.run.status_counts();
    let mut lines = vec![
        heading_line("Auto Flow"),
        labelled_line("task", run.prompt_summary.clone()),
        labelled_line("work", run.worktree_path.display().to_string()),
        Line::from(vec![
            Span::styled("mode ", muted_style()),
            Span::raw(auto_mode_label(run.mode)),
            Span::styled("  status ", muted_style()),
            Span::styled(auto_status_label(run.status), auto_style(run.status)),
            Span::styled("  elapsed ", muted_style()),
            Span::raw(elapsed_label(run.created_unix_ms, run.updated_unix_ms)),
        ]),
        Line::from(vec![
            Span::styled("source ", muted_style()),
            Span::raw(auto_source_label(run.implementation_source)),
            Span::raw(
                run.plan_path
                    .as_ref()
                    .map(|path| format!("  plan {}", path.display()))
                    .unwrap_or_default(),
            ),
        ]),
        labelled_line("branch", run.branch.clone()),
    ];
    if let Some(pr_number) = run.pr_number {
        lines.push(labelled_line("pr", format!("#{pr_number}")));
    }
    if let Some(step) = selected_step {
        lines.push(Line::from(vec![
            Span::styled("step ", muted_style()),
            Span::raw(format!(
                "#{} {} attempt {} ",
                step.sequence,
                step.step_key.as_str(),
                step.attempt
            )),
            Span::styled(
                auto_step_status_label(step.status),
                auto_step_status_style(step.status),
            ),
        ]));
        if let Some(session_id) = step.opencode_session_id.as_deref() {
            lines.push(labelled_line(
                "opencode session",
                short_id(session_id).to_string(),
            ));
        }
        if let Some(summary) = step.summary.as_deref().or(step.reason.as_deref()) {
            lines.push(labelled_line("latest", summary.to_string()));
        }
        if let Some(error) = step.error.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("error ", muted_style()),
                Span::styled(error.to_string(), error_style()),
            ]));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("counts queued ", muted_style()),
        Span::raw((counts.queued + counts.starting).to_string()),
        Span::styled("  running ", muted_style()),
        Span::raw(counts.running.to_string()),
        Span::styled("  waiting ", muted_style()),
        Span::raw(counts.waiting.to_string()),
        Span::styled("  done ", muted_style()),
        Span::raw(counts.done.to_string()),
        Span::styled("  failed ", muted_style()),
        Span::raw(counts.failed.to_string()),
    ]));
    lines.push(Line::from(""));
    lines.push(heading_line("Checklist"));
    let linked_plan_rows_reserved = linked_plan_summary_lines(dashboard).len();
    let output_rows_reserved = dashboard.output_lines.len().min(8) + linked_plan_rows_reserved + 2;
    let step_rows_available = visible_rows
        .saturating_sub(lines.len())
        .saturating_sub(output_rows_reserved)
        .max(3);
    lines.extend(auto_checklist_lines(dashboard, step_rows_available));
    lines.push(Line::from(""));
    lines.push(heading_line(if dashboard.output_state.follow {
        "Output (follow)"
    } else {
        "Output"
    }));
    lines.extend(linked_plan_summary_lines(dashboard));
    if dashboard.output_lines.is_empty() {
        lines.push(Line::from(Span::styled("No output yet", muted_style())));
    } else {
        let output_rows_available = visible_rows.saturating_sub(lines.len()).max(1);
        let cursor = dashboard
            .output_state
            .cursor
            .min(dashboard.output_lines.len().saturating_sub(1));
        let start = if dashboard.output_state.follow {
            dashboard
                .output_lines
                .len()
                .saturating_sub(output_rows_available)
        } else {
            scroll_start(cursor, output_rows_available)
        };
        for (index, line) in dashboard
            .output_lines
            .iter()
            .enumerate()
            .skip(start)
            .take(output_rows_available)
        {
            lines.push(auto_output_row(line, index == cursor));
        }
    }
    lines.truncate(visible_rows);
    lines
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, model: &view::FrameModel<'_>) {
    let actions = match model.focus {
        PanelFocus::Status => "1/2/3 focus  Tab next  P plan  A auto  ? help  q quit",
        PanelFocus::Repos => "j/k select  Enter open  r refresh  R manage  / search  q quit",
        PanelFocus::Worktrees => "j/k select  Enter tmux  Space g git  c create  D delete  q quit",
    };
    let mut spans = vec![
        Span::styled(
            format!(" repo {} ", model.selected_repo_root),
            muted_style(),
        ),
        Span::raw(actions.to_string()),
    ];
    if let Some(message) = model.status_message {
        spans.push(Span::styled(format!(" | {message}"), attention_style()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_leader_hint(frame: &mut Frame<'_>, area: Rect, hint: &view::LeaderHintModel) {
    let lines = choice_lines(hint);
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = content_width
        .max(hint.title.chars().count() as u16)
        .saturating_add(4)
        .min(area.width.max(1));
    let height = (lines.len() as u16)
        .saturating_add(2)
        .min(area.height.max(1));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = panel_block(
        Line::from(Span::styled(hint.title.clone(), title_style(true))),
        false,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block),
        popup,
    );
}

fn render_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &view::DialogModel) {
    let lines = dialog_lines(dialog);
    let content_width = lines
        .iter()
        .map(|line| line.width() as u16)
        .max()
        .unwrap_or(0)
        .max(dialog_title(dialog).chars().count() as u16)
        .saturating_add(4);
    let width = content_width
        .min(area.width.saturating_sub(2))
        .max(24.min(area.width));
    let height = (lines.len() as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(2))
        .max(5.min(area.height));
    let popup = centered_rect(width, height, area);
    let block = panel_block(
        Line::from(Span::styled(dialog_title(dialog), title_style(true))),
        false,
    );
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
    if let view::DialogModel::Prompt { prompt, input, .. } = dialog {
        set_prompt_cursor(frame, inner, prompt, input);
    }
}

fn set_prompt_cursor(frame: &mut Frame<'_>, area: Rect, prompt: &str, input: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let prompt_prefix_lines = prompt.split('\n').collect::<Vec<_>>();
    let prompt_prefix = prompt_prefix_lines.last().copied().unwrap_or(prompt);
    let prompt_line = Line::from(vec![
        Span::raw(prompt_prefix.to_string()),
        Span::raw(input.to_string()),
    ]);
    let width = prompt_line.width() as u16;
    let line_offset = width / area.width;
    let x_offset = width % area.width;
    let prompt_y = prompt_prefix_lines.len().saturating_sub(1) as u16;
    frame.set_cursor_position((
        area.x + x_offset.min(area.width.saturating_sub(1)),
        area.y
            + prompt_y
                .saturating_add(line_offset)
                .min(area.height.saturating_sub(1)),
    ));
}

fn dialog_title(dialog: &view::DialogModel) -> String {
    match dialog {
        view::DialogModel::Help { .. } => "Keybindings".to_string(),
        view::DialogModel::Confirm { title, .. }
        | view::DialogModel::Prompt { title, .. }
        | view::DialogModel::Choice {
            choices: view::ChoiceList { title, .. },
            ..
        }
        | view::DialogModel::Progress { title, .. } => title.clone(),
    }
}

fn dialog_lines(dialog: &view::DialogModel) -> Vec<Line<'static>> {
    match dialog {
        view::DialogModel::Help {
            filter,
            editing_filter,
            items,
        } => {
            let query = filter.trim().to_ascii_lowercase();
            let mut lines = vec![Line::from(vec![
                Span::styled("Filter: ", muted_style()),
                Span::raw(format!("/{filter}")),
                Span::styled(
                    if *editing_filter {
                        "  typing"
                    } else {
                        "  / to search"
                    },
                    muted_style(),
                ),
            ])];
            lines.push(Line::from(""));
            let mut matched = 0;
            for item in items {
                if query.is_empty() || item.to_ascii_lowercase().contains(&query) {
                    lines.push(Line::from(item.clone()));
                    matched += 1;
                }
            }
            if matched == 0 {
                lines.push(Line::from(Span::styled(
                    "No matching keybindings",
                    muted_style(),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc/q closes. / searches.",
                muted_style(),
            )));
            lines
        }
        view::DialogModel::Confirm {
            lines,
            confirm_label,
            cancel_label,
            ..
        } => {
            let mut rendered = Vec::new();
            for line in lines {
                rendered.extend(styled_text_lines(
                    &line.text,
                    if line.attention {
                        attention_style()
                    } else {
                        Style::default()
                    },
                ));
            }
            rendered.push(Line::from(""));
            rendered.push(Line::from(vec![
                Span::styled("Enter ", selected_style(true)),
                Span::styled(confirm_label.clone(), selected_style(true)),
                Span::styled("   Esc/q ", muted_style()),
                Span::raw(cancel_label.clone()),
            ]));
            rendered
        }
        view::DialogModel::Prompt { prompt, input, .. } => {
            let mut lines = prompt_dialog_lines(prompt, input);
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter to continue, Esc to cancel",
                muted_style(),
            )));
            lines
        }
        view::DialogModel::Choice { choices, .. } => choice_lines(choices),
        view::DialogModel::Progress { message, .. } => {
            let mut lines = vec![Line::from(Span::styled(
                "[*] Please wait",
                title_style(true),
            ))];
            lines.extend(styled_text_lines(message, Style::default()));
            lines
        }
    }
}

fn choice_lines(choices: &view::ChoiceList) -> Vec<Line<'static>> {
    choices
        .choices
        .iter()
        .map(|choice| {
            Line::from(vec![
                Span::styled(format!("[{}]", choice.key), selected_style(true)),
                Span::styled(format!(" {}", choice.label), muted_style()),
            ])
        })
        .collect::<Vec<_>>()
}

fn prompt_dialog_lines(prompt: &str, input: &str) -> Vec<Line<'static>> {
    let prompt_lines = prompt.split('\n').collect::<Vec<_>>();
    let mut lines = Vec::new();
    for (index, line) in prompt_lines.iter().enumerate() {
        let mut spans = styled_prompt_spans(line);
        if index + 1 == prompt_lines.len() {
            spans.push(Span::raw(input.to_string()));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn styled_prompt_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let (before, after_start) = rest.split_at(start);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), muted_style()));
        }
        if let Some(end) = after_start.find(']') {
            let (option, after_option) = after_start.split_at(end + 1);
            spans.push(Span::styled(option.to_string(), selected_style(true)));
            rest = after_option;
        } else {
            rest = after_start;
            break;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), muted_style()));
    }
    spans
}

fn styled_text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect()
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[derive(Default)]
struct RepoGithubSummary {
    open_prs: usize,
    review_needed: usize,
    ci_failed: usize,
    local_branches: usize,
}

fn repo_github_summary(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
) -> RepoGithubSummary {
    let mut summary = RepoGithubSummary::default();
    for index in session_indices {
        let Some(session) = sessions.get(*index) else {
            continue;
        };
        if session.is_default_branch(config) {
            continue;
        }
        match &session.pr.summary {
            Some(pr) => {
                if !pr.merged && pr.state == "OPEN" {
                    summary.open_prs += 1;
                }
                if review_decision_for_display(pr, session.pr.details.as_ref()) == "REVIEW_REQUIRED"
                {
                    summary.review_needed += 1;
                }
                if pr.check_status == "failed" {
                    summary.ci_failed += 1;
                }
            }
            None => summary.local_branches += 1,
        }
    }
    summary
}

fn repo_github_panel_lines(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = repo_work_list_lines(config, sessions, session_indices, selected, visible_rows);
    if lines.len() < visible_rows {
        lines.push(Line::from(""));
    }
    let preview = selected.and_then(|index| sessions.get(index));
    lines.extend(repo_preview_lines(config, preview, width));
    lines.truncate(visible_rows);
    lines
}

fn repo_work_list_lines(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading_line("PRs / Work")];
    if session_indices.is_empty() {
        lines.push(Line::from(Span::styled(
            "No worktrees discovered",
            muted_style(),
        )));
        lines.push(Line::from(Span::styled(
            "Create one with c",
            attention_style(),
        )));
        lines.truncate(visible_rows);
        return lines;
    }
    for index in session_indices {
        if lines.len() >= visible_rows {
            break;
        }
        let Some(session) = sessions.get(*index) else {
            continue;
        };
        lines.push(repo_work_item_line(
            config,
            session,
            Some(*index) == selected,
        ));
    }
    lines
}

fn repo_work_item_line(
    config: &crate::config::Config,
    session: &Session,
    selected: bool,
) -> Line<'static> {
    let marker = if selected { "▶" } else { " " };
    let kind = repo_work_kind_label(config, session);
    let label = session
        .pr
        .summary
        .as_ref()
        .map(|summary| format!("{} - {}", session.branch, summary.title))
        .unwrap_or_else(|| session.branch.clone());
    Line::from(vec![
        Span::styled(
            marker,
            if selected {
                title_style(true)
            } else {
                muted_style()
            },
        ),
        Span::raw(" "),
        Span::styled(format!("{kind:<8}"), muted_style()),
        Span::styled(
            label,
            if selected {
                selected_text_style()
            } else {
                Style::default()
            },
        ),
        Span::styled(
            format!("  {}", repo_work_detail_label(config, session)),
            muted_style(),
        ),
    ])
}

fn repo_preview_lines(
    config: &crate::config::Config,
    session: Option<&Session>,
    _width: usize,
) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![
            heading_line("Preview"),
            Line::from(Span::styled("No selected worktree", muted_style())),
            Line::from(Span::styled("Enter focuses worktrees", attention_style())),
        ];
    };
    let mut lines = vec![heading_line("Preview")];
    if session.is_default_branch(config) {
        lines.push(Line::from(Span::styled(
            "Default branch",
            selected_text_style(),
        )));
        lines.push(labelled_line("branch", session.branch.clone()));
        lines.push(labelled_line("status", session.status_label.clone()));
        lines.push(Line::from(Span::styled(
            "PR tracking disabled",
            muted_style(),
        )));
        return lines;
    }
    if let Some(error) = &session.pr.error {
        lines.push(Line::from(Span::styled(
            "✕ PR refresh error",
            error_style(),
        )));
        lines.push(Line::from(error.clone()));
        return lines;
    }
    let Some(summary) = &session.pr.summary else {
        lines.push(Line::from(Span::styled("○ No PR detected", muted_style())));
        lines.push(labelled_line("branch", session.branch.clone()));
        lines.push(labelled_line("status", session.status_label.clone()));
        lines.push(Line::from(Span::styled(
            "Space g P creates one",
            attention_style(),
        )));
        return lines;
    };
    let review = review_decision_for_display(summary, session.pr.details.as_ref());
    lines.push(Line::from(vec![
        Span::styled(pr_state_icon(summary), pr_state_style(summary)),
        Span::styled(
            format!(" PR #{} {}", summary.number, pr_state_label(summary)),
            pr_state_style(summary),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        summary.title.clone(),
        selected_text_style(),
    )));
    lines.push(Line::from(vec![
        Span::styled("review ", muted_style()),
        Span::styled(review_label(&review).to_string(), review_style(&review)),
        Span::styled("  ci ", muted_style()),
        Span::styled(summary.check_status.clone(), ci_style(config, session)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("base ", muted_style()),
        Span::raw(summary.base_ref.clone()),
        Span::styled("  head ", muted_style()),
        Span::raw(summary.head_ref.clone()),
    ]));
    if !summary.requested_reviewers.is_empty() {
        lines.push(labelled_line(
            "awaiting",
            summary.requested_reviewers.join(", "),
        ));
    }
    if let Some(details) = &session.pr.details {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("comments ", muted_style()),
            Span::raw((details.comments.len() + details.review_comments.len()).to_string()),
            Span::styled("  reviews ", muted_style()),
            Span::raw(details.reviews.len().to_string()),
            Span::styled("  files ", muted_style()),
            Span::raw(details.files.len().to_string()),
        ]));
        lines.extend(pr_comment_lines(details, 3));
        if !details.failing_checks.is_empty() {
            lines.push(Line::from(Span::styled("Failing checks", error_style())));
            for check in details.failing_checks.iter().take(2) {
                lines.push(Line::from(vec![
                    Span::styled("✕ ", error_style()),
                    Span::raw(check.clone()),
                ]));
            }
        }
        if !details.ci_failures.is_empty() {
            lines.push(labelled_line(
                "CI failures cached",
                details.ci_failures.len().to_string(),
            ));
        }
    } else {
        lines.push(Line::from(Span::styled("Activity pending", muted_style())));
    }
    lines
}

#[derive(Clone, Copy)]
enum KanbanLane {
    Plan,
    Impl,
    PrCi,
    Merged,
}

impl KanbanLane {
    fn index(self) -> usize {
        match self {
            Self::Plan => 0,
            Self::Impl => 1,
            Self::PrCi => 2,
            Self::Merged => 3,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Impl => "impl",
            Self::PrCi => "pr/ci",
            Self::Merged => "merged",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Plan => muted_style(),
            Self::Impl => attention_style(),
            Self::PrCi => Style::default().fg(Color::Green),
            Self::Merged => Style::default().fg(Color::Magenta),
        }
    }
}

const KANBAN_LANES: [KanbanLane; 4] = [
    KanbanLane::Plan,
    KanbanLane::Impl,
    KanbanLane::PrCi,
    KanbanLane::Merged,
];

fn kanban_panel_lines(
    config: &crate::config::Config,
    sessions: &[Session],
    session_indices: &[usize],
    selected: Option<usize>,
    width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    if width < 32 {
        return vec![Line::from(Span::styled(
            "Kanban needs more width",
            muted_style(),
        ))];
    }
    let mut lanes: [Vec<(usize, &Session)>; 4] = std::array::from_fn(|_| Vec::new());
    for index in session_indices {
        let Some(session) = sessions.get(*index) else {
            continue;
        };
        if let Some(lane) = kanban_lane(config, session) {
            lanes[lane.index()].push((*index, session));
        }
    }
    if lanes.iter().all(Vec::is_empty) {
        return vec![
            Line::from(Span::styled("No feature worktrees", muted_style())),
            Line::from(Span::styled("Create one with c", attention_style())),
        ];
    }
    let mut lines = vec![Line::from(
        KANBAN_LANES
            .iter()
            .enumerate()
            .flat_map(|(index, lane)| {
                vec![
                    Span::styled(
                        format!("{} {}", lane.label(), lanes[index].len()),
                        lane.style(),
                    ),
                    Span::raw("   "),
                ]
            })
            .collect::<Vec<_>>(),
    )];
    let max_lane_rows = lanes.iter().map(Vec::len).max().unwrap_or(0);
    let shown_rows = max_lane_rows.min(visible_rows.saturating_sub(lines.len()));
    for row in 0..shown_rows {
        lines.push(Line::from(
            lanes
                .iter()
                .flat_map(|lane_sessions| {
                    if let Some((index, session)) = lane_sessions.get(row) {
                        kanban_card_spans(config, session, Some(*index) == selected)
                    } else {
                        vec![Span::raw("   ")]
                    }
                })
                .collect::<Vec<_>>(),
        ));
    }
    lines
}

fn kanban_card_spans(
    config: &crate::config::Config,
    session: &Session,
    selected: bool,
) -> Vec<Span<'static>> {
    let mut suffix = git_status_indicator(&session.status_label);
    if let Some(summary) = &session.pr.summary {
        if !suffix.is_empty() {
            suffix.push(' ');
        }
        suffix.push_str(&format!("#{} {}", summary.number, ci_icon(config, session)));
    }
    vec![
        Span::styled(if selected { "▶ " } else { "  " }, title_style(selected)),
        Span::styled(
            session.branch.clone(),
            if selected {
                selected_text_style()
            } else {
                Style::default()
            },
        ),
        Span::styled(format!(" {suffix}   "), muted_style()),
    ]
}

fn kanban_lane(config: &crate::config::Config, session: &Session) -> Option<KanbanLane> {
    if session.is_default_branch(config) {
        return None;
    }
    if session
        .pr
        .summary
        .as_ref()
        .is_some_and(|summary| summary.merged)
    {
        return Some(KanbanLane::Merged);
    }
    if session.pr.summary.is_some() {
        return Some(KanbanLane::PrCi);
    }
    if status_count(&session.status_label, "dirty").is_some()
        || status_count(&session.status_label, "ahead").is_some()
        || matches!(
            session.agent_state,
            AgentState::Running
                | AgentState::ExitedError
                | AgentState::NeedsRestart
                | AgentState::NeedsInput
        )
    {
        return Some(KanbanLane::Impl);
    }
    Some(KanbanLane::Plan)
}

fn opencode_status_lines(status: &OpencodeStatus) -> Vec<Line<'static>> {
    let session = status.session_id.as_deref().map(short_id).unwrap_or("none");
    let title = status.title.as_deref().filter(|title| !title.is_empty());
    let mut lines = vec![match title {
        Some(title) => Line::from(vec![
            Span::styled("opencode ", muted_style()),
            Span::raw(status.state.label().to_string()),
            Span::styled("  session ", muted_style()),
            Span::raw(session.to_string()),
            Span::raw(format!("  {title}")),
        ]),
        None => Line::from(vec![
            Span::styled("opencode ", muted_style()),
            Span::raw(status.state.label().to_string()),
            Span::styled("  session ", muted_style()),
            Span::raw(session.to_string()),
        ]),
    }];
    if let Some(tool) = &status.active_tool {
        lines.push(labelled_line("tool", tool.clone()));
    }
    if let Some(message) = &status.latest_message {
        lines.push(labelled_line("latest", message.clone()));
    }
    let todo = todo_summary(&status.todos);
    if !todo.is_empty() {
        lines.push(labelled_line("todos", todo));
    }
    lines
}

fn pr_panel_lines(config: &crate::config::Config, session: Option<&Session>) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![Line::from(Span::styled(
            "No selected worktree",
            muted_style(),
        ))];
    };
    if session.is_default_branch(config) {
        return vec![
            heading_line("Default branch"),
            labelled_line("branch", session.branch.clone()),
            Line::from(Span::styled("PR tracking disabled", muted_style())),
        ];
    }
    if let Some(error) = &session.pr.error {
        return vec![
            Line::from(Span::styled("✕ PR refresh error", error_style())),
            Line::from(error.clone()),
            Line::from(Span::styled("Press r to retry", attention_style())),
        ];
    }
    let Some(summary) = &session.pr.summary else {
        let refreshed = session
            .pr
            .last_refreshed
            .as_deref()
            .unwrap_or("not refreshed");
        return vec![
            Line::from(Span::styled("○ No PR detected", muted_style())),
            labelled_line("branch", session.branch.clone()),
            labelled_line("last", refreshed.to_string()),
            Line::from(Span::styled("P creates one explicitly", attention_style())),
        ];
    };
    let review = review_decision_for_display(summary, session.pr.details.as_ref());
    let mut lines = vec![
        Line::from(vec![
            Span::styled(pr_state_icon(summary), pr_state_style(summary)),
            Span::styled(
                format!(" PR #{} {}", summary.number, pr_state_label(summary)),
                pr_state_style(summary),
            ),
        ]),
        Line::from(Span::styled(summary.title.clone(), selected_text_style())),
        Line::from(vec![
            Span::styled("base ", muted_style()),
            Span::raw(summary.base_ref.clone()),
            Span::styled("  head ", muted_style()),
            Span::raw(summary.head_ref.clone()),
        ]),
        Line::from(vec![
            Span::styled("review ", muted_style()),
            Span::styled(review_label(&review).to_string(), review_style(&review)),
            Span::styled("  ci ", muted_style()),
            Span::styled(ci_icon(config, session), ci_style(config, session)),
            Span::raw(format!(" {}", summary.check_status)),
        ]),
    ];
    if !summary.requested_reviewers.is_empty() {
        lines.push(labelled_line(
            "awaiting",
            summary.requested_reviewers.join(", "),
        ));
    }
    lines.push(Line::from(""));
    lines.push(heading_line("Description"));
    lines.extend(description_lines(&summary.body, 4));
    if let Some(details) = &session.pr.details {
        lines.push(Line::from(""));
        lines.push(heading_line("Activity"));
        lines.push(Line::from(vec![
            Span::styled("comments ", muted_style()),
            Span::raw((details.comments.len() + details.review_comments.len()).to_string()),
            Span::styled("  reviews ", muted_style()),
            Span::raw(details.reviews.len().to_string()),
            Span::styled("  files ", muted_style()),
            Span::raw(details.files.len().to_string()),
        ]));
        lines.extend(pr_comment_lines(details, 5));
        if !details.failing_checks.is_empty() {
            lines.push(Line::from(Span::styled("Failing checks", error_style())));
            for check in details.failing_checks.iter().take(3) {
                lines.push(Line::from(vec![
                    Span::styled("✕ ", error_style()),
                    Span::raw(check.clone()),
                ]));
            }
        }
        if !details.ci_failures.is_empty() {
            lines.push(labelled_line(
                "CI failures cached",
                details.ci_failures.len().to_string(),
            ));
        }
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Activity pending", muted_style())));
    }
    if let Some(refreshed) = &session.pr.last_refreshed {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("refreshed {refreshed}"),
            muted_style(),
        )));
    }
    lines
}

fn pr_comment_lines(details: &crate::github::PrDetails, max_comments: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(""), heading_line("Comments")];
    let mut shown = 0;
    for comment in details.comments.iter().rev() {
        if shown >= max_comments {
            break;
        }
        append_comment(&mut lines, &comment.author, "", &comment.body);
        shown += 1;
    }
    for review in details
        .reviews
        .iter()
        .rev()
        .filter(|review| !review.body.trim().is_empty())
    {
        if shown >= max_comments {
            break;
        }
        append_comment(
            &mut lines,
            &review.author,
            review_label(&review.state),
            &review.body,
        );
        shown += 1;
    }
    for comment in details.review_comments.iter().rev() {
        if shown >= max_comments {
            break;
        }
        let context = if comment.line.is_empty() {
            comment.path.clone()
        } else {
            format!("{}:{}", comment.path, comment.line)
        };
        append_comment(&mut lines, &comment.author, &context, &comment.body);
        shown += 1;
    }
    if shown == 0 {
        lines.push(Line::from(Span::styled("No comments", muted_style())));
    }
    let total = details.comments.len()
        + details.review_comments.len()
        + details
            .reviews
            .iter()
            .filter(|review| !review.body.trim().is_empty())
            .count();
    if total > shown {
        lines.push(Line::from(Span::styled(
            format!("+{} more", total - shown),
            muted_style(),
        )));
    }
    lines
}

fn append_comment(lines: &mut Vec<Line<'static>>, author: &str, context: &str, body: &str) {
    let author = if author.trim().is_empty() {
        "unknown"
    } else {
        author.trim()
    };
    let context = context.trim();
    if context.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("@ ", muted_style()),
            Span::raw(author.to_string()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("@ ", muted_style()),
            Span::raw(author.to_string()),
            Span::styled(format!(" {context}"), muted_style()),
        ]));
    }
    let mut body_lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(2)
        .peekable();
    if body_lines.peek().is_none() {
        lines.push(Line::from(Span::styled("  empty comment", muted_style())));
        return;
    }
    for line in body_lines {
        lines.push(Line::from(format!("  {line}")));
    }
}

fn description_lines(body: &str, max_lines: usize) -> Vec<Line<'static>> {
    let lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .map(|line| Line::from(line.to_string()))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        vec![Line::from(Span::styled("No description", muted_style()))]
    } else {
        lines
    }
}

fn linked_plan_summary_lines(dashboard: &view::AutoDashboard) -> Vec<Line<'static>> {
    let selected_step = dashboard
        .run
        .run
        .selected_step_run_id
        .and_then(|id| dashboard.run.steps.iter().find(|step| step.id == Some(id)))
        .or_else(|| dashboard.run.steps.first());
    if !matches!(
        selected_step.map(|step| &step.step_key),
        Some(AutoStepKey::RunPlan)
    ) {
        return Vec::new();
    }
    let Some(plan_dashboard) = dashboard.linked_plan_dashboard.as_ref() else {
        if selected_step
            .and_then(|step| step.plan_run_id.as_ref())
            .is_some()
        {
            return vec![Line::from(Span::styled(
                "linked plan unavailable",
                muted_style(),
            ))];
        }
        return Vec::new();
    };
    let plan_run = &plan_dashboard.run.run;
    let selected_phase = plan_dashboard
        .run
        .steps
        .iter()
        .find(|step| step.step == plan_run.selected_step)
        .or_else(|| plan_dashboard.run.steps.first());
    let mut lines = vec![Line::from(vec![
        Span::styled("linked plan ", muted_style()),
        Span::raw(plan_run.plan_display.clone()),
        Span::styled("  status ", muted_style()),
        Span::styled(
            plan_run_status_label(plan_run.status),
            plan_run_status_style(plan_run.status),
        ),
        Span::styled("  mode ", muted_style()),
        Span::raw(plan_mode_label(plan_run.mode)),
    ])];
    if let Some(phase) = selected_phase {
        lines.push(Line::from(vec![
            Span::styled("phase ", muted_style()),
            Span::raw(format!("{}/{} ", phase.step, plan_run.total_steps)),
            Span::styled(
                plan_step_status_label(phase.status),
                plan_step_status_style(phase.status),
            ),
            Span::raw(
                phase
                    .latest_message
                    .as_ref()
                    .map(|message| format!("  {message}"))
                    .unwrap_or_default(),
            ),
        ]));
        if let Some(error) = phase.error.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("plan error ", muted_style()),
                Span::styled(error.to_string(), error_style()),
            ]));
        }
    }
    if let Some(line) = plan_dashboard.output_lines.last() {
        lines.push(labelled_line("plan output", line.text.clone()));
    }
    lines.push(Line::from(""));
    lines
}

fn auto_checklist_lines(dashboard: &view::AutoDashboard, max_rows: usize) -> Vec<Line<'static>> {
    let run = &dashboard.run.run;
    let steps = &dashboard.run.steps;
    let mut lines = Vec::new();
    lines.push(checklist_line(
        0,
        auto_status_for_key(steps, AutoStepKey::Prepare),
        "Prepare worktree".to_string(),
    ));

    if run.implementation_source == AutoImplementationSource::DraftPlan {
        lines.push(checklist_line(
            0,
            auto_status_for_key(steps, AutoStepKey::CreatePlan),
            "Create implementation plan".to_string(),
        ));
        lines.push(checklist_line(
            0,
            auto_status_for_key(steps, AutoStepKey::ReviewPlan),
            "Review implementation plan".to_string(),
        ));
        lines.push(checklist_line(
            0,
            auto_status_for_key(steps, AutoStepKey::ApprovePlan),
            "Approve implementation plan".to_string(),
        ));
    }

    if run.implementation_source == AutoImplementationSource::Prompt {
        let label = format!("Implement \"{}\"", truncate(&run.prompt_summary, 50));
        lines.push(checklist_line(
            0,
            auto_status_for_key(steps, AutoStepKey::Implement),
            label,
        ));
    } else {
        let plan_name = dashboard
            .linked_plan_dashboard
            .as_ref()
            .map(|plan| plan.run.run.plan_display.clone())
            .or_else(|| {
                run.plan_path
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| run.prompt_summary.clone());
        lines.push(checklist_line(
            0,
            plan_implementation_status(dashboard),
            format!("Run plan {plan_name}"),
        ));
        if let Some(plan) = dashboard.linked_plan_dashboard.as_ref() {
            for phase in &plan.run.steps {
                lines.push(checklist_line(
                    1,
                    plan_step_as_auto_status(phase.status),
                    format!("Run Phase {}", phase.step),
                ));
            }
        }
    }

    push_local_validation_loop(&mut lines, steps);
    lines.push(checklist_line(
        0,
        auto_status_for_key(steps, AutoStepKey::CommitImpl),
        "Commit implementation".to_string(),
    ));
    lines.push(checklist_line(
        0,
        auto_status_for_key(steps, AutoStepKey::PushPr),
        "Create or update PR".to_string(),
    ));
    push_review_loop(&mut lines, steps);
    push_ci_loop(&mut lines, steps);
    lines.push(checklist_line(
        0,
        auto_status_for_key(steps, AutoStepKey::Merge),
        "Run final merge safety gate".to_string(),
    ));
    lines.push(checklist_line(
        0,
        auto_status_for_key(steps, AutoStepKey::Cleanup),
        "Clean up merged worktree/session".to_string(),
    ));

    if lines.len() > max_rows {
        lines.truncate(max_rows.saturating_sub(1));
        lines.push(Line::from(Span::styled("  ...", muted_style())));
    }
    lines
}

fn push_local_validation_loop(lines: &mut Vec<Line<'static>>, steps: &[AutoStepRun]) {
    let group_status = first_active_or_latest_status(
        steps,
        &[AutoStepKey::FixLocalVerify, AutoStepKey::LocalVerify],
    );
    lines.push(checklist_line(
        0,
        group_status,
        "Local validation loop".to_string(),
    ));
    lines.push(checklist_line(
        1,
        auto_status_for_key(steps, AutoStepKey::LocalVerify),
        "Run local validation".to_string(),
    ));
    if step_seen(steps, &AutoStepKey::FixLocalVerify) {
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::FixLocalVerify),
            format!(
                "Fix local validation failure ({})",
                attempt_label(steps, &AutoStepKey::FixLocalVerify, 3)
            ),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::LocalVerify),
            "Re-run local validation".to_string(),
        ));
    }
}

fn push_review_loop(lines: &mut Vec<Line<'static>>, steps: &[AutoStepRun]) {
    let group_status = if step_seen(steps, &AutoStepKey::WaitCi) {
        AutoStepStatus::Done
    } else {
        first_active_or_latest_status(
            steps,
            &[
                AutoStepKey::FixReview,
                AutoStepKey::VerifyReviewFix,
                AutoStepKey::CommitReviewFix,
                AutoStepKey::WaitReview,
            ],
        )
    };
    lines.push(checklist_line(
        0,
        group_status,
        "Review feedback loop".to_string(),
    ));
    lines.push(checklist_line(
        1,
        auto_status_for_key(steps, AutoStepKey::WaitReview),
        "Wait for automated review".to_string(),
    ));
    if step_seen(steps, &AutoStepKey::FixReview) {
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::FixReview),
            format!(
                "Fix review feedback ({})",
                attempt_label(steps, &AutoStepKey::FixReview, 3)
            ),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::VerifyReviewFix),
            "Verify review fixes".to_string(),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::CommitReviewFix),
            "Commit and push review fixes".to_string(),
        ));
        if step_count(steps, &AutoStepKey::WaitReview) > 1 {
            lines.push(checklist_line(
                1,
                auto_status_for_key(steps, AutoStepKey::WaitReview),
                "Wait for automated review again".to_string(),
            ));
        }
    }
}

fn push_ci_loop(lines: &mut Vec<Line<'static>>, steps: &[AutoStepRun]) {
    let group_status = if step_seen(steps, &AutoStepKey::Merge) {
        AutoStepStatus::Done
    } else {
        first_active_or_latest_status(
            steps,
            &[
                AutoStepKey::FixCi,
                AutoStepKey::VerifyCiFix,
                AutoStepKey::CommitCiFix,
                AutoStepKey::WaitCi,
            ],
        )
    };
    lines.push(checklist_line(0, group_status, "CI loop".to_string()));
    lines.push(checklist_line(
        1,
        auto_status_for_key(steps, AutoStepKey::WaitCi),
        "Wait for PR checks".to_string(),
    ));
    if step_seen(steps, &AutoStepKey::FixCi) {
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::FixCi),
            format!(
                "Fix CI failure ({})",
                attempt_label(steps, &AutoStepKey::FixCi, 3)
            ),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::VerifyCiFix),
            "Verify CI fixes".to_string(),
        ));
        lines.push(checklist_line(
            1,
            auto_status_for_key(steps, AutoStepKey::CommitCiFix),
            "Commit and push CI fixes".to_string(),
        ));
        if step_count(steps, &AutoStepKey::WaitCi) > 1 {
            lines.push(checklist_line(
                1,
                auto_status_for_key(steps, AutoStepKey::WaitCi),
                "Wait for PR checks again".to_string(),
            ));
        }
    }
}

fn checklist_line(indent: usize, status: AutoStepStatus, label: String) -> Line<'static> {
    Line::from(vec![
        Span::raw("  ".repeat(indent)),
        Span::styled(checklist_mark(status), auto_step_status_style(status)),
        Span::raw(" "),
        Span::styled(label, auto_step_status_style(status)),
    ])
}

fn checklist_mark(status: AutoStepStatus) -> &'static str {
    match status {
        AutoStepStatus::Done | AutoStepStatus::Skipped => "[x]",
        AutoStepStatus::Failed | AutoStepStatus::Aborted => "[!]",
        AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting => "[-]",
        AutoStepStatus::Queued => "[ ]",
    }
}

fn auto_status_for_key(steps: &[AutoStepRun], key: AutoStepKey) -> AutoStepStatus {
    latest_step_for_key(steps, &key)
        .map(|step| step.status)
        .unwrap_or(AutoStepStatus::Queued)
}

fn latest_step_for_key<'a>(steps: &'a [AutoStepRun], key: &AutoStepKey) -> Option<&'a AutoStepRun> {
    steps.iter().rev().find(|step| &step.step_key == key)
}

fn step_seen(steps: &[AutoStepRun], key: &AutoStepKey) -> bool {
    steps.iter().any(|step| &step.step_key == key)
}

fn step_count(steps: &[AutoStepRun], key: &AutoStepKey) -> usize {
    steps.iter().filter(|step| &step.step_key == key).count()
}

fn attempt_label(steps: &[AutoStepRun], key: &AutoStepKey, max_attempts: usize) -> String {
    let attempt = latest_step_for_key(steps, key)
        .map(|step| step.attempt)
        .unwrap_or(1);
    format!("attempt {}/{max_attempts}", attempt.min(max_attempts))
}

fn first_active_or_latest_status(steps: &[AutoStepRun], keys: &[AutoStepKey]) -> AutoStepStatus {
    for key in keys {
        let status = auto_status_for_key(steps, key.clone());
        if matches!(
            status,
            AutoStepStatus::Starting
                | AutoStepStatus::Running
                | AutoStepStatus::Waiting
                | AutoStepStatus::Failed
                | AutoStepStatus::Aborted
        ) {
            return status;
        }
    }
    keys.iter()
        .rev()
        .map(|key| auto_status_for_key(steps, key.clone()))
        .find(|status| *status != AutoStepStatus::Queued)
        .unwrap_or(AutoStepStatus::Queued)
}

fn plan_implementation_status(dashboard: &view::AutoDashboard) -> AutoStepStatus {
    if let Some(plan) = dashboard.linked_plan_dashboard.as_ref() {
        if plan
            .run
            .steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Done)
        {
            return AutoStepStatus::Done;
        }
        if plan
            .run
            .steps
            .iter()
            .any(|step| step.status == PlanStepStatus::Failed)
        {
            return AutoStepStatus::Failed;
        }
        if plan.run.steps.iter().any(|step| {
            matches!(
                step.status,
                PlanStepStatus::Starting | PlanStepStatus::Running
            )
        }) {
            return AutoStepStatus::Running;
        }
    }
    auto_status_for_key(&dashboard.run.steps, AutoStepKey::RunPlan)
}

fn plan_step_as_auto_status(status: PlanStepStatus) -> AutoStepStatus {
    match status {
        PlanStepStatus::Queued => AutoStepStatus::Queued,
        PlanStepStatus::Starting => AutoStepStatus::Starting,
        PlanStepStatus::Running => AutoStepStatus::Running,
        PlanStepStatus::Done => AutoStepStatus::Done,
        PlanStepStatus::Failed => AutoStepStatus::Failed,
        PlanStepStatus::Aborted => AutoStepStatus::Aborted,
        PlanStepStatus::Skipped => AutoStepStatus::Skipped,
    }
}

fn auto_output_row(line: &crate::auto_flow::AutoOutputLine, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(if selected { "▶ " } else { "  " }, title_style(selected)),
        Span::styled(
            format!("{:<10}", auto_output_kind_label(line.kind)),
            auto_output_kind_style(line.kind),
        ),
        Span::raw(format!(" {}", line.text)),
    ])
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedPlanOutputRow {
    line_number: u64,
    kind: PlanOutputKind,
    text: String,
    collapsed: bool,
    block_key: Option<String>,
}

fn render_plan_output_rows(
    dashboard: &view::PlanDashboard,
    width: usize,
) -> Vec<RenderedPlanOutputRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < dashboard.output_lines.len() {
        let line = &dashboard.output_lines[index];
        if let Some(block_key) = plan_output_block_key(line) {
            let block_len = block_len_at(&dashboard.output_lines, index, &block_key);
            let collapsed = !dashboard.output_state.expanded_blocks.contains(&block_key);
            if collapsed {
                rows.push(RenderedPlanOutputRow {
                    line_number: line.line_number,
                    kind: line.kind,
                    text: collapsed_block_summary(
                        &dashboard.output_lines[index..index + block_len],
                        width,
                    ),
                    collapsed: true,
                    block_key: Some(block_key),
                });
                index += block_len;
                continue;
            }
        }
        for text in output_display_lines(line) {
            rows.push(RenderedPlanOutputRow {
                line_number: line.line_number,
                kind: line.kind,
                text,
                collapsed: false,
                block_key: plan_output_block_key(line),
            });
        }
        index += 1;
    }
    rows
}

fn block_len_at(lines: &[PlanOutputLine], index: usize, block_key: &str) -> usize {
    let mut len = 0;
    for line in &lines[index..] {
        if plan_output_block_key(line).as_deref() != Some(block_key) {
            break;
        }
        len += 1;
    }
    len.max(1)
}

fn selected_rendered_output_index(
    dashboard: &view::PlanDashboard,
    rendered_output: &[RenderedPlanOutputRow],
) -> usize {
    let Some(selected_line) = dashboard.output_lines.get(
        dashboard
            .output_state
            .cursor
            .min(dashboard.output_lines.len().saturating_sub(1)),
    ) else {
        return 0;
    };
    let selected_block_key = plan_output_block_key(selected_line);
    if let Some(block_key) = selected_block_key.as_deref()
        && let Some(index) = rendered_output
            .iter()
            .position(|row| row.collapsed && row.block_key.as_deref() == Some(block_key))
    {
        return index;
    }
    rendered_output
        .iter()
        .position(|row| row.line_number == selected_line.line_number)
        .or_else(|| {
            selected_block_key.as_deref().and_then(|block_key| {
                rendered_output
                    .iter()
                    .position(|row| row.block_key.as_deref() == Some(block_key))
            })
        })
        .unwrap_or_else(|| rendered_output.len().saturating_sub(1))
}

fn collapsed_block_summary(lines: &[PlanOutputLine], _width: usize) -> String {
    let Some(first) = lines.first() else {
        return String::new();
    };
    let line_count = lines
        .iter()
        .map(|line| line.text.lines().count().max(1))
        .sum::<usize>();
    let text = first.text.lines().next().unwrap_or("").replace('\n', " ");
    format!("[+] L{} {} lines  {}", first.line_number, line_count, text)
}

fn output_display_lines(line: &PlanOutputLine) -> Vec<String> {
    let rows = line.text.lines().collect::<Vec<_>>();
    if rows.is_empty() {
        return vec![String::new()];
    }
    rows.into_iter().map(ToString::to_string).collect()
}

fn plan_step_row(step: &PlanStepRun, selected_step: usize, total_steps: usize) -> Line<'static> {
    let selected = step.step == selected_step;
    let detail = step
        .active_tool
        .as_deref()
        .or(step.latest_message.as_deref())
        .or(step.error.as_deref())
        .unwrap_or("");
    Line::from(vec![
        Span::styled(if selected { "▶ " } else { "  " }, title_style(selected)),
        Span::styled(format!("{}/{} ", step.step, total_steps), muted_style()),
        Span::styled(
            format!("{:<10}", plan_step_status_label(step.status)),
            plan_step_status_style(step.status),
        ),
        Span::styled(format!(" {:<8}", elapsed_step_label(step)), muted_style()),
        Span::raw(detail.to_string()),
    ])
}

fn plan_output_row(row: &RenderedPlanOutputRow, selected: bool) -> Line<'static> {
    let fold = if row.block_key.is_some() {
        if row.collapsed { "[+]" } else { "[-]" }
    } else {
        "   "
    };
    Line::from(vec![
        Span::styled(if selected { "> " } else { "  " }, title_style(selected)),
        Span::raw(format!("{fold} ")),
        Span::styled(format!("L{:<4} ", row.line_number), muted_style()),
        Span::styled(
            format!("{:<10}", plan_output_kind_label(row.kind)),
            plan_output_kind_style(row.kind),
        ),
        Span::raw(" "),
        Span::styled(row.text.clone(), diff_text_style(row.kind, &row.text)),
    ])
}

fn labelled_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} "), muted_style()),
        Span::raw(value),
    ])
}

fn heading_line(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(label, title_style(true)))
}

fn scroll_start(selected: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return selected;
    }
    selected.saturating_sub(visible_rows / 2)
}

fn repo_work_kind_label(config: &crate::config::Config, session: &Session) -> String {
    if session.is_default_branch(config) {
        "default".to_string()
    } else if let Some(summary) = &session.pr.summary {
        format!("#{}", summary.number)
    } else {
        "local".to_string()
    }
}

fn repo_work_detail_label(config: &crate::config::Config, session: &Session) -> String {
    let mut parts = Vec::new();
    if session.is_default_branch(config) {
        parts.push("tracking off".to_string());
    } else if let Some(summary) = &session.pr.summary {
        parts.push(pr_state_label(summary).to_string());
        parts.push(
            review_label(&review_decision_for_display(
                summary,
                session.pr.details.as_ref(),
            ))
            .to_string(),
        );
        parts.push(format!(
            "ci {} {}",
            ci_icon(config, session),
            summary.check_status
        ));
        parts.push(pr_comment_count_label(&session.pr));
    } else {
        parts.push("no PR".to_string());
    }
    let git = git_status_indicator(&session.status_label);
    if !git.is_empty() {
        parts.push(git);
    }
    if matches!(
        session.agent_state,
        AgentState::Running | AgentState::NeedsInput | AgentState::NeedsRestart
    ) {
        parts.push(format!("agent {}", agent_icon(session.agent_state)));
    }
    parts.join("  ")
}

fn pr_comment_count_label(cache: &crate::github::PrCache) -> String {
    if let Some(details) = &cache.details {
        let open = details.comments.len()
            + details
                .review_comments
                .iter()
                .filter(|comment| !comment.resolved)
                .count();
        let resolved = details
            .review_comments
            .iter()
            .filter(|comment| comment.resolved)
            .count();
        return format!("#{open}✓{resolved}");
    }
    cache
        .summary
        .as_ref()
        .map(|summary| format!("#{}", summary.comment_count))
        .unwrap_or_else(|| "#?".to_string())
}

fn review_decision_for_display(
    summary: &crate::github::PrSummary,
    details: Option<&crate::github::PrDetails>,
) -> String {
    if !matches!(summary.review_decision.as_str(), "" | "UNKNOWN") {
        return summary.review_decision.clone();
    }
    if !summary.requested_reviewers.is_empty() {
        return "REVIEW_REQUIRED".to_string();
    }
    details
        .and_then(|details| {
            details
                .reviews
                .iter()
                .rev()
                .find(|review| !review.state.trim().is_empty())
        })
        .map(|review| review.state.clone())
        .or_else(|| {
            details
                .is_some_and(|details| !details.review_comments.is_empty())
                .then(|| "COMMENTED".to_string())
        })
        .unwrap_or_else(|| summary.review_decision.clone())
}

fn pr_state_label(summary: &crate::github::PrSummary) -> &'static str {
    if summary.merged {
        "merged"
    } else if summary.draft {
        "draft"
    } else if summary.state == "OPEN" {
        "open"
    } else {
        "closed"
    }
}

fn review_label(decision: &str) -> &str {
    match decision {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" => "changes",
        "REVIEW_REQUIRED" => "needed",
        "COMMENTED" => "commented",
        "" | "UNKNOWN" => "unknown",
        _ => decision,
    }
}

fn pr_state_icon(summary: &crate::github::PrSummary) -> &'static str {
    if summary.merged {
        "⋈"
    } else if summary.draft {
        "◐"
    } else if summary.state == "OPEN" {
        "⇄"
    } else {
        "×"
    }
}

fn ci_icon(config: &crate::config::Config, session: &Session) -> &'static str {
    if session.is_default_branch(config) {
        return "";
    }
    match session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => "✓",
        Some("failed") => "✕",
        Some("running") => "•",
        Some("mixed") => "±",
        Some("unknown") | None => "?",
        Some(_) => "!",
    }
}

fn agent_icon(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "○",
        AgentState::Running => "●",
        AgentState::ExitedOk => "✓",
        AgentState::ExitedError => "✕",
        AgentState::NeedsRestart => "↻",
        AgentState::NeedsInput => "!",
    }
}

fn git_status_indicator(status: &str) -> String {
    let mut out = String::new();
    if let Some(count) = status_count(status, "dirty") {
        out.push('✗');
        out.push_str(&count.to_string());
    }
    if let Some(count) = status_count(status, "ahead") {
        out.push('↑');
        out.push_str(&count.to_string());
    }
    if let Some(count) = status_count(status, "behind") {
        out.push('↓');
        out.push_str(&count.to_string());
    }
    out
}

fn plan_mode_label(mode: PlanRunMode) -> &'static str {
    match mode {
        PlanRunMode::Sequential => "sequential",
        PlanRunMode::Parallel => "parallel",
    }
}

fn auto_mode_label(mode: AutoRunMode) -> &'static str {
    match mode {
        AutoRunMode::Standard => "standard",
        AutoRunMode::PlanFirst => "plan_first",
    }
}

fn auto_source_label(source: AutoImplementationSource) -> &'static str {
    match source {
        AutoImplementationSource::Prompt => "prompt",
        AutoImplementationSource::ExistingPlan => "plan file",
        AutoImplementationSource::DraftPlan => "draft plan",
    }
}

fn auto_step_status_label(status: AutoStepStatus) -> &'static str {
    match status {
        AutoStepStatus::Queued => "queued",
        AutoStepStatus::Starting => "starting",
        AutoStepStatus::Running => "running",
        AutoStepStatus::Waiting => "waiting",
        AutoStepStatus::Done => "done",
        AutoStepStatus::Failed => "failed",
        AutoStepStatus::Aborted => "aborted",
        AutoStepStatus::Skipped => "skipped",
    }
}

fn auto_output_kind_label(kind: AutoOutputKind) -> &'static str {
    match kind {
        AutoOutputKind::Assistant => "assistant",
        AutoOutputKind::Tool => "tool",
        AutoOutputKind::ToolOutput => "tool out",
        AutoOutputKind::Diff => "diff",
        AutoOutputKind::Status => "status",
        AutoOutputKind::System => "system",
        AutoOutputKind::Error => "error",
        AutoOutputKind::RawJson => "json",
    }
}

fn plan_run_status_label(status: PlanRunStatus) -> &'static str {
    match status {
        PlanRunStatus::Draft => "draft",
        PlanRunStatus::Queued => "queued",
        PlanRunStatus::Running => "running",
        PlanRunStatus::Paused => "paused",
        PlanRunStatus::Done => "done",
        PlanRunStatus::Failed => "failed",
        PlanRunStatus::Aborted => "aborted",
    }
}

fn plan_step_status_label(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Queued => "queued",
        PlanStepStatus::Starting => "starting",
        PlanStepStatus::Running => "running",
        PlanStepStatus::Done => "done",
        PlanStepStatus::Failed => "failed",
        PlanStepStatus::Aborted => "aborted",
        PlanStepStatus::Skipped => "skipped",
    }
}

fn plan_output_kind_label(kind: PlanOutputKind) -> &'static str {
    match kind {
        PlanOutputKind::Assistant => "assistant",
        PlanOutputKind::Tool => "tool",
        PlanOutputKind::ToolOutput => "tool out",
        PlanOutputKind::Diff => "diff",
        PlanOutputKind::Todo => "todo",
        PlanOutputKind::Status => "status",
        PlanOutputKind::RawJson => "json",
        PlanOutputKind::System => "system",
        PlanOutputKind::Error => "error",
    }
}

fn plan_todo_summary(step: &PlanStepRun) -> String {
    let mut pending = 0;
    let mut active = 0;
    let mut done = 0;
    for todo in &step.todos {
        match todo.status.as_str() {
            "completed" | "complete" | "done" => done += 1,
            "in_progress" | "in-progress" | "active" | "running" => active += 1,
            _ => pending += 1,
        }
    }
    let mut parts = Vec::new();
    if pending > 0 {
        parts.push(format!("pending {pending}"));
    }
    if active > 0 {
        parts.push(format!("active {active}"));
    }
    if done > 0 {
        parts.push(format!("done {done}"));
    }
    parts.join("  ")
}

fn elapsed_step_label(step: &PlanStepRun) -> String {
    match (step.started_unix_ms, step.finished_unix_ms) {
        (Some(start), Some(end)) => elapsed_label(start, end),
        (Some(start), None) => elapsed_label(start, now_unix_ms()),
        _ => String::new(),
    }
}

fn elapsed_label(start_unix_ms: u64, end_unix_ms: u64) -> String {
    let total_seconds = end_unix_ms.saturating_sub(start_unix_ms) / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn todo_summary(todos: &[crate::opencode::OpencodeTodo]) -> String {
    let mut pending = 0;
    let mut active = 0;
    let mut completed = 0;
    for todo in todos {
        match todo.status.as_str() {
            "completed" | "complete" | "done" => completed += 1,
            "in_progress" | "in-progress" | "active" | "running" => active += 1,
            _ => pending += 1,
        }
    }
    let mut parts = Vec::new();
    if pending > 0 {
        parts.push(format!("pending {pending}"));
    }
    if active > 0 {
        parts.push(format!("active {active}"));
    }
    if completed > 0 {
        parts.push(format!("done {completed}"));
    }
    parts.join("  ")
}

fn sidebar_width(cols: u16) -> u16 {
    if cols >= 160 {
        72
    } else if cols >= 120 {
        56
    } else {
        cols.saturating_mul(2).saturating_div(5).clamp(20, 42)
    }
}

fn panel_block(title: Line<'static>, highlighted: bool) -> Block<'static> {
    let block = Block::default().borders(Borders::ALL).title(title);
    if highlighted {
        block.border_style(highlight_style())
    } else {
        block
    }
}

fn panel_title(key: &'static str, title: &'static str, focused: bool) -> Line<'static> {
    Line::from(Span::styled(
        format!("[{key}] {title}"),
        title_style(focused),
    ))
}

fn agent_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Running => "running",
        AgentState::NeedsInput => "input",
        AgentState::NeedsRestart => "restart",
        AgentState::ExitedOk => "done",
        AgentState::ExitedError => "error",
        AgentState::Idle => "idle",
    }
}

fn truncate_column(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 0 {
        out.pop();
        out.push('~');
    }
    out
}

fn worktree_pr_column(worktree: &view::WorktreeRow) -> (&'static str, Style) {
    if matches!(worktree.kind, view::WorktreeKind::DefaultBranch) {
        return ("·", muted_style());
    }
    if worktree.pr.error.is_some() {
        return ("!", error_style());
    }
    let Some(summary) = &worktree.pr.summary else {
        return ("○", muted_style());
    };
    (pr_state_icon(summary), pr_style(summary))
}

fn worktree_git_column(worktree: &view::WorktreeRow) -> (&'static str, Style) {
    if status_count(&worktree.status_label, "dirty").is_some() {
        (
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if status_count(&worktree.status_label, "ahead").is_some()
        && status_count(&worktree.status_label, "behind").is_some()
    {
        ("↕", attention_style())
    } else if status_count(&worktree.status_label, "ahead").is_some() {
        ("↑", attention_style())
    } else if status_count(&worktree.status_label, "behind").is_some() {
        ("↓", attention_style())
    } else {
        ("✓", muted_style())
    }
}

fn worktree_ci_column(worktree: &view::WorktreeRow) -> (&'static str, Style) {
    if matches!(worktree.kind, view::WorktreeKind::DefaultBranch) {
        return ("·", muted_style());
    }
    let Some(summary) = &worktree.pr.summary else {
        return ("·", muted_style());
    };
    (
        ci_icon_for_status(&summary.check_status),
        pr_check_style(&summary.check_status),
    )
}

fn worktree_comments_column(worktree: &view::WorktreeRow) -> (String, Style) {
    let label = if let Some(details) = &worktree.pr.details {
        let unresolved = details.comments.len()
            + details
                .review_comments
                .iter()
                .filter(|comment| !comment.resolved)
                .count();
        let resolved = details
            .review_comments
            .iter()
            .filter(|comment| comment.resolved)
            .count();
        if unresolved == 0 && resolved == 0 {
            "·".to_string()
        } else {
            format!("{unresolved}/{resolved}")
        }
    } else if let Some(summary) = &worktree.pr.summary {
        if summary.comment_count == 0 {
            "·".to_string()
        } else {
            format!("{}/?", summary.comment_count)
        }
    } else {
        "·".to_string()
    };
    let has_unresolved = worktree.pr.details.as_ref().is_some_and(|details| {
        !details.comments.is_empty()
            || details
                .review_comments
                .iter()
                .any(|comment| !comment.resolved)
    });
    let style = if worktree.unseen_comments || has_unresolved {
        attention_style()
    } else {
        muted_style()
    };
    (truncate_column(&label, 5), style)
}

fn worktree_error_column(worktree: &view::WorktreeRow) -> (&'static str, Style) {
    if worktree.pr.error.is_some() || worktree.agent_state == AgentState::ExitedError {
        ("!", error_style())
    } else if matches!(
        worktree.agent_state,
        AgentState::NeedsInput | AgentState::NeedsRestart
    ) {
        ("?", attention_style())
    } else {
        ("·", muted_style())
    }
}

fn ci_icon_for_status(status: &str) -> &'static str {
    match status {
        "passed" => "✓",
        "failed" => "✕",
        "running" => "•",
        "mixed" => "±",
        "unknown" => "?",
        _ => "!",
    }
}

fn auto_status_label(status: AutoRunStatus) -> &'static str {
    match status {
        AutoRunStatus::Queued => "queued",
        AutoRunStatus::Running => "running",
        AutoRunStatus::Paused => "paused",
        AutoRunStatus::Done => "done",
        AutoRunStatus::Failed => "failed",
        AutoRunStatus::Aborted => "aborted",
    }
}

fn highlight_style() -> Style {
    Style::default().fg(highlight_color())
}

fn highlight_color() -> Color {
    Color::Rgb(0, 255, 255)
}

fn title_style(focused: bool) -> Style {
    let style = highlight_style();
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn logo_style() -> Style {
    highlight_style().add_modifier(Modifier::BOLD)
}

fn selected_text_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn error_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

fn selected_style(focused: bool) -> Style {
    let style = if focused {
        Style::default().fg(Color::Black).bg(highlight_color())
    } else {
        Style::default().bg(Color::DarkGray)
    };
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn attention_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn muted_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn health_style(health: &str) -> Style {
    if health == "ok" {
        Style::default().fg(Color::Green)
    } else if health.contains('!') || health.contains("CIx") {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    }
}

fn agent_style(state: AgentState) -> Style {
    match state {
        AgentState::Running => Style::default().fg(Color::Green),
        AgentState::NeedsInput | AgentState::NeedsRestart => attention_style(),
        AgentState::ExitedOk => muted_style(),
        AgentState::ExitedError => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentState::Idle => muted_style(),
    }
}

fn auto_style(status: AutoRunStatus) -> Style {
    match status {
        AutoRunStatus::Running | AutoRunStatus::Queued => Style::default().fg(Color::Green),
        AutoRunStatus::Paused => attention_style(),
        AutoRunStatus::Failed | AutoRunStatus::Aborted => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        AutoRunStatus::Done => muted_style(),
    }
}

fn auto_step_status_style(status: AutoStepStatus) -> Style {
    match status {
        AutoStepStatus::Done => Style::default().fg(Color::Green),
        AutoStepStatus::Failed | AutoStepStatus::Aborted => error_style(),
        AutoStepStatus::Running | AutoStepStatus::Starting | AutoStepStatus::Waiting => {
            attention_style()
        }
        AutoStepStatus::Queued | AutoStepStatus::Skipped => Style::default(),
    }
}

fn auto_output_kind_style(kind: AutoOutputKind) -> Style {
    match kind {
        AutoOutputKind::Assistant => Style::default(),
        AutoOutputKind::Tool | AutoOutputKind::ToolOutput => attention_style(),
        AutoOutputKind::Diff => title_style(false),
        AutoOutputKind::Error => error_style(),
        AutoOutputKind::Status | AutoOutputKind::System | AutoOutputKind::RawJson => muted_style(),
    }
}

fn plan_run_status_style(status: PlanRunStatus) -> Style {
    match status {
        PlanRunStatus::Paused => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        PlanRunStatus::Queued | PlanRunStatus::Running => attention_style(),
        PlanRunStatus::Failed | PlanRunStatus::Aborted => error_style(),
        PlanRunStatus::Done => Style::default().fg(Color::Green),
        PlanRunStatus::Draft => muted_style(),
    }
}

fn plan_step_status_style(status: PlanStepStatus) -> Style {
    match status {
        PlanStepStatus::Done => Style::default().fg(Color::Green),
        PlanStepStatus::Failed | PlanStepStatus::Aborted => error_style(),
        PlanStepStatus::Running | PlanStepStatus::Starting => attention_style(),
        PlanStepStatus::Queued | PlanStepStatus::Skipped => Style::default(),
    }
}

fn plan_output_kind_style(kind: PlanOutputKind) -> Style {
    match kind {
        PlanOutputKind::Assistant => Style::default(),
        PlanOutputKind::Tool | PlanOutputKind::ToolOutput => attention_style(),
        PlanOutputKind::Diff => title_style(false),
        PlanOutputKind::Todo => Style::default().fg(Color::Magenta),
        PlanOutputKind::Error => error_style(),
        PlanOutputKind::Status | PlanOutputKind::RawJson | PlanOutputKind::System => muted_style(),
    }
}

fn diff_text_style(kind: PlanOutputKind, text: &str) -> Style {
    if kind != PlanOutputKind::Diff {
        return Style::default();
    }
    if text.starts_with("+++") || text.starts_with("---") || text.starts_with("@@") {
        return title_style(true);
    }
    if text.starts_with('+') {
        return Style::default().fg(Color::Green);
    }
    if text.starts_with('-') {
        return Style::default().fg(Color::Red);
    }
    Style::default()
}

fn pr_state_style(summary: &crate::github::PrSummary) -> Style {
    if summary.merged {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if summary.draft {
        muted_style().add_modifier(Modifier::BOLD)
    } else if summary.state == "OPEN" {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        error_style()
    }
}

fn review_style(decision: &str) -> Style {
    match decision {
        "APPROVED" => Style::default().fg(Color::Green),
        "CHANGES_REQUESTED" => Style::default().fg(Color::Red),
        "REVIEW_REQUIRED" => attention_style(),
        "COMMENTED" => title_style(false),
        _ => muted_style(),
    }
}

fn ci_style(config: &crate::config::Config, session: &Session) -> Style {
    if session.is_default_branch(config) {
        return muted_style();
    }
    match session
        .pr
        .summary
        .as_ref()
        .map(|summary| summary.check_status.as_str())
    {
        Some("passed") => Style::default().fg(Color::Green),
        Some("failed") => Style::default().fg(Color::Red),
        Some("running") => attention_style(),
        Some("mixed") => Style::default().fg(Color::Magenta),
        Some("unknown") | None => muted_style(),
        Some(_) => attention_style(),
    }
}

fn pr_check_style(status: &str) -> Style {
    match status {
        "passed" => Style::default().fg(Color::Green),
        "failed" => Style::default().fg(Color::Red),
        "running" => attention_style(),
        "mixed" => Style::default().fg(Color::Magenta),
        "unknown" => muted_style(),
        _ => attention_style(),
    }
}

fn pr_style(summary: &crate::github::PrSummary) -> Style {
    if summary.merged {
        Style::default().fg(Color::Magenta)
    } else if summary.draft {
        muted_style()
    } else if summary.state == "OPEN" {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    }
}

#[cfg(test)]
mod tests {
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
        },
        config::{Checks, Config, EscapeKey, MergeMethod},
        github::{PrCache, PrDetails, PrReviewComment, PrSummary},
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

    use super::*;

    #[test]
    fn renders_wide_shell_with_sidebar_main_and_footer() {
        let config = test_config();
        let sessions = vec![test_session("feature", AgentState::Running)];
        let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        let buffer = render_to_buffer(&model, 120, 30);

        assert_region_contains(&buffer, 0..44, 0..30, "[1] Status");
        assert_region_contains(&buffer, 0..44, 0..30, "[2] Repos");
        assert_region_contains(&buffer, 0..44, 0..30, "[3] Worktrees");
        assert_cell_style(&buffer, 0, 20, highlight_style().bg(Color::Reset));
        assert_region_contains(&buffer, 44..120, 0..29, "Main");
        assert_region_contains(&buffer, 0..44, 0..30, "feature");
        assert!(!line_text(&buffer, 29).contains("normal"));
    }

    #[test]
    fn renders_narrow_shell_without_panicking() {
        let config = test_config();
        let sessions = vec![test_session("feature", AgentState::Idle)];
        let model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
        let buffer = render_to_buffer(&model, 48, 12);

        assert_region_contains(&buffer, 0..48, 0..11, "[2] Repos");
        assert_cell_style(&buffer, 0, 6, highlight_style().bg(Color::Reset));
        assert_region_contains(&buffer, 0..48, 0..11, "repo");
        assert!(!line_text(&buffer, 11).contains("normal"));
    }

    #[test]
    fn renders_selected_sidebar_rows_with_focused_style() {
        let config = test_config();
        let sessions = vec![test_session("feature", AgentState::Running)];
        let model = test_model(&config, &sessions, PanelFocus::Repos, None, None);
        let buffer = render_to_buffer(&model, 120, 30);
        let row = find_line(&buffer, "1 repo");

        assert_cell_style(
            &buffer,
            4,
            row,
            Style::default()
                .fg(Color::Black)
                .bg(highlight_color())
                .add_modifier(Modifier::BOLD),
        );

        let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        let buffer = render_to_buffer(&model, 120, 30);
        let row = find_line(&buffer, "feature      ●");

        assert_cell_style(
            &buffer,
            5,
            row,
            Style::default()
                .fg(Color::Black)
                .bg(highlight_color())
                .add_modifier(Modifier::BOLD),
        );
    }

    #[test]
    fn renders_worktree_sidebar_metadata() {
        let config = test_config();
        let mut session = test_session("feature", AgentState::Running);
        session.status_label = "dirty 2 ahead 1".to_string();
        session.pr.summary = Some(test_pr_summary());
        session.pr.details = Some(PrDetails {
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
        });
        session
            .wt_columns
            .insert("todo".to_string(), "3".to_string());
        session.unseen_comments = true;
        let sessions = vec![session];
        let model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        let buffer = render_to_string(&model, 160, 30);

        assert!(buffer.contains("branch"));
        assert!(buffer.contains("A P G C @"));
        assert!(buffer.contains("⇄"));
        assert!(buffer.contains("✗"));
        assert!(buffer.contains("✕"));
        assert!(buffer.contains("1/1"));
        assert!(buffer.contains("todo:3"));
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

        assert_region_contains(&status_buffer, 44..120, 0..29, "Documentation");
        assert_region_contains(&repo_buffer, 44..120, 0..29, "view github");
        assert_region_contains(&worktree_buffer, 44..120, 0..29, "prompt implement feature");
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
                    KeyChoice {
                        key: "a".to_string(),
                        label: "one".to_string(),
                    },
                    KeyChoice {
                        key: "b".to_string(),
                        label: "two".to_string(),
                    },
                    KeyChoice {
                        key: "c".to_string(),
                        label: "three".to_string(),
                    },
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
                    KeyChoice {
                        key: "u".to_string(),
                        label: "pause/resume".to_string(),
                    },
                    KeyChoice {
                        key: "f".to_string(),
                        label: "retry failed".to_string(),
                    },
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
        assert_eq!(lines[0].spans[0].style, selected_style(true));

        model.dialog = Some(DialogModel::Confirm {
            title: "Delete Session".to_string(),
            lines: vec![DialogLine {
                text: "dirty worktree\nremove local state".to_string(),
                attention: true,
            }],
            confirm_label: "Delete".to_string(),
            cancel_label: "Cancel".to_string(),
        });
        let buffer = render_to_string(&model, 80, 20);

        assert!(buffer.contains("Delete Session"));
        assert!(buffer.contains("dirty worktree"));
        assert!(buffer.contains("remove local state"));
        assert!(buffer.contains("Enter Delete"));
        assert!(buffer.contains("Esc/q Cancel"));
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
        assert_eq!(backend.cursor_position(), Position::new(34, 8));

        model.dialog = None;
        let backend = render_to_backend(&model, 80, 20);

        assert!(!backend.cursor_visible());
    }

    #[test]
    fn renders_plan_dashboard_output_cursor_and_collapsed_tool_blocks() {
        let config = test_config();
        let sessions = vec![test_session("feature", AgentState::Running)];
        let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        model.plan_dashboard = Some(test_plan_dashboard(false));
        let buffer = render_to_string(&model, 120, 32);

        assert!(buffer.contains("Plan Run"));
        assert!(buffer.contains("phase"));
        assert!(buffer.contains("Output"));
        assert!(buffer.contains("[+]"));
        assert!(buffer.contains("L2"));
        assert!(buffer.contains("tool"));
    }

    #[test]
    fn renders_plan_dashboard_expanded_tool_block() {
        let config = test_config();
        let sessions = vec![test_session("feature", AgentState::Running)];
        let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        model.plan_dashboard = Some(test_plan_dashboard(true));
        let buffer = render_to_string(&model, 120, 32);

        assert!(buffer.contains("[-]"));
        assert!(buffer.contains("running command"));
        assert!(buffer.contains("command output"));
    }

    #[test]
    fn renders_auto_dashboard_steps_and_output_cursor() {
        let config = test_config();
        let sessions = vec![test_session("feature", AgentState::Running)];
        let mut model = test_model(&config, &sessions, PanelFocus::Worktrees, None, None);
        model.auto_dashboard = Some(test_auto_dashboard());
        let buffer = render_to_string(&model, 120, 32);

        assert!(buffer.contains("Auto Flow"));
        assert!(buffer.contains("task"));
        assert!(buffer.contains("Checklist"));
        assert!(buffer.contains("Implement \"implement feature\""));
        assert!(buffer.contains("Local validation loop"));
        assert!(buffer.contains("Run local validation"));
        assert!(buffer.contains("Output (follow)"));
        assert!(buffer.contains("auto output"));
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

    fn test_config() -> Config {
        Config {
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            auto: crate::config::AutoConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }

    fn test_session(branch: &str, agent_state: AgentState) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: Some('1'),
            path: PathBuf::from(format!("/repo/{branch}")),
            path_display: format!("/repo/{branch}"),
            branch: branch.to_string(),
            prompt_summary: "implement feature".to_string(),
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
                    repo_root: "/repo".to_string(),
                    worktree_path: session.path_display.clone(),
                    branch: session.branch.clone(),
                    kind: WorktreeKind::FeatureWorktree,
                    agent_state: session.agent_state,
                    status_label: session.status_label.clone(),
                    pr: session.pr.clone(),
                    wt_columns: session.wt_columns.clone(),
                    auto_status: None,
                    unseen_comments: session.unseen_comments,
                    prompt_summary: session.prompt_summary.clone(),
                    selected: index == 0,
                })
                .collect(),
            current_repo_index: 0,
            selected_repo_label: "repo".to_string(),
            selected_repo_root: "/repo".to_string(),
            selected_session: Some(0),
            focus,
            repo_main_view: RepoMainView::Github,
            worktree_main_view: WorktreeMainView::Details,
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
                        opencode_server_url: None,
                        opencode_session_id: Some("abcdefgh1234".to_string()),
                        process_id: None,
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
                        opencode_server_url: None,
                        opencode_session_id: None,
                        process_id: None,
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
                    id: "auto-run".to_string(),
                    repo_root: "/repo".to_string(),
                    worktree_path: PathBuf::from("/repo/feature"),
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
                    opencode_server_url: None,
                    opencode_session_id: Some("abcdefgh1234".to_string()),
                    process_id: None,
                    plan_run_id: None,
                    commit_sha: None,
                    head_sha: None,
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
}
