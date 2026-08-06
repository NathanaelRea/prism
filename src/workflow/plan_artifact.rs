#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::run::{ArtifactInput, Sensitivity, TrustClass};

const PLAN_ARTIFACT_TYPE: &str = "builtin:plan@1";

/// Immutable, validated payload stored as `builtin:plan@1`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct PlanManifest {
    pub schema_version: u32,
    pub title: String,
    pub source_display: String,
    pub content: String,
    pub phases: Vec<PlanPhase>,
    pub selected_phase_ids: Vec<String>,
    pub max_fan_out: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct PlanPhase {
    pub id: String,
    pub display: String,
    pub dependencies: Vec<String>,
    pub body: String,
}

impl PlanManifest {
    /// Reads and copies a plan once. The path is diagnostic metadata only and is
    /// never retained as execution authority.
    pub(crate) fn from_file(
        path: &Path,
        selected: Option<std::ops::RangeInclusive<usize>>,
        max_fan_out: u32,
    ) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|error| format!("read Plan {}: {error}", path.display()))?;
        Self::parse(&content, &path.display().to_string(), selected, max_fan_out)
    }

    pub(crate) fn parse(
        content: &str,
        source_display: &str,
        selected: Option<std::ops::RangeInclusive<usize>>,
        max_fan_out: u32,
    ) -> Result<Self, String> {
        if max_fan_out == 0 {
            return Err("Plan max_fan_out must be greater than zero".to_string());
        }
        let mut headings = Vec::new();
        for (line_index, line) in content.lines().enumerate() {
            let heading = line.trim_start().trim_start_matches('#').trim_start();
            let Some(rest) = heading.strip_prefix("Phase ") else {
                continue;
            };
            let number_len = rest.bytes().take_while(u8::is_ascii_digit).count();
            if number_len == 0 {
                continue;
            }
            let number = rest[..number_len]
                .parse::<usize>()
                .map_err(|_| "invalid Phase number")?;
            let display = rest[number_len..]
                .trim_start_matches([':', '-', ' '])
                .trim();
            let explicit = display
                .rsplit_once("{#")
                .and_then(|(_, id)| id.strip_suffix('}'));
            let id = explicit
                .map(str::to_string)
                .unwrap_or_else(|| format!("phase-{number}"));
            validate_id(&id)?;
            let display = explicit
                .and_then(|_| display.rsplit_once(" {#").map(|(text, _)| text))
                .unwrap_or(display)
                .trim()
                .to_string();
            headings.push((line_index, number, id, display));
        }
        if headings.is_empty() {
            return Err("Plan contains no headings like 'Phase 1: description'".to_string());
        }
        let lines = content.lines().collect::<Vec<_>>();
        let mut phases = Vec::with_capacity(headings.len());
        let mut ids = BTreeSet::new();
        for (index, (start, _number, id, display)) in headings.iter().enumerate() {
            if !ids.insert(id.clone()) {
                return Err(format!("duplicate Plan phase ID '{id}'"));
            }
            let end = headings
                .get(index + 1)
                .map_or(lines.len(), |heading| heading.0);
            let body = lines[start + 1..end].join("\n").trim().to_string();
            // Ordinary numbered plans are sequential by default. An explicit
            // `Depends on: none` opts a phase into a parallel root, while an
            // explicit list describes the exact DAG edges.
            let dependencies = body
                .lines()
                .find_map(parse_dependencies)
                .unwrap_or_else(|| {
                    phases
                        .last()
                        .map(|phase: &PlanPhase| vec![phase.id.clone()])
                        .unwrap_or_default()
                });
            phases.push(PlanPhase {
                id: id.clone(),
                display: display.clone(),
                dependencies,
                body,
            });
        }
        validate_graph(&phases)?;
        validate_fan_out(&phases, max_fan_out)?;
        let selected_phase_ids: Vec<String> = match selected {
            Some(range) => {
                if range.is_empty() || *range.start() == 0 || *range.end() > phases.len() {
                    return Err(format!(
                        "selected phase range must be within 1..={}",
                        phases.len()
                    ));
                }
                phases[*range.start() - 1..*range.end()]
                    .iter()
                    .map(|phase| phase.id.clone())
                    .collect()
            }
            None => phases.iter().map(|phase| phase.id.clone()).collect(),
        };
        let selected_set = selected_phase_ids.iter().collect::<BTreeSet<_>>();
        for phase in phases
            .iter()
            .filter(|phase| selected_set.contains(&phase.id))
        {
            if let Some(dependency) = phase
                .dependencies
                .iter()
                .find(|dependency| !selected_set.contains(dependency))
            {
                return Err(format!(
                    "selected phase '{}' excludes dependency '{dependency}'",
                    phase.id
                ));
            }
        }
        let title = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("# "))
            .unwrap_or("Plan")
            .to_string();
        Ok(Self {
            schema_version: 1,
            title,
            source_display: source_display.to_string(),
            content: content.to_string(),
            phases,
            selected_phase_ids,
            max_fan_out,
        })
    }

    /// Builds the immutable Task payload consumed by `builtin:create-plan@1`.
    /// The file is copied by the launching caller; execution never receives a
    /// mutable path from which to rediscover authority.
    pub(crate) fn launch_task_from_file(
        path: &Path,
        selected: Option<std::ops::RangeInclusive<usize>>,
        max_fan_out: u32,
    ) -> Result<ArtifactInput, String> {
        let manifest = Self::from_file(path, selected, max_fan_out)?;
        Ok(ArtifactInput {
            name: "task".to_string(),
            artifact_type: "builtin:task@1".to_string(),
            payload: serde_json::json!({"plan_manifest": manifest}),
            trust: TrustClass::Trusted,
            sensitivity: Sensitivity::Internal,
        })
    }

    pub(crate) fn from_task(payload: &serde_json::Value) -> Result<Self, String> {
        let value = payload
            .get("plan_manifest")
            .ok_or_else(|| "Plan Task must contain an immutable plan_manifest".to_string())?;
        let manifest: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("decode Plan manifest: {error}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported Plan manifest schema {}",
                self.schema_version
            ));
        }
        if self.content.is_empty() {
            return Err("Plan content cannot be empty".to_string());
        }
        if self.selected_phase_ids.is_empty() {
            return Err("Plan must select at least one phase".to_string());
        }
        validate_graph(&self.phases)?;
        validate_fan_out(&self.phases, self.max_fan_out)?;
        let reparsed = Self::parse(&self.content, &self.source_display, None, self.max_fan_out)?;
        if reparsed.title != self.title || reparsed.phases != self.phases {
            return Err(
                "Plan manifest structure does not match its immutable Markdown content".to_string(),
            );
        }
        let ids = self
            .phases
            .iter()
            .map(|phase| phase.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut selected = BTreeSet::new();
        for id in &self.selected_phase_ids {
            if !ids.contains(id.as_str()) {
                return Err(format!("selected Plan phase '{id}' is missing"));
            }
            if !selected.insert(id) {
                return Err(format!("selected Plan phase '{id}' is duplicated"));
            }
        }
        for phase in self
            .phases
            .iter()
            .filter(|phase| selected.contains(&phase.id))
        {
            if let Some(dependency) = phase.dependencies.iter().find(|id| !selected.contains(id)) {
                return Err(format!(
                    "selected phase '{}' excludes dependency '{dependency}'",
                    phase.id
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn phase_manifest(&self, phase_id: &str) -> Result<Self, String> {
        if !self.selected_phase_ids.iter().any(|id| id == phase_id) {
            return Err(format!("Plan phase '{phase_id}' is not selected"));
        }
        let mut manifest = self.clone();
        manifest.selected_phase_ids = vec![phase_id.to_string()];
        // Dependencies remain immutable evidence but are enforced by the
        // parent child-run scheduler, so a one-phase child need not select its
        // ancestors again.
        Ok(manifest)
    }

    pub(crate) fn selected_phases(&self) -> impl Iterator<Item = &PlanPhase> {
        self.selected_phase_ids
            .iter()
            .filter_map(|id| self.phases.iter().find(|phase| &phase.id == id))
    }

    pub(crate) fn into_artifact(self, trust: TrustClass) -> ArtifactInput {
        ArtifactInput {
            name: "plan".to_string(),
            artifact_type: PLAN_ARTIFACT_TYPE.to_string(),
            payload: serde_json::to_value(self).expect("Plan Manifest serializes"),
            trust,
            sensitivity: Sensitivity::Internal,
        }
    }
}

fn parse_dependencies(line: &str) -> Option<Vec<String>> {
    let value = line.trim().strip_prefix("Depends on:")?.trim();
    Some(if value.eq_ignore_ascii_case("none") || value.is_empty() {
        Vec::new()
    } else {
        value
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn validate_id(id: &str) -> Result<(), String> {
    let valid = id
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("invalid Plan phase ID '{id}'"))
    }
}

fn validate_fan_out(phases: &[PlanPhase], max_fan_out: u32) -> Result<(), String> {
    if max_fan_out == 0 {
        return Err("Plan max_fan_out must be greater than zero".to_string());
    }
    let mut completed = BTreeSet::new();
    let mut maximum_width = 0;
    while completed.len() < phases.len() {
        let ready = phases
            .iter()
            .filter(|phase| {
                !completed.contains(&phase.id)
                    && phase
                        .dependencies
                        .iter()
                        .all(|dependency| completed.contains(dependency))
            })
            .map(|phase| phase.id.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            // Cycle and missing-edge diagnostics belong to validate_graph.
            break;
        }
        maximum_width = maximum_width.max(ready.len());
        completed.extend(ready);
    }
    if maximum_width > max_fan_out as usize {
        return Err(format!(
            "Plan dependency graph exposes {maximum_width} concurrent phases but max_fan_out is {max_fan_out}"
        ));
    }
    Ok(())
}

fn validate_graph(phases: &[PlanPhase]) -> Result<(), String> {
    let ids = phases
        .iter()
        .map(|phase| phase.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut indegree = phases
        .iter()
        .map(|phase| (phase.id.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = BTreeMap::<&str, Vec<&str>>::new();
    for phase in phases {
        for dependency in &phase.dependencies {
            validate_id(dependency)?;
            if !ids.contains(dependency.as_str()) {
                return Err(format!(
                    "Plan phase '{}' depends on missing phase '{dependency}'",
                    phase.id
                ));
            }
            *indegree.get_mut(phase.id.as_str()).unwrap() += 1;
            outgoing.entry(dependency).or_default().push(&phase.id);
        }
    }
    let mut queue = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(id) = queue.pop_front() {
        visited += 1;
        for child in outgoing.get(id).into_iter().flatten() {
            let degree = indegree.get_mut(child).unwrap();
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(child);
            }
        }
    }
    if visited == phases.len() {
        Ok(())
    } else {
        Err("Plan phase dependencies contain a cycle".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_content_and_validates_stable_dependencies() {
        let plan = PlanManifest::parse("# Work\n## Phase 1: Build {#build}\ntext\n## Phase 2: Test {#test}\nDepends on: build\n", "plan.md", None, 2).unwrap();
        assert_eq!(plan.selected_phase_ids, ["build", "test"]);
        assert_eq!(plan.phases[1].dependencies, ["build"]);
        assert!(plan.content.contains("Phase 1"));
    }

    #[test]
    fn selection_cannot_drop_a_dependency() {
        let error = PlanManifest::parse(
            "# Phase 1: Build\n# Phase 2: Test\nDepends on: phase-1",
            "plan.md",
            Some(2..=2),
            1,
        )
        .unwrap_err();
        assert!(error.contains("excludes dependency"));
    }

    #[test]
    fn plan_text_is_data_not_workflow_structure() {
        let plan = PlanManifest::parse(
            "# Phase 1: Work\ncapabilities = [\"merge\"]\n[[steps]]",
            "plan.md",
            None,
            1,
        )
        .unwrap();
        assert_eq!(plan.phases.len(), 1);
        assert!(plan.phases[0].body.contains("capabilities"));
    }

    #[test]
    fn ordinary_numbered_phases_are_sequential_by_default() {
        let plan = PlanManifest::parse(
            "# Plan\n## Phase 1: One {#one}\na\n## Phase 2: Two {#two}\nb\n## Phase 3: Three {#three}\nc",
            "plan.md",
            None,
            1,
        )
        .unwrap();
        assert!(plan.phases[0].dependencies.is_empty());
        assert_eq!(plan.phases[1].dependencies, ["one"]);
        assert_eq!(plan.phases[2].dependencies, ["two"]);
    }

    #[test]
    fn explicit_parallelism_is_bounded() {
        let error = PlanManifest::parse(
            "# Plan\n## Phase 1: One {#one}\nDepends on: none\n## Phase 2: Two {#two}\nDepends on: none",
            "plan.md",
            None,
            1,
        )
        .unwrap_err();
        assert!(error.contains("2 concurrent phases"), "{error}");
    }

    #[test]
    fn launch_task_survives_source_change_and_rejects_manifest_tampering() {
        let path = std::env::temp_dir().join(format!(
            "prism-plan-artifact-{}-{}.md",
            std::process::id(),
            crate::run::now_ms()
        ));
        fs::write(&path, "# Plan\n## Phase 1: Build\noriginal").unwrap();
        let task = PlanManifest::launch_task_from_file(&path, None, 1).unwrap();
        fs::write(&path, "changed").unwrap();
        fs::remove_file(path).unwrap();
        let copied = PlanManifest::from_task(&task.payload).unwrap();
        assert!(copied.content.contains("original"));

        let mut tampered = task.payload;
        tampered["plan_manifest"]["phases"][0]["body"] = serde_json::json!("merge everything");
        assert!(
            PlanManifest::from_task(&tampered)
                .unwrap_err()
                .contains("does not match")
        );
    }
}
