//! Goal Completion Tracker
//!
//! Lightweight heuristic tracker that complements drift detection.
//! Tracks progress toward the user's original goal by observing
//! milestone signals from tool results, user feedback, and turn flow.
//!
//! Zero LLM cost — uses TF-IDF keyword relevance from `text_tokenize`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use astra_services::session_workspace::{
    GoalMilestoneSignalSnapshot, GoalMilestoneSnapshot, GoalProgressSnapshot,
};

use astra_text_utils::text_tokenize;

// ─── Data Types ─────────────────────────────────────────────────────────────

/// A signal that indicates progress toward (or away from) the goal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MilestoneSignal {
    /// A tool call succeeded (tool_name, brief description).
    ToolSuccess(String, String),
    /// Tests passed (count).
    TestPass(u32),
    /// Tests failed (count).
    TestFail(u32),
    /// A file was created or modified (path).
    FileChanged(String),
    /// A git commit was made (message snippet).
    CommitMade(String),
    /// User expressed approval (e.g., "good", "perfect", "thanks").
    UserApproval,
    /// User expressed disapproval (e.g., "no", "wrong", "revert").
    UserDisapproval,
    /// Build succeeded.
    BuildSuccess,
    /// Build failed.
    BuildFail,
}

/// A recorded milestone with relevance to the goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Milestone {
    /// Turn number when this milestone occurred.
    pub turn: u32,
    /// The signal type.
    pub signal: MilestoneSignal,
    /// Relevance to the original goal (0.0–1.0).
    pub relevance: f64,
}

/// Goal completion analysis result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalProgress {
    /// Overall estimated completion (0.0–1.0).
    pub completion_score: f64,
    /// Momentum: positive = making progress, negative = regressing.
    pub momentum: f64,
    /// Number of milestones recorded.
    pub milestone_count: usize,
    /// Summary of recent progress.
    pub summary: String,
}

// ─── GoalTracker ────────────────────────────────────────────────────────────

/// Tracks progress toward a session's original goal.
///
/// Heuristic scoring:
/// - Each milestone contributes `weight × relevance` to the score
/// - Positive signals (success, approval) increase the score
/// - Negative signals (fail, disapproval) decrease momentum
/// - Relevance is computed via TF-IDF similarity between the
///   milestone description and the original goal keywords
pub struct GoalTracker {
    /// The original user goal (first message).
    original_goal: String,
    /// TF vector for the goal (cached for efficiency).
    goal_tf: HashMap<String, f64>,
    /// Recorded milestones.
    milestones: Vec<Milestone>,
    /// Running weighted score accumulator.
    weighted_progress: f64,
    /// Running negative signal accumulator.
    negative_signals: f64,
}

impl GoalTracker {
    /// Create a new tracker for the given goal.
    pub fn new(goal: &str) -> Self {
        let tokens = text_tokenize::tokenize(goal);
        let goal_tf = text_tokenize::build_tf(&tokens);
        Self {
            original_goal: goal.to_string(),
            goal_tf,
            milestones: Vec::new(),
            weighted_progress: 0.0,
            negative_signals: 0.0,
        }
    }

    /// Record a milestone signal at a given turn.
    pub fn record(&mut self, turn: u32, signal: MilestoneSignal) {
        let description = signal_description(&signal);
        let relevance = self.compute_relevance(&description);
        let weight = signal_weight(&signal);

        if weight > 0.0 {
            self.weighted_progress += weight * relevance.max(0.3);
        } else {
            self.negative_signals += weight.abs() * relevance.max(0.3);
        }

        self.milestones.push(Milestone {
            turn,
            signal,
            relevance,
        });
    }

    /// Get the current progress analysis.
    pub fn progress(&self) -> GoalProgress {
        if self.milestones.is_empty() {
            return GoalProgress {
                completion_score: 0.0,
                momentum: 0.0,
                milestone_count: 0,
                summary: "No milestones recorded yet.".to_string(),
            };
        }

        // Score: sigmoid-like curve that approaches 1.0 as milestones accumulate.
        // weighted_progress is unbounded, so we map through a soft-ceiling.
        let raw = self.weighted_progress - self.negative_signals * 0.5;
        let completion = soft_ceiling(raw.max(0.0));

        // Momentum: compare last 3 milestones vs previous 3.
        let momentum = self.compute_momentum();

        let summary = self.build_summary(completion, momentum);

        GoalProgress {
            completion_score: completion,
            momentum,
            milestone_count: self.milestones.len(),
            summary,
        }
    }

