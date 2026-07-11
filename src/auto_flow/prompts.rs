use super::*;

pub(super) fn prompt_for_step(config: &Config, run: &AutoRun, step: &AutoStepRun) -> String {
    let (template_name, default) = match step.step_key {
        AutoStepKey::CreatePlan => ("auto_create_plan", auto_create_plan_prompt(run)),
        AutoStepKey::ReviewPlan => ("auto_review_plan", auto_review_plan_prompt(run)),
        AutoStepKey::Implement => ("auto_implement", auto_implementation_prompt(run)),
        AutoStepKey::FixLocalVerify => ("auto_fix_local_verify", auto_verify_fix_prompt(run, step)),
        AutoStepKey::FixReview => ("auto_fix_review", auto_review_fix_prompt(run, step)),
        AutoStepKey::FixCi => ("auto_fix_ci", auto_ci_fix_prompt(run, step)),
        _ => {
            return step
                .reason
                .clone()
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| run.initial_prompt.clone());
        }
    };
    render_auto_prompt(
        config.prompt_template(template_name).unwrap_or(&default),
        run,
        step,
    )
}

fn render_auto_prompt(template: &str, run: &AutoRun, step: &AutoStepRun) -> String {
    let plan_path = plan_first_plan_path(run).display().to_string();
    let values = [
        ("{{task}}", run.initial_prompt.as_str()),
        ("{{plan_path}}", plan_path.as_str()),
        ("{{mode}}", run.mode.as_str()),
        ("{{variant}}", run.variant.as_str()),
        (
            "{{agent_profile}}",
            run.agent_profile.as_deref().unwrap_or("default"),
        ),
        (
            "{{context}}",
            step.reason
                .as_deref()
                .unwrap_or("No details were recorded."),
        ),
        ("{{branch}}", run.branch.as_str()),
    ];
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while !rest.is_empty() {
        if let Some((key, value)) = values.iter().find(|(key, _)| rest.starts_with(key)) {
            rendered.push_str(value);
            rest = &rest[key.len()..];
        } else {
            let character = rest.chars().next().expect("rest is not empty");
            rendered.push(character);
            rest = &rest[character.len_utf8()..];
        }
    }
    rendered
}

pub(super) fn auto_create_plan_prompt(run: &AutoRun) -> String {
    let plan_path = plan_first_plan_path(run);
    format!(
        "Create an implementation plan for the following task. Write the plan to `{}` in this repository. Do not implement the task, commit, push, create a pull request, or merge.\n\nThe plan should be concrete enough for automated execution and include phases, tests, verification, risks, observability needs, and architecture fit. Keep repository conventions and existing domain language in mind.\n\nTask:\n{}\n\nMode: {}\nVariant: {}\nAgent profile: {}",
        plan_path.display(),
        run.initial_prompt,
        run.mode.as_str(),
        run.variant,
        run.agent_profile.as_deref().unwrap_or("default")
    )
}

pub(super) fn auto_review_plan_prompt(run: &AutoRun) -> String {
    let plan_path = plan_first_plan_path(run);
    format!(
        "Review `{}` for the Auto Flow task below. Edit the plan in place so it is ready for implementation. Do not implement the task, commit, push, create a pull request, or merge.\n\nReview for missing phases, hidden risks, test strategy, observability, restartability, safety boundaries, and architecture fit with this repository. Preserve useful details and tighten vague steps.\n\nTask:\n{}\n\nMode: {}\nVariant: {}\nAgent profile: {}",
        plan_path.display(),
        run.initial_prompt,
        run.mode.as_str(),
        run.variant,
        run.agent_profile.as_deref().unwrap_or("default")
    )
}

pub(super) fn auto_implementation_prompt(run: &AutoRun) -> String {
    if run.mode == AutoRunMode::PlanFirst {
        let plan_path = plan_first_plan_path(run);
        format!(
            "Implement the approved plan in `{}` for this Auto Flow task. Stop after the implementation changes are complete; do not commit, push, create a pull request, or merge.\n\nOriginal task:\n{}",
            plan_path.display(),
            run.initial_prompt
        )
    } else {
        format!(
            "Implement the following task in this worktree. Stop after the implementation changes are complete; do not commit, push, create a pull request, or merge.\n\nTask:\n{}",
            run.initial_prompt
        )
    }
}

pub(super) fn auto_verify_fix_prompt(run: &AutoRun, step: &AutoStepRun) -> String {
    format!(
        "Local verification failed for this Auto Flow run. Fix the failures, then stop without committing.\n\nOriginal task:\n{}\n\nFailure context:\n{}",
        run.initial_prompt,
        step.reason
            .as_deref()
            .unwrap_or("No verification details were recorded.")
    )
}

pub(super) fn auto_review_fix_prompt(run: &AutoRun, step: &AutoStepRun) -> String {
    format!(
        "Resolve the review feedback for this branch, then stop without committing.\n\nOriginal task:\n{}\n\nReview context:\n{}",
        run.initial_prompt,
        step.reason
            .as_deref()
            .unwrap_or("No review details were recorded.")
    )
}

pub(super) fn auto_ci_fix_prompt(run: &AutoRun, step: &AutoStepRun) -> String {
    format!(
        "CI failed for this branch. Fix the failure, then stop without committing.\n\nOriginal task:\n{}\n\nCI context:\n{}",
        run.initial_prompt,
        step.reason
            .as_deref()
            .unwrap_or("No CI details were recorded.")
    )
}
