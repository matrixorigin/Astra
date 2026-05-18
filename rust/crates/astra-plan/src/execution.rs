//! Execution-time helpers for prompting and subtask scheduling.

use astra_services::{
    durable_task::VerifierKind,
    task_orchestrator::{SubtaskPlan, TaskPlan},
};

/// Returns true when a subtask explicitly calls for real browser/UI verification.
pub fn subtask_requires_browser_verification(subtask: &SubtaskPlan) -> bool {
    let mut text = subtask.title.to_lowercase();
    if let Some(desc) = &subtask.description {
        text.push('\n');
        text.push_str(&desc.to_lowercase());
    }

    let strong_browser = [
        "browser",
        "in browser",
        "浏览器",
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
    ]
    .iter()
    .any(|needle| text.contains(needle));

    let weak_browser = !strong_browser
        && [" web page", "web ui", "in the dom", "html canvas", "页面"]
            .iter()
            .any(|needle| text.contains(needle));

    let mentions_browser = strong_browser || weak_browser;
    let mentions_verification = [
        "test in browser",
        "verify in browser",
        "test",
        "verify",
        "validation",
        "validate",
        "check",
        "qa",
        "smoke",
        "open in",
        "测试",
        "验证",
        "检查",
        "打开",
        "试玩",
    ]
    .iter()
    .any(|needle| text.contains(needle));

    mentions_browser && mentions_verification
}

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

    if subtask_requires_browser_verification(subtask) {
        body.push_str(
            "\n\
             IMPORTANT — this subtask explicitly requires browser/UI verification:\n\
             - `curl`, `grep`, `head`, `ps`, or starting a local HTTP server do NOT count as browser verification.\n\
             - Only mark the subtask done after collecting evidence from a real browser-capable tool or workflow \
               (for example: Playwright, Selenium, Puppeteer, Cypress, a browser headless screenshot, \
               or a browser DOM dump after real page execution).\n\
             - If no browser-capable tool is available in this environment, say that plainly instead of claiming \
               the browser behavior was verified.\n",
        );
    }

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
    fn browser_verification_subtask_prompt_requires_real_browser_evidence() {
        let st = SubtaskPlan {
            id: "t4".into(),
            title: "Test game in browser".into(),
            description: Some(
                "Open the page, play a round, and verify keyboard input works.".into(),
            ),
            ..Default::default()
        };
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
        assert!(
            prompt.contains("requires browser/UI verification"),
            "prompt should surface explicit browser-verification guidance: {prompt}"
        );
        assert!(
            prompt.contains("curl") && prompt.contains("do NOT count as browser verification"),
            "prompt should explicitly reject curl-style checks as sufficient evidence: {prompt}"
        );
        assert!(
            prompt.contains("Playwright") || prompt.contains("browser headless screenshot"),
            "prompt should name acceptable browser-capable evidence: {prompt}"
        );
    }

    #[test]
    fn browser_verification_no_false_positive_on_non_browser_tasks() {
        for title in [
            "Run database migration for user page",
            "Build UI component library",
            "Run unit tests for the pagination module",
            "Check DOM manipulation in JSDOM tests",
            "Run canvas rendering benchmark",
        ] {
            let st = SubtaskPlan {
                id: "t1".into(),
                title: title.into(),
                description: None,
                ..Default::default()
            };
            assert!(
                !subtask_requires_browser_verification(&st),
                "should NOT trigger browser verification for: {title}"
            );
        }
    }

    #[test]
    fn browser_verification_true_positive_on_real_browser_tasks() {
        for title in [
            "Test game in browser",
            "Verify the web page renders correctly",
            "Open in browser and check layout",
            "用浏览器测试页面",
            "Run Playwright tests for login flow",
        ] {
            let st = SubtaskPlan {
                id: "t1".into(),
                title: title.into(),
                description: None,
                ..Default::default()
            };
            assert!(
                subtask_requires_browser_verification(&st),
                "should trigger browser verification for: {title}"
            );
        }
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