    /// Get all recorded milestones.
    pub fn milestones(&self) -> &[Milestone] {
        &self.milestones
    }

    /// Get the original goal text.
    pub fn goal(&self) -> &str {
        &self.original_goal
    }

    /// Export the tracker into a persisted snapshot for workspace resume.
    pub fn snapshot(&self) -> GoalProgressSnapshot {
        let progress = self.progress();
        GoalProgressSnapshot {
            goal: self.original_goal.clone(),
            completion_score: progress.completion_score,
            momentum: progress.momentum,
            milestone_count: progress.milestone_count,
            summary: progress.summary,
            weighted_progress: self.weighted_progress,
            negative_signals: self.negative_signals,
            milestones: self
                .milestones
                .iter()
                .map(GoalMilestoneSnapshot::from)
                .collect(),
        }
    }

    /// Restore a tracker from a persisted workspace snapshot.
    pub fn from_snapshot(snapshot: &GoalProgressSnapshot) -> Self {
        let tokens = text_tokenize::tokenize(&snapshot.goal);
        let goal_tf = text_tokenize::build_tf(&tokens);
        let milestones = snapshot
            .milestones
            .iter()
            .cloned()
            .map(Milestone::from)
            .collect::<Vec<_>>();
        let (weighted_progress, negative_signals) = if milestones.is_empty() {
            (snapshot.weighted_progress, snapshot.negative_signals)
        } else {
            accumulated_progress(&milestones)
        };

        Self {
            original_goal: snapshot.goal.clone(),
            goal_tf,
            milestones,
            weighted_progress,
            negative_signals,
        }
    }

    // ── Internal ────────────────────────────────────────────────────────────

    /// Compute relevance of a description to the goal using TF-IDF cosine.
    fn compute_relevance(&self, description: &str) -> f64 {
        if self.goal_tf.is_empty() || description.is_empty() {
            return 0.0;
        }
        let tokens = text_tokenize::tokenize(description);
        if tokens.is_empty() {
            return 0.0;
        }
        let desc_tf = text_tokenize::build_tf(&tokens);
        cosine_sim(&self.goal_tf, &desc_tf)
    }

    /// Momentum: weighted signal direction of recent milestones.
    fn compute_momentum(&self) -> f64 {
        let recent_window = 5;
        let n = self.milestones.len();
        if n == 0 {
            return 0.0;
        }
        let start = n.saturating_sub(recent_window);
        let recent = &self.milestones[start..];

        let mut pos = 0.0_f64;
        let mut neg = 0.0_f64;
        for m in recent {
            let w = signal_weight(&m.signal);
            if w > 0.0 {
                pos += w * m.relevance.max(0.2);
            } else {
                neg += w.abs() * m.relevance.max(0.2);
            }
        }

        let total = pos + neg;
        if total < 0.01 {
            0.0
        } else {
            (pos - neg) / total
        }
    }

    fn build_summary(&self, completion: f64, momentum: f64) -> String {
        let phase = if completion < 0.2 {
            "Just started"
        } else if completion < 0.5 {
            "Making progress"
        } else if completion < 0.8 {
            "Well underway"
        } else {
            "Nearing completion"
        };

        let trend = if momentum > 0.3 {
            " (positive momentum)"
        } else if momentum < -0.3 {
            " (struggling)"
        } else {
            ""
        };

        format!("{phase}{trend} — {:.0}% estimated", completion * 100.0)
    }
}

// ─── Signal Classification ──────────────────────────────────────────────────

