//! Turn execution profiles, session error classification, and memory repo extraction.
//!
//! Natural-language turn intent is deliberately not inferred here. Strong
//! runtime behavior must be driven by structured judge output or concrete
//! tool/workspace evidence, not keyword lists over user text.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use tracing::warn;

const DEFAULT_STALL_WINDOW: usize = 3;
const DEFAULT_EXPLORATION_ROUND_WINDOW: usize = 5;
const EXPLORATORY_STALL_WINDOW: usize = 4;
const EXPLORATORY_ROUND_WINDOW: usize = 8;

// Start with a useful execution slice, then renew only while observed progress
// justifies it. Large up-front budgets postpone the first convergence decision
// until after a weak model has already spent dozens of low-yield rounds.
const STANDARD_ANALYSIS_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(24, 72, 12, 4);
const COMPLEX_ANALYSIS_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(32, 96, 16, 4);
const STANDARD_IMPLEMENTATION_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(32, 96, 16, 4);
const COMPLEX_IMPLEMENTATION_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(40, 120, 20, 4);
// Exploration changes stall sensitivity, not the amount of work a user has
// authorized. Keep the same resource boundary for the same complexity.
const STANDARD_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget = STANDARD_ANALYSIS_TURN_BUDGET;
const COMPLEX_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget = COMPLEX_ANALYSIS_TURN_BUDGET;
const STANDARD_MUTATING_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget =
    STANDARD_IMPLEMENTATION_TURN_BUDGET;
const COMPLEX_MUTATING_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget =
    COMPLEX_IMPLEMENTATION_TURN_BUDGET;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    Standard,
    Complex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgenticTurnBudget {
    pub initial_turns: usize,
    pub hard_turn_limit: usize,
    pub extension_turns: usize,
    pub max_extensions: u32,
    /// Kept for wire compatibility. A hard turn limit is always a real
    /// boundary; callers may raise it explicitly when they need a longer run.
    pub renewable_past_review_limit: bool,
}

