//! Execution-time helpers for prompting and subtask scheduling.

use astra_services::VerifierKind;

use crate::{SubtaskPlan, TaskPlan};

/// Build the executor prompt for a subtask, optionally prefixed with stacked
/// operator guidance from prior pause/correction turns.
pub fn format_subtask_prompt_with_operator_notes(
    subtask: &SubtaskPlan,
    operator_notes: &[String],
) -> String {
    let mut body = format!("Execute this subtask: {}\n", subtask.title);

    if let Some(ref desc) = subtask.description {
        body.push_str(&format!("\nDescription: {}\n", desc));
    }

    if !subtask.files.is_empty() {
        body.push_str(&format!(
            "\nFiles to modify: {}\n",
            subtask.files.join(", ")
        ));
    }

    if !subtask.acceptance_checks.is_empty() {
        body.push_str("\nAcceptance checks (automated verification will run these):\n");
        for (i, vk) in subtask.acceptance_checks.iter().enumerate() {
            let desc = match vk {
                VerifierKind::FileExists { paths } => format!("Files exist: {}", paths.join(", ")),
                VerifierKind::ReadFileContains { path, contains, .. } => {
                    format!("{path} contains {:?}", contains)
                }
                VerifierKind::GrepCheck {
                    file,
                    pattern,
                    should_match,
                } => {
                    if *should_match {
                        format!("grep '{pattern}' matches in {file}")
                    } else {
                        format!("grep '{pattern}' must NOT match in {file}")
                    }
                }
                VerifierKind::Command { cmd, .. } => format!("Command succeeds: {cmd}"),
                VerifierKind::CommandOutput { cmd, contains, .. } => {
                    format!("{cmd} output contains {:?}", contains)
                }
                VerifierKind::BuildPass { cmd } => format!("Build: {cmd}"),
                VerifierKind::TestPass { cmd, .. } => format!("Test: {cmd}"),
                _ => "Automated check".into(),
            };
            body.push_str(&format!("  {}. {}\n", i + 1, desc));
        }
    }

    body.push_str(
        "\nPlease implement this change. Read the relevant files first, \
         make the changes, and verify they compile/pass tests.\n\
         Before referencing any project type, function, struct, or API in new code, \
         confirm it exists using read_file, grep, or LSP tools. Do not assume symbol names.\n\
         \n\
         IMPORTANT — how to produce code:\n\
         - Emit actual file mutations as tool_calls: `write_file`, `str_replace`, \
           `create_file`, or `bash` (for mkdir / scaffolding). DO NOT paste \
           implementation code inside the assistant response as markdown code blocks \
           — markdown is inert and does not modify the filesystem.\n\
         - After writing any new file, confirm it exists (`read_file` or `bash ls`) \
           before declaring the subtask done.\n\
         - `skill` and `discover_skills` are advisory: consulting a skill does NOT \
           satisfy the subtask. The subtask is only complete when concrete files \
           have been written and any acceptance check passes.\n\
         - Do not invoke `github_create_pr` (or any PR-creation skill) before you \
         have actually written and committed the changes this subtask requires.",
    );

    if operator_notes.is_empty() {
        return body;
    }
    let mut block = String::from(
        "[Operator guidance — follow for this subtask unless unsafe; reconcile with the task text.]\n",
    );
    for (i, note) in operator_notes.iter().enumerate() {
        block.push_str(&format!("{}. {}\n", i + 1, note));
    }
    format!("{block}\n{body}")
}

/// Analysis of which subtasks can run in parallel.
#[derive(Debug, Clone)]
pub struct ParallelGroups {
    /// Groups of subtask IDs that can execute concurrently.
    /// Each group contains subtasks that are all ready and have no file conflicts.
    pub groups: Vec<Vec<String>>,
    /// File conflicts detected: (subtask_a, subtask_b, shared_files).
    pub conflicts: Vec<FileConflict>,
}

/// Two subtasks targeting overlapping files.
#[derive(Debug, Clone)]
pub struct FileConflict {
    pub subtask_a: String,
    pub subtask_b: String,
    pub shared_files: Vec<String>,
}