/// Weight of a signal. Positive = progress, negative = regression.
fn signal_weight(signal: &MilestoneSignal) -> f64 {
    match signal {
        MilestoneSignal::ToolSuccess(_, _) => 0.15,
        MilestoneSignal::TestPass(n) => 0.3 + (*n as f64 * 0.01).min(0.2),
        MilestoneSignal::TestFail(_) => -0.15,
        MilestoneSignal::FileChanged(_) => 0.1,
        MilestoneSignal::CommitMade(_) => 0.4,
        MilestoneSignal::UserApproval => 0.5,
        MilestoneSignal::UserDisapproval => -0.3,
        MilestoneSignal::BuildSuccess => 0.25,
        MilestoneSignal::BuildFail => -0.2,
    }
}

/// Extract a text description from a signal for relevance computation.
fn signal_description(signal: &MilestoneSignal) -> String {
    match signal {
        MilestoneSignal::ToolSuccess(name, desc) => format!("{name} {desc}"),
        MilestoneSignal::TestPass(n) => format!("{n} tests passed"),
        MilestoneSignal::TestFail(n) => format!("{n} tests failed"),
        MilestoneSignal::FileChanged(path) => path.clone(),
        MilestoneSignal::CommitMade(msg) => msg.clone(),
        MilestoneSignal::UserApproval => "user approved".to_string(),
        MilestoneSignal::UserDisapproval => "user rejected".to_string(),
        MilestoneSignal::BuildSuccess => "build succeeded".to_string(),
        MilestoneSignal::BuildFail => "build failed".to_string(),
    }
}

fn accumulated_progress(milestones: &[Milestone]) -> (f64, f64) {
    let mut weighted_progress = 0.0;
    let mut negative_signals = 0.0;
    for milestone in milestones {
        let weight = signal_weight(&milestone.signal);
        if weight > 0.0 {
            weighted_progress += weight * milestone.relevance.max(0.3);
        } else {
            negative_signals += weight.abs() * milestone.relevance.max(0.3);
        }
    }
    (weighted_progress, negative_signals)
}

impl From<&MilestoneSignal> for GoalMilestoneSignalSnapshot {
    fn from(signal: &MilestoneSignal) -> Self {
        match signal {
            MilestoneSignal::ToolSuccess(tool, detail) => Self::ToolSuccess {
                tool: tool.clone(),
                detail: detail.clone(),
            },
            MilestoneSignal::TestPass(count) => Self::TestPass { count: *count },
            MilestoneSignal::TestFail(count) => Self::TestFail { count: *count },
            MilestoneSignal::FileChanged(path) => Self::FileChanged { path: path.clone() },
            MilestoneSignal::CommitMade(message) => Self::CommitMade {
                message: message.clone(),
            },
            MilestoneSignal::UserApproval => Self::UserApproval,
            MilestoneSignal::UserDisapproval => Self::UserDisapproval,
            MilestoneSignal::BuildSuccess => Self::BuildSuccess,
            MilestoneSignal::BuildFail => Self::BuildFail,
        }
    }
}

impl From<GoalMilestoneSignalSnapshot> for MilestoneSignal {
    fn from(signal: GoalMilestoneSignalSnapshot) -> Self {
        match signal {
            GoalMilestoneSignalSnapshot::ToolSuccess { tool, detail } => {
                Self::ToolSuccess(tool, detail)
            }
            GoalMilestoneSignalSnapshot::TestPass { count } => Self::TestPass(count),
            GoalMilestoneSignalSnapshot::TestFail { count } => Self::TestFail(count),
            GoalMilestoneSignalSnapshot::FileChanged { path } => Self::FileChanged(path),
            GoalMilestoneSignalSnapshot::CommitMade { message } => Self::CommitMade(message),
            GoalMilestoneSignalSnapshot::UserApproval => Self::UserApproval,
            GoalMilestoneSignalSnapshot::UserDisapproval => Self::UserDisapproval,
            GoalMilestoneSignalSnapshot::BuildSuccess => Self::BuildSuccess,
            GoalMilestoneSignalSnapshot::BuildFail => Self::BuildFail,
        }
    }
}

impl From<&Milestone> for GoalMilestoneSnapshot {
    fn from(milestone: &Milestone) -> Self {
        Self {
            turn: milestone.turn,
            signal: GoalMilestoneSignalSnapshot::from(&milestone.signal),
            relevance: milestone.relevance,
        }
    }
}