impl AgenticTurnBudget {
    pub const fn new(
        initial_turns: usize,
        hard_turn_limit: usize,
        extension_turns: usize,
        max_extensions: u32,
    ) -> Self {
        Self {
            initial_turns,
            hard_turn_limit,
            extension_turns,
            max_extensions,
            renewable_past_review_limit: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgenticTurnBudgetOverride {
    pub initial_turns: Option<usize>,
    pub hard_turn_limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskExecutionProfile {
    pub mutates_workspace: bool,
    pub verification_required: bool,
    pub exploration_round_window: usize,
    pub stall_window: usize,
    pub complexity: TaskComplexity,
    pub exploratory_task: bool,
    pub agentic_turn_budget: AgenticTurnBudget,
}

impl Default for TaskExecutionProfile {
    fn default() -> Self {
        Self {
            mutates_workspace: false,
            verification_required: false,
            exploration_round_window: DEFAULT_EXPLORATION_ROUND_WINDOW,
            stall_window: DEFAULT_STALL_WINDOW,
            complexity: TaskComplexity::Standard,
            exploratory_task: false,
            agentic_turn_budget: STANDARD_ANALYSIS_TURN_BUDGET,
        }
    }
}

#[must_use]
pub fn active_user_task_text(input: &str) -> String {
    input.trim().to_string()
}

impl TaskExecutionProfile {
    /// Build a profile from structured control-plane intent.
    ///
    /// This is the only supported path for semantic task classification.
    /// `input`-text heuristics intentionally fail closed via
    /// [`infer_task_execution_profile`].
    #[must_use]
    pub fn from_structured_intent(
        mutates_workspace: bool,
        exploratory_task: bool,
        complexity: TaskComplexity,
    ) -> Self {
        let stall_window = if exploratory_task || complexity == TaskComplexity::Complex {
            EXPLORATORY_STALL_WINDOW
        } else {
            DEFAULT_STALL_WINDOW
        };
        let exploration_round_window = if exploratory_task {
            EXPLORATORY_ROUND_WINDOW
        } else {
            DEFAULT_EXPLORATION_ROUND_WINDOW
        };
        Self {
            mutates_workspace,
            verification_required: mutates_workspace,
            exploration_round_window,
            stall_window,
            complexity,
            exploratory_task,
            agentic_turn_budget: default_agentic_turn_budget(
                mutates_workspace,
                exploratory_task,
                complexity,
            ),
        }
    }
}

/// Fallback profile for callers that don't yet have a structured intent.
///
/// Always returns `TaskExecutionProfile::default()` (standard analysis).
/// Prefer [`TaskExecutionProfile::from_structured_intent`] for semantic
/// classification; this exists only for call sites that haven't migrated.
pub fn infer_task_execution_profile(_input: &str) -> TaskExecutionProfile {
    TaskExecutionProfile::default()
}

fn default_agentic_turn_budget(
    mutates_workspace: bool,
    exploratory: bool,
    complexity: TaskComplexity,
) -> AgenticTurnBudget {
    match (mutates_workspace, exploratory, complexity) {
        (false, false, TaskComplexity::Standard) => STANDARD_ANALYSIS_TURN_BUDGET,
        (false, false, TaskComplexity::Complex) => COMPLEX_ANALYSIS_TURN_BUDGET,
        (true, false, TaskComplexity::Standard) => STANDARD_IMPLEMENTATION_TURN_BUDGET,
        (true, false, TaskComplexity::Complex) => COMPLEX_IMPLEMENTATION_TURN_BUDGET,
        (false, true, TaskComplexity::Standard) => STANDARD_EXPLORATORY_TURN_BUDGET,
        (false, true, TaskComplexity::Complex) => COMPLEX_EXPLORATORY_TURN_BUDGET,
        (true, true, TaskComplexity::Standard) => STANDARD_MUTATING_EXPLORATORY_TURN_BUDGET,
        (true, true, TaskComplexity::Complex) => COMPLEX_MUTATING_EXPLORATORY_TURN_BUDGET,
    }
}

#[must_use]
pub fn resolve_agentic_turn_budget(
    profile: TaskExecutionProfile,
    runtime_ceiling: usize,
    override_budget: Option<AgenticTurnBudgetOverride>,
) -> AgenticTurnBudget {
    let ceiling = runtime_ceiling.max(1);
    let mut budget = profile.agentic_turn_budget;
    let requested_hard = override_budget
        .and_then(|value| value.hard_turn_limit)
        .unwrap_or(budget.hard_turn_limit);
    let hard_turn_limit = requested_hard.max(1).min(ceiling);
    if requested_hard > hard_turn_limit {
        warn!(
            requested_hard,
            ceiling,
            hard_turn_limit,
            "agentic turn budget override clamped: hard_turn_limit reduced to runtime ceiling"
        );
    }
    let requested_initial = override_budget
        .and_then(|value| value.initial_turns)
        .unwrap_or(budget.initial_turns);
    let initial_turns = requested_initial.max(1).min(hard_turn_limit);
    if requested_initial > initial_turns {
        warn!(
            requested_initial,
            hard_turn_limit,
            initial_turns,
            "agentic turn budget override clamped: initial_turns reduced to hard_turn_limit"
        );
    }
    let headroom = hard_turn_limit.saturating_sub(initial_turns);
    let extension_turns = if headroom == 0 {
        0
    } else {
        budget.extension_turns.max(1).min(headroom)
    };
    let max_extensions = if extension_turns == 0 {
        0
    } else {
        let max_possible = headroom.div_ceil(extension_turns) as u32;
        budget.max_extensions.min(max_possible).max(1)
    };
    budget.initial_turns = initial_turns;
    budget.hard_turn_limit = hard_turn_limit;
    budget.extension_turns = extension_turns;
    budget.max_extensions = max_extensions;
    budget.renewable_past_review_limit = false;
    budget
}

/// Resolve the adaptive execution slices for an isolated child while keeping
/// one deterministic, administrator-owned runaway boundary.
///
/// Children use the same task profile as a root run. The process runtime
/// ceiling is explicit here so progress can extend the initial slice but can
/// never renew beyond that ceiling. Cost/token totals are observability, not a
/// second hidden execution policy.
#[must_use]
pub fn resolve_isolated_agentic_turn_budget(
    profile: TaskExecutionProfile,
    runtime_ceiling: usize,
) -> AgenticTurnBudget {
    resolve_isolated_agentic_turn_budget_with_initial_slice(profile, runtime_ceiling, None)
}

/// Resolve an isolated child's adaptive budget with an optional initial-slice
/// hint.
///
/// Agent personas use the hint to choose the first convergence checkpoint; it
/// is not a semantic hard stop. Only a caller-owned explicit numeric limit is
/// allowed to become a hard boundary, and that is handled by
/// [`resolve_agentic_turn_budget`] at the execution owner. This distinction
/// prevents a read-only persona default from silently truncating a productive
/// child run while retaining one administrator-owned runaway ceiling.
#[must_use]
pub fn resolve_isolated_agentic_turn_budget_with_initial_slice(
    profile: TaskExecutionProfile,
    runtime_ceiling: usize,
    initial_slice: Option<usize>,
) -> AgenticTurnBudget {
    let mut budget = resolve_agentic_turn_budget(
        profile,
        runtime_ceiling,
        Some(AgenticTurnBudgetOverride {
            initial_turns: initial_slice,
            hard_turn_limit: Some(runtime_ceiling),
        }),
    );
    if budget.extension_turns > 0 {
        let headroom = budget.hard_turn_limit.saturating_sub(budget.initial_turns);
        budget.max_extensions = headroom
            .div_ceil(budget.extension_turns)
            .min(u32::MAX as usize) as u32;
    }
    budget
}

/// Resolve the one shared child-run budget protocol used by local and server
/// executors.
///
/// `initial_slice` is always a scheduling checkpoint. `explicit_hard_limit`
/// is present only when the caller supplied a numeric max-turns constraint;
/// persona defaults and qualitative complexity must pass `None`.
#[must_use]
pub fn resolve_spawned_agentic_turn_budget(
    profile: TaskExecutionProfile,
    runtime_ceiling: usize,
    initial_slice: usize,
    explicit_hard_limit: Option<usize>,
) -> AgenticTurnBudget {
    let Some(explicit_hard_limit) = explicit_hard_limit else {
        return resolve_isolated_agentic_turn_budget_with_initial_slice(
            profile,
            runtime_ceiling,
            Some(initial_slice),
        );
    };
    resolve_agentic_turn_budget(
        profile,
        runtime_ceiling,
        Some(AgenticTurnBudgetOverride {
            initial_turns: Some(initial_slice.min(explicit_hard_limit)),
            hard_turn_limit: Some(explicit_hard_limit),
        }),
    )
}

/// Cloud API returned no such session (case-insensitive substring match).
pub fn is_session_not_found_error(error: &str) -> bool {
    error.to_lowercase().contains("session not found")
}

/// Extract `owner/repo` patterns from memory text.
pub fn extract_repos_from_memory(text: &str) -> Vec<String> {
    static GITHUB_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)github\.com/([a-zA-Z0-9][\w-]{0,38})/([a-zA-Z0-9][\w.-]{0,99})")
            .expect("github url regex")
    });

    static BARE_REPO_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b([a-zA-Z0-9][\w-]{0,38})/([a-zA-Z0-9][\w.-]{0,99})\b")
            .expect("repo regex")
    });

    let mut repos = Vec::new();
    let mut seen = HashSet::new();

    let mut add = |owner: &str, repo: &str| {
        let full = format!("{owner}/{repo}");
        let key = full.to_lowercase();
        if seen.insert(key) {
            repos.push(full);
        }
    };

    for cap in GITHUB_URL_RE.captures_iter(text) {
        add(&cap[1], &cap[2]);
    }

    for cap in BARE_REPO_RE.captures_iter(text) {
        let owner = &cap[1];
        let repo = &cap[2];
        if [
            "http", "https", "ftp", "ssh", "git", "usr", "etc", "var", "tmp", "home",
        ]
        .contains(&owner.to_lowercase().as_str())
        {
            continue;
        }
        if owner.contains('.') {
            continue;
        }
        let match_start = cap.get(0).expect("group 0 always exists").start();
        if text[..match_start].ends_with('@') {
            continue;
        }
        add(owner, repo);
    }

    repos
}