/// Analyze a plan to find parallelizable subtask groups and file conflicts.
pub fn analyze_parallelism(plan: &TaskPlan) -> ParallelGroups {
    let ready = plan.ready_subtasks();
    if ready.len() <= 1 {
        return ParallelGroups {
            groups: if ready.is_empty() {
                vec![]
            } else {
                vec![vec![ready[0].id.clone()]]
            },
            conflicts: vec![],
        };
    }

    let mut conflicts = Vec::new();
    for i in 0..ready.len() {
        for j in (i + 1)..ready.len() {
            let shared: Vec<String> = ready[i]
                .files
                .iter()
                .filter(|f| ready[j].files.contains(f))
                .cloned()
                .collect();
            if !shared.is_empty() {
                conflicts.push(FileConflict {
                    subtask_a: ready[i].id.clone(),
                    subtask_b: ready[j].id.clone(),
                    shared_files: shared,
                });
            }
        }
    }

    let conflict_pairs: std::collections::HashSet<(String, String)> = conflicts
        .iter()
        .flat_map(|c| {
            vec![
                (c.subtask_a.clone(), c.subtask_b.clone()),
                (c.subtask_b.clone(), c.subtask_a.clone()),
            ]
        })
        .collect();

    let mut groups: Vec<Vec<String>> = Vec::new();
    for st in &ready {
        let mut placed = false;
        for group in &mut groups {
            let has_conflict = group
                .iter()
                .any(|g_id| conflict_pairs.contains(&(g_id.clone(), st.id.clone())));
            if !has_conflict {
                group.push(st.id.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push(vec![st.id.clone()]);
        }
    }

    ParallelGroups { groups, conflicts }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_subtask_prompt_minimal() {
        let st = SubtaskPlan {
            id: "t1".into(),
            title: "Add login page".into(),
            ..Default::default()
        };
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
        assert!(prompt.contains("Add login page"));
        assert!(prompt.contains("implement this change"));
        assert!(!prompt.contains("Description:"));
        assert!(!prompt.contains("Files to modify:"));
        assert!(!prompt.contains("Acceptance checks"));
    }

    #[test]
    fn format_subtask_prompt_full() {
        let st = SubtaskPlan {
            id: "t2".into(),
            title: "Add auth middleware".into(),
            description: Some("JWT token validation for all /api routes".into()),
            files: vec!["src/middleware.rs".into(), "src/auth.rs".into()],
            acceptance_checks: vec![VerifierKind::GrepCheck {
                file: "src/middleware.rs".into(),
                pattern: "401".into(),
                should_match: true,
            }],
            ..Default::default()
        };
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
        assert!(prompt.contains("Add auth middleware"));
        assert!(prompt.contains("JWT token validation"));
        assert!(prompt.contains("src/middleware.rs, src/auth.rs"));
        assert!(
            prompt.contains("401"),
            "should mention 401 from acceptance checks"
        );
    }

    #[test]
    fn format_subtask_prompt_preserves_description_detail() {
        let st = SubtaskPlan {
            id: "t3".into(),
            title: "Refactor DB layer".into(),
            description: Some(
                "Extract connection pooling into a separate module.\nAdd retry logic.".into(),
            ),
            ..Default::default()
        };
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
        assert!(prompt.contains("Extract connection pooling"));
        assert!(prompt.contains("retry logic"));
    }

    #[test]
    fn prompt_policy_is_invariant_to_untyped_subtask_wording() {
        fn policy_suffix(prompt: &str) -> &str {
            prompt
                .split_once("\nPlease implement this change.")
                .map(|(_, suffix)| suffix)
                .expect("common executor policy")
        }

        let wordings = [
            ("arbitrary task", "arbitrary details"),
            (
                "Test game in browser",
                "Open the page and verify keyboard input works.",
            ),
            ("用浏览器测试页面", "打开页面并检查交互。"),
        ];
        let suffixes: Vec<String> = wordings
            .iter()
            .map(|(title, description)| {
                let subtask = SubtaskPlan {
                    id: "wording-invariant".into(),
                    title: (*title).into(),
                    description: Some((*description).into()),
                    ..Default::default()
                };
                let prompt = format_subtask_prompt_with_operator_notes(&subtask, &[]);
                policy_suffix(&prompt).to_string()
            })
            .collect();

        assert!(
            suffixes.windows(2).all(|pair| pair[0] == pair[1]),
            "free-form title/description wording must not infer execution or verification policy"
        );
    }

    #[test]
    fn parallel_groups_no_deps_all_parallel() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert_eq!(analysis.groups.len(), 1, "all should be in one group");
        assert_eq!(analysis.groups[0].len(), 3);
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn parallel_groups_with_file_conflict() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    files: vec!["src/main.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    files: vec!["src/main.rs".into(), "src/lib.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    files: vec!["src/other.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert!(!analysis.conflicts.is_empty(), "should detect a-b conflict");
        assert!(
            analysis.conflicts[0]
                .shared_files
                .contains(&"src/main.rs".to_string())
        );
        assert!(
            analysis.groups.len() >= 2,
            "should split conflicting subtasks: {:?}",
            analysis.groups
        );
    }

    #[test]
    fn parallel_groups_single_subtask() {
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "only".into(),
                title: "Only one".into(),
                ..Default::default()
            }],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert_eq!(analysis.groups.len(), 1);
        assert_eq!(analysis.groups[0], vec!["only"]);
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn parallel_groups_respects_dependency_filter() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert_eq!(analysis.groups.len(), 1);
        assert_eq!(analysis.groups[0], vec!["a"]);
    }
}