impl From<GoalMilestoneSnapshot> for Milestone {
    fn from(milestone: GoalMilestoneSnapshot) -> Self {
        Self {
            turn: milestone.turn,
            signal: milestone.signal.into(),
            relevance: milestone.relevance,
        }
    }
}

/// Soft ceiling: maps [0, ∞) → [0, 1) with diminishing returns.
/// f(x) = 1 - e^(-kx), k chosen so f(2.0) ≈ 0.8.
fn soft_ceiling(x: f64) -> f64 {
    let k = 0.8; // f(2.0) ≈ 0.80
    (1.0 - (-k * x).exp()).clamp(0.0, 1.0)
}

/// Cosine similarity between two TF vectors.
fn cosine_sim(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (term, &c1) in a {
        norm_a += c1 * c1;
        if let Some(&c2) = b.get(term) {
            dot += c1 * c2;
        }
    }
    for &c2 in b.values() {
        norm_b += c2 * c2;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-9 {
        0.0
    } else {
        (dot / denom).min(1.0)
    }
}

// ─── Detect Milestone Signals from Tool Results ─────────────────────────────

/// Heuristic: detect milestone signals from a tool call result.
///
/// Examines tool name + output text to classify the result.
pub fn detect_signal(
    tool_name: &str,
    output: &str,
    exit_code: Option<i32>,
) -> Option<MilestoneSignal> {
    let name_lower = tool_name.to_ascii_lowercase();
    let output_lower = output.to_ascii_lowercase();

    // Git commit detection
    if name_lower.contains("bash") || name_lower.contains("shell") {
        if let Some(msg) = detect_git_commit(&output_lower, output) {
            return Some(MilestoneSignal::CommitMade(msg));
        }
    }

    // Test results detection
    if let Some(signal) = detect_test_result(&output_lower, exit_code) {
        return signal;
    }

    // Build detection
    if let Some(signal) = detect_build_result(&name_lower, &output_lower, exit_code) {
        return Some(signal);
    }

    // File change detection
    if name_lower.contains("write") || name_lower.contains("create") || name_lower.contains("edit")
    {
        let path = output.lines().next().unwrap_or("").trim();
        if !path.is_empty() && path.len() < 200 {
            return Some(MilestoneSignal::FileChanged(path.to_string()));
        }
    }

    // Generic tool success
    if exit_code == Some(0) || (exit_code.is_none() && !output_lower.contains("error")) {
        let desc: String = output.chars().take(80).collect();
        return Some(MilestoneSignal::ToolSuccess(tool_name.to_string(), desc));
    }

    None
}

/// Detect user approval/disapproval from a query.
pub fn detect_user_sentiment(query: &str) -> Option<MilestoneSignal> {
    let q = query.to_ascii_lowercase();

    let approval_phrases = [
        "good",
        "great",
        "perfect",
        "thanks",
        "thank you",
        "nice",
        "awesome",
        "lgtm",
        "looks good",
        "well done",
        "correct",
        "好的",
        "不错",
        "很好",
        "正确",
        "谢谢",
        "可以",
    ];
    let disapproval_phrases = [
        "wrong", "no", "revert", "undo", "bad", "broken", "不对", "错了", "不行", "回退", "撤销",
    ];

    for phrase in &approval_phrases {
        if q.contains(phrase) {
            return Some(MilestoneSignal::UserApproval);
        }
    }
    for phrase in &disapproval_phrases {
        if q.contains(phrase) {
            return Some(MilestoneSignal::UserDisapproval);
        }
    }
    None
}

// ── Internal detection helpers ──────────────────────────────────────────────

fn detect_git_commit(output_lower: &str, output: &str) -> Option<String> {
    // Pattern: "[branch hash] commit message"
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.contains(']') {
            if let Some(msg_start) = trimmed.find("] ") {
                let msg: String = trimmed[msg_start + 2..].chars().take(80).collect();
                if !msg.is_empty() {
                    return Some(msg);
                }
            }
        }
    }
    if output_lower.contains("create mode") && output_lower.contains("-->") {
        return Some("git commit".to_string());
    }
    None
}