// ── Shared prompt text normalization ───────────────────────────────────────
//
/// Trim trailing punctuation, ellipsis markers, and Chinese tone particles
/// from a user message.
pub fn trim_trailing_punctuation(s: &str) -> &str {
    s.trim().trim_end_matches(|ch: char| {
        matches!(
            ch,
            '?' | '？'
                | '!'
                | '！'
                | '.'
                | '。'
                | ','
                | '，'
                | '啊'
                | '呀'
                | '呢'
                | '吧'
                | '嘛'
                | '啦'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_language_profile_inference_fails_closed() {
        let cases = [
            "implement the feature",
            "fix the bug",
            "修复这个问题",
            "review the current changes and fix any issues",
            "当前的实现，能够想起来吗？",
        ];
        for input in cases {
            assert_eq!(
                infer_task_execution_profile(input),
                TaskExecutionProfile::default(),
                "natural-language text must not produce execution policy: {input}"
            );
        }
    }

    #[test]
    fn structured_profiles_scale_budget_for_mutation_and_exploration() {
        let standard = TaskExecutionProfile::default();
        let implementation =
            TaskExecutionProfile::from_structured_intent(true, false, TaskComplexity::Standard);
        assert!(implementation.mutates_workspace);
        assert!(implementation.verification_required);
        assert!(
            implementation.agentic_turn_budget.initial_turns
                > standard.agentic_turn_budget.initial_turns
        );
        assert!(
            implementation.agentic_turn_budget.hard_turn_limit
                > standard.agentic_turn_budget.hard_turn_limit
        );

        let exploratory =
            TaskExecutionProfile::from_structured_intent(false, true, TaskComplexity::Complex);
        assert!(exploratory.exploration_round_window > standard.exploration_round_window);
        assert!(exploratory.stall_window > standard.stall_window);
        assert!(exploratory.exploratory_task);
    }

    #[test]
    fn resolve_agentic_turn_budget_clamps_override_to_runtime_ceiling() {
        let profile =
            TaskExecutionProfile::from_structured_intent(true, false, TaskComplexity::Standard);
        let budget = resolve_agentic_turn_budget(
            profile,
            12,
            Some(AgenticTurnBudgetOverride {
                initial_turns: Some(20),
                hard_turn_limit: Some(30),
            }),
        );
        assert_eq!(budget.initial_turns, 12);
        assert_eq!(budget.hard_turn_limit, 12);
        assert_eq!(budget.max_extensions, 0);
        assert!(
            !budget.renewable_past_review_limit,
            "an explicit caller limit must remain a real resource boundary"
        );
        let default_budget = resolve_agentic_turn_budget(profile, 1_000, None);
        assert!(
            !default_budget.renewable_past_review_limit,
            "a hard turn limit must not silently become an unbounded renewable checkpoint"
        );
    }

    #[test]
    fn isolated_budget_uses_profile_slice_with_nonrenewable_runtime_ceiling() {
        let profile = TaskExecutionProfile::default();
        let budget = resolve_isolated_agentic_turn_budget(profile, 300);

        assert_eq!(
            budget.initial_turns,
            profile.agentic_turn_budget.initial_turns
        );
        assert_eq!(budget.hard_turn_limit, 300);
        assert!(!budget.renewable_past_review_limit);
        assert!(
            budget.initial_turns
                + budget.extension_turns * usize::try_from(budget.max_extensions).unwrap()
                >= budget.hard_turn_limit,
            "a progressing isolated run must be able to consume bounded slices through the administrator ceiling"
        );

        let clamped = resolve_isolated_agentic_turn_budget(profile, 24);
        assert_eq!(clamped.initial_turns, 24);
        assert_eq!(clamped.hard_turn_limit, 24);
        assert_eq!(clamped.max_extensions, 0);

        let persona_slice =
            resolve_isolated_agentic_turn_budget_with_initial_slice(profile, 300, Some(12));
        assert_eq!(persona_slice.initial_turns, 12);
        assert_eq!(persona_slice.hard_turn_limit, 300);
        assert!(
            persona_slice.initial_turns
                + persona_slice.extension_turns
                    * usize::try_from(persona_slice.max_extensions).unwrap()
                >= persona_slice.hard_turn_limit
        );
    }

    #[test]
    fn session_not_found_detection() {
        assert!(is_session_not_found_error("Session not found"));
        assert!(is_session_not_found_error("error: SESSION NOT FOUND"));
        assert!(!is_session_not_found_error("authentication failed"));
        assert!(!is_session_not_found_error(""));
    }

    #[test]
    fn extract_repos_explicit_owner_repo() {
        let text = "user follows matrixorigin/Memoria and wants to track their projects";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos, vec!["matrixorigin/Memoria"]);
    }

    #[test]
    fn extract_repos_multiple() {
        let text = "tracks matrixorigin/Memoria and also watches rust-lang/rust";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains(&"matrixorigin/Memoria".to_string()));
        assert!(repos.contains(&"rust-lang/rust".to_string()));
    }

    #[test]
    fn extract_repos_dedup() {
        let text = "matrixorigin/Memoria and MATRIXORIGIN/memoria again";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos.len(), 1, "should deduplicate case-insensitively");
    }

    #[test]
    fn extract_repos_skips_tag_namespaces() {
        let text = "[@pref/active] user follows matrixorigin/Memoria";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos, vec!["matrixorigin/Memoria"]);
        assert!(
            !repos.iter().any(|r| r.contains("pref")),
            "should not extract @pref/active as a repo"
        );
    }

    #[test]
    fn extract_repos_skips_protocols() {
        let text = "see https://github.com/matrixorigin/Memoria for details";
        let repos = extract_repos_from_memory(text);
        assert!(repos.iter().any(|r| r == "matrixorigin/Memoria"));
        assert!(!repos.iter().any(|r| r.to_lowercase().contains("http")));
    }

    #[test]
    fn extract_repos_empty_for_no_repos() {
        let text = "user prefers concise responses and dark mode";
        let repos = extract_repos_from_memory(text);
        assert!(repos.is_empty());
    }

    #[test]
    fn extract_repos_handles_hyphen() {
        let text = "watching my-org/my-project and also some-user/cool-lib";
        let repos = extract_repos_from_memory(text);
        assert!(repos.iter().any(|r| r == "my-org/my-project"));
        assert!(repos.iter().any(|r| r == "some-user/cool-lib"));
    }

    // ── trim_trailing_punctuation ────────────────────────────────────────

    #[test]
    fn trim_trailing_punctuation_removes_chinese_tone_particles() {
        assert_eq!(trim_trailing_punctuation("继续吧"), "继续");
        assert_eq!(trim_trailing_punctuation("好的呢"), "好的");
        assert_eq!(trim_trailing_punctuation("行啊"), "行");
    }

    #[test]
    fn trim_trailing_punctuation_removes_trailing_punctuation() {
        assert_eq!(trim_trailing_punctuation("继续？"), "继续");
        assert_eq!(trim_trailing_punctuation("继续！"), "继续");
        assert_eq!(trim_trailing_punctuation("go."), "go");
    }

    #[test]
    fn trim_trailing_punctuation_preserves_non_trailing() {
        assert_eq!(trim_trailing_punctuation("继续啊呀"), "继续");
        assert_eq!(trim_trailing_punctuation("next step"), "next step");
    }
}