fn detect_test_result(
    output_lower: &str,
    exit_code: Option<i32>,
) -> Option<Option<MilestoneSignal>> {
    // Rust test result pattern: "test result: ok. N passed"
    if output_lower.contains("test result:") {
        if let Some(count) = extract_test_count(output_lower, "passed") {
            if output_lower.contains("0 failed") || exit_code == Some(0) {
                return Some(Some(MilestoneSignal::TestPass(count)));
            }
        }
        if let Some(count) = extract_test_count(output_lower, "failed") {
            if count > 0 {
                return Some(Some(MilestoneSignal::TestFail(count)));
            }
        }
    }
    // npm/jest/pytest patterns
    if output_lower.contains("tests passed") || output_lower.contains("passing") {
        if exit_code == Some(0) {
            return Some(Some(MilestoneSignal::TestPass(1)));
        }
    }
    None
}

fn extract_test_count(text: &str, label: &str) -> Option<u32> {
    // Match patterns like "985 passed" or "5 failed".
    // Use rfind to skip labels that appear earlier (e.g., "test result: FAILED").
    let idx = text.rfind(label)?;
    let before = text[..idx].trim_end();
    let num_str: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let num_str: String = num_str.chars().rev().collect();
    num_str.parse().ok()
}

fn detect_build_result(
    name_lower: &str,
    output_lower: &str,
    exit_code: Option<i32>,
) -> Option<MilestoneSignal> {
    let is_build_context =
        name_lower.contains("bash") || name_lower.contains("shell") || name_lower.contains("build");
    if !is_build_context {
        return None;
    }

    let build_keywords = [
        "cargo build",
        "cargo check",
        "npm run build",
        "make",
        "go build",
    ];
    let is_build = build_keywords.iter().any(|kw| output_lower.contains(kw));
    if !is_build {
        return None;
    }

    if exit_code == Some(0) || output_lower.contains("finished") {
        Some(MilestoneSignal::BuildSuccess)
    } else if exit_code.is_some() && exit_code != Some(0) {
        Some(MilestoneSignal::BuildFail)
    } else {
        None
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_goal_tracker_basic_progress() {
        let mut tracker = GoalTracker::new("implement user authentication with JWT");

        // Simulate a session working on auth
        tracker.record(0, MilestoneSignal::FileChanged("src/auth.rs".to_string()));
        tracker.record(
            1,
            MilestoneSignal::ToolSuccess("bash".to_string(), "cargo check".to_string()),
        );
        tracker.record(2, MilestoneSignal::TestPass(5));
        tracker.record(
            3,
            MilestoneSignal::CommitMade("add JWT authentication module".to_string()),
        );

        let progress = tracker.progress();
        assert!(
            progress.completion_score > 0.0,
            "should have positive completion: {}",
            progress.completion_score
        );
        assert!(progress.momentum > 0.0, "should have positive momentum");
        assert_eq!(progress.milestone_count, 4);
    }

    #[test]
    fn test_goal_tracker_empty() {
        let tracker = GoalTracker::new("implement something");
        let progress = tracker.progress();
        assert_eq!(progress.completion_score, 0.0);
        assert_eq!(progress.momentum, 0.0);
        assert_eq!(progress.milestone_count, 0);
    }

    #[test]
    fn test_goal_tracker_negative_signals() {
        let mut tracker = GoalTracker::new("fix database connection");

        tracker.record(0, MilestoneSignal::BuildFail);
        tracker.record(1, MilestoneSignal::TestFail(3));
        tracker.record(2, MilestoneSignal::UserDisapproval);

        let progress = tracker.progress();
        assert!(
            progress.momentum < 0.0,
            "should have negative momentum: {}",
            progress.momentum
        );
    }

    #[test]
    fn test_goal_tracker_recovery() {
        let mut tracker = GoalTracker::new("fix database connection");

        // Failure then recovery
        tracker.record(0, MilestoneSignal::BuildFail);
        tracker.record(1, MilestoneSignal::TestFail(2));
        tracker.record(2, MilestoneSignal::BuildSuccess);
        tracker.record(3, MilestoneSignal::TestPass(10));
        tracker.record(4, MilestoneSignal::UserApproval);

        let progress = tracker.progress();
        assert!(
            progress.completion_score > 0.1,
            "recovery should show progress"
        );
        assert!(
            progress.momentum > 0.0,
            "momentum should be positive after recovery"
        );
    }

    #[test]
    fn test_goal_tracker_snapshot_round_trip() {
        let mut tracker = GoalTracker::new("implement user authentication with JWT");
        tracker.record(0, MilestoneSignal::FileChanged("src/auth.rs".to_string()));
        tracker.record(1, MilestoneSignal::TestPass(12));
        tracker.record(2, MilestoneSignal::BuildSuccess);

        let snapshot = tracker.snapshot();
        let restored = GoalTracker::from_snapshot(&snapshot);
        let progress = restored.progress();

        assert_eq!(restored.goal(), "implement user authentication with JWT");
        assert_eq!(progress.milestone_count, 3);
        assert!(
            (progress.completion_score - snapshot.completion_score).abs() < 0.0001,
            "completion score should survive round-trip"
        );
        assert!(
            (progress.momentum - snapshot.momentum).abs() < 0.0001,
            "momentum should survive round-trip"
        );
    }

    #[test]
    fn test_soft_ceiling() {
        assert!((soft_ceiling(0.0) - 0.0).abs() < 0.01);
        assert!(soft_ceiling(1.0) > 0.4);
        assert!(soft_ceiling(2.0) > 0.7);
        assert!(soft_ceiling(5.0) > 0.95);
        assert!(soft_ceiling(10.0) < 1.0);
    }

    #[test]
    fn test_detect_signal_git_commit() {
        let output = "[migrate_to_rust f90fe3c6] upgrade drift detection\n 1 file changed";
        let signal = detect_signal("bash", output, Some(0));
        assert!(matches!(signal, Some(MilestoneSignal::CommitMade(_))));
    }

    #[test]
    fn test_detect_signal_test_pass() {
        let output = "test result: ok. 985 passed; 0 failed; 0 ignored";
        let signal = detect_signal("bash", output, Some(0));
        assert!(matches!(signal, Some(MilestoneSignal::TestPass(985))));
    }

    #[test]
    fn test_detect_signal_test_fail() {
        let output = "test result: FAILED. 980 passed; 5 failed; 0 ignored";
        let signal = detect_signal("bash", output, Some(101));
        assert!(matches!(signal, Some(MilestoneSignal::TestFail(5))));
    }

    #[test]
    fn test_detect_signal_build_success() {
        let output = "cargo build\n    Finished `dev` profile";
        let signal = detect_signal("bash", output, Some(0));
        assert!(matches!(signal, Some(MilestoneSignal::BuildSuccess)));
    }

    #[test]
    fn test_detect_user_sentiment() {
        assert!(matches!(
            detect_user_sentiment("looks good, thanks!"),
            Some(MilestoneSignal::UserApproval)
        ));
        assert!(matches!(
            detect_user_sentiment("这个不对，回退"),
            Some(MilestoneSignal::UserDisapproval)
        ));
        assert!(detect_user_sentiment("implement auth").is_none());
    }

    #[test]
    fn test_extract_test_count() {
        assert_eq!(
            extract_test_count("985 passed; 0 failed", "passed"),
            Some(985)
        );
        assert_eq!(
            extract_test_count("985 passed; 0 failed", "failed"),
            Some(0)
        );
        assert_eq!(extract_test_count("no match here", "passed"), None);
    }

    #[test]
    fn test_relevance_related_milestone() {
        let tracker = GoalTracker::new("implement user authentication with JWT");
        let high = tracker.compute_relevance("add JWT authentication module");
        let low = tracker.compute_relevance("configure kubernetes deployment");
        assert!(
            high > low,
            "auth milestone should be more relevant than k8s: {high} vs {low}"
        );
    }

    #[test]
    fn test_chinese_goal_tracking() {
        let mut tracker = GoalTracker::new("实现用户认证功能");
        tracker.record(0, MilestoneSignal::FileChanged("src/auth.rs".to_string()));
        tracker.record(1, MilestoneSignal::TestPass(3));
        let progress = tracker.progress();
        assert!(progress.completion_score > 0.0);
    }
}
