//! Entity Knowledge Graph — persistent entity→domain→tools mapping.
//!
//! Learns entity associations from successful tool interactions and
//! uses them to improve routing confidence on subsequent queries.
//!
//! # Learning flow
//!
//! 1. User says "我关注matrixorigin" → extract_entities() → ["matrixorigin"]
//! 2. First encounter: EntityGraph has no knowledge → low confidence
//! 3. Agent uses GitHub tools successfully → learn("matrixorigin", GitHub, ["github_search"])
//! 4. Next query about "matrixorigin" → domain_for() → Some(GitHub) → high confidence
//!
//! # Cross-session persistence
//!
//! The EntityGraph can be serialized/deserialized for storage in Memoria or
//! other persistence layers. At session start, merge previous knowledge.
//! At session end, export learned knowledge.

use super::routing::DomainHint;
use std::collections::HashMap;

// ─── Entity Knowledge ────────────────────────────────────────────────────────

/// Knowledge about a single entity, learned from successful interactions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityKnowledge {
    /// Canonical entity name (lowercased).
    pub name: String,
    /// Known aliases (e.g., "mo" → "matrixorigin").
    pub aliases: Vec<String>,
    /// Domain this entity belongs to.
    pub domain: Option<DomainHint>,
    /// Tools that successfully handled queries about this entity.
    pub associated_tools: Vec<String>,
    /// Confidence in this knowledge (0.0–1.0), increases with observations.
    pub confidence: f64,
    /// Number of times this entity was observed in successful interactions.
    pub observation_count: u32,
    /// Unix timestamp of last observation (seconds since epoch).
    #[serde(default)]
    pub last_observed_at: u64,
}

/// Days after which entity confidence starts decaying.
const ENTITY_DECAY_GRACE_DAYS: u64 = 14;
/// Half-life in days for confidence decay after grace period.
const ENTITY_DECAY_HALF_LIFE_DAYS: f64 = 60.0;

impl EntityKnowledge {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            domain: None,
            associated_tools: Vec::new(),
            confidence: 0.0,
            observation_count: 0,
            last_observed_at: current_entity_timestamp(),
        }
    }

    /// Get time-decayed confidence.
    ///
    /// Entities not observed for a long time have reduced confidence,
    /// reflecting uncertainty about whether the association is still valid.
    pub fn decayed_confidence(&self) -> f64 {
        let decay = entity_time_decay_factor(self.last_observed_at);
        self.confidence * decay
    }

    /// Time decay factor (0.0–1.0) based on staleness.
    pub fn time_decay_factor(&self) -> f64 {
        entity_time_decay_factor(self.last_observed_at)
    }

    /// Update the last_observed_at timestamp to now.
    pub fn touch(&mut self) {
        self.last_observed_at = current_entity_timestamp();
    }
}

/// Calculate time decay factor for entity confidence (0.0–1.0).
fn entity_time_decay_factor(last_observed_at: u64) -> f64 {
    let now = current_entity_timestamp();
    if last_observed_at >= now {
        return 1.0;
    }

    let age_secs = now - last_observed_at;
    let age_days = age_secs as f64 / 86400.0;

    if age_days <= ENTITY_DECAY_GRACE_DAYS as f64 {
        return 1.0;
    }

    let days_past_grace = age_days - ENTITY_DECAY_GRACE_DAYS as f64;
    0.5_f64.powf(days_past_grace / ENTITY_DECAY_HALF_LIFE_DAYS)
}

/// Get current Unix timestamp in seconds.
fn current_entity_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Entity Graph ────────────────────────────────────────────────────────────

/// In-memory entity knowledge graph.
///
/// Maps entity names to their known domains, tools, and confidence.
/// Designed to be populated from memory service at session start and
/// exported at session end.
#[derive(Debug, Clone, Default)]
pub struct EntityGraph {
    /// Entity name (lowercased) → knowledge.
    entities: HashMap<String, EntityKnowledge>,
    /// Alias → canonical name mapping.
    alias_index: HashMap<String, String>,
    /// Entities modified since last sync (for delta export).
    dirty_entities: std::collections::HashSet<String>,
    /// Unix timestamp of last successful sync export.
    last_sync_epoch: u64,
}

impl EntityGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Learn from a successful tool interaction.
    ///
    /// Called after the Evaluate stage confirms good progress.
    /// Strengthens the association between entity, domain, and tools.
    ///
    /// If `user_feedback_score` is provided (0-100 scale), it modulates confidence growth:
    /// - High feedback (≥50) allows normal confidence growth
    /// - Low feedback (<50) dampens confidence growth (user unhappy despite success)
    pub fn learn(
        &mut self,
        entity: &str,
        domain: DomainHint,
        tools_used: &[String],
        user_feedback_score: Option<i64>,
    ) {
        let key = entity.to_lowercase();
        let entry = self
            .entities
            .entry(key.clone())
            .or_insert_with(|| EntityKnowledge::new(&key));

        entry.domain = Some(domain);
        entry.observation_count += 1;

        // Add new tools (dedup)
        for tool in tools_used {
            if !entry.associated_tools.contains(tool) {
                entry.associated_tools.push(tool.clone());
            }
        }

        // Base confidence grows with observations (asymptotic to 1.0)
        let base_confidence = 1.0 - 1.0 / (1.0 + entry.observation_count as f64);

        // Apply feedback modulation:
        // - No feedback: use base confidence
        // - Low feedback (<50): dampen confidence growth by 30%
        // - High feedback (≥50): allow full confidence
        entry.confidence = match user_feedback_score {
            Some(score) if score < 50 => {
                // User unhappy → reduce confidence growth
                let dampening = 0.7; // retain 70% of growth
                let prev = entry.confidence;
                prev + (base_confidence - prev) * dampening
            }
            _ => base_confidence,
        };

        // Update timestamp to reflect recent observation
        entry.touch();

        // Mark as dirty for delta sync
        self.dirty_entities.insert(key);
    }

    /// Record that an entity-tool association failed.
    ///
    /// Reduces confidence for the entity mapping (dampened by 0.8×, floored at 0.1).
    /// Does nothing if the entity is unknown — we don't create entries from failures.
    pub fn record_failure(&mut self, entity_name: &str, _tools_used: &[String]) {
        let resolved = self.resolve(entity_name);
        if let Some(ek) = self.entities.get_mut(&resolved) {
            ek.confidence = (ek.confidence * 0.8).max(0.1);
            self.dirty_entities.insert(resolved);
        }
    }

    /// Register an alias for an entity.
    pub fn add_alias(&mut self, alias: &str, canonical: &str) {
        let alias_lower = alias.to_lowercase();
        let canonical_lower = canonical.to_lowercase();
        self.alias_index.insert(alias_lower, canonical_lower);
    }

    /// Look up domain hint for an entity.
    pub fn domain_for(&self, entity: &str) -> Option<DomainHint> {
        let key = self.resolve(entity);
        self.entities.get(&key).and_then(|e| e.domain)
    }

    /// Get boost terms for routing based on entity knowledge.
    ///
    /// Returns domain keywords + associated tool names.
    pub fn boost_for(&self, entity: &str) -> Vec<String> {
        let key = self.resolve(entity);
        let entry = match self.entities.get(&key) {
            Some(e) => e,
            None => return Vec::new(),
        };

        let mut terms = Vec::new();

        // Domain keywords
        if let Some(domain) = &entry.domain {
            match domain {
                DomainHint::GitHub => {
                    terms.extend(
                        ["github", "repository", "pr", "issue"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                }
                DomainHint::Git => {
                    terms.extend(
                        ["git", "commit", "branch", "diff"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                }
                DomainHint::Code => {
                    terms.extend(["code", "file", "source"].iter().map(|s| s.to_string()));
                }
                DomainHint::Memory => {
                    terms.extend(
                        ["memory", "store", "retrieve"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                }
                DomainHint::Web => {
                    terms.extend(["web", "http", "url"].iter().map(|s| s.to_string()));
                }
                DomainHint::System => {
                    terms.extend(["system", "process", "file"].iter().map(|s| s.to_string()));
                }
                DomainHint::Database => {
                    terms.extend(
                        ["database", "sql", "query", "table", "matrixone"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                }
            }
        }

        // Associated tool names as terms
        terms.extend(entry.associated_tools.iter().cloned());

        terms
    }

    /// Get the full knowledge for an entity (if any).
    pub fn get(&self, entity: &str) -> Option<&EntityKnowledge> {
        let key = self.resolve(entity);
        self.entities.get(&key)
    }

    /// Get confidence for an entity (0.0 if unknown).
    ///
    /// Returns time-decayed confidence: entities not observed recently
    /// have lower effective confidence to reflect uncertainty.
    pub fn confidence_for(&self, entity: &str) -> f64 {
        let key = self.resolve(entity);
        self.entities
            .get(&key)
            .map(|e| e.decayed_confidence())
            .unwrap_or(0.0)
    }

    /// Merge knowledge from persistent storage.
    ///
    /// For each incoming entity, if we have higher observation count,
    /// keep ours; otherwise, take the stored version.
    pub fn merge(&mut self, entries: &[EntityKnowledge]) {
        for entry in entries {
            let key = entry.name.to_lowercase();
            let existing = self.entities.get(&key);
            if existing.is_none_or(|e| e.observation_count < entry.observation_count) {
                self.entities.insert(key.clone(), entry.clone());
            }
            // Also index aliases
            for alias in &entry.aliases {
                self.alias_index.insert(alias.to_lowercase(), key.clone());
            }
        }
    }

    /// Export all entity knowledge for persistence.
    pub fn export(&self) -> Vec<EntityKnowledge> {
        self.entities.values().cloned().collect()
    }

    /// Export only entities modified since last sync.
    /// Call `clear_dirty()` after successful sync to reset tracking.
    pub fn export_dirty(&self) -> Vec<EntityKnowledge> {
        self.dirty_entities
            .iter()
            .filter_map(|name| self.entities.get(name).cloned())
            .collect()
    }

    /// Check if there are dirty entities needing sync.
    pub fn has_dirty(&self) -> bool {
        !self.dirty_entities.is_empty()
    }

    /// Clear dirty tracking after successful sync.
    pub fn clear_dirty(&mut self) {
        self.dirty_entities.clear();
        self.last_sync_epoch = current_entity_timestamp();
    }

    /// Get the timestamp of last successful sync.
    pub fn last_sync_epoch(&self) -> u64 {
        self.last_sync_epoch
    }

    /// Number of known entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Report health metrics for the entity graph.
    pub fn health_report(&self) -> EntityGraphHealth {
        let total = self.entities.len();
        let low_confidence = self
            .entities
            .values()
            .filter(|e| e.decayed_confidence() < 0.3)
            .count();
        let stale = self
            .entities
            .values()
            .filter(|e| e.time_decay_factor() < 0.3)
            .count();
        EntityGraphHealth {
            total_entities: total,
            low_confidence,
            stale_entities: stale,
        }
    }

    /// Remove entities whose decayed confidence falls below `min_confidence`.
    ///
    /// Returns the number of entities pruned. Also cleans alias_index
    /// and dirty_entities for removed entries.
    pub fn prune(&mut self, min_confidence: f64) -> usize {
        let stale_keys: Vec<String> = self
            .entities
            .iter()
            .filter(|(_, e)| e.decayed_confidence() < min_confidence)
            .map(|(k, _)| k.clone())
            .collect();

        let count = stale_keys.len();
        for key in &stale_keys {
            if let Some(entity) = self.entities.remove(key) {
                self.dirty_entities.remove(key);
                // Clean alias_index for all aliases pointing to this entity
                for alias in &entity.aliases {
                    let lower = alias.to_lowercase();
                    if self.alias_index.get(&lower) == Some(key) {
                        self.alias_index.remove(&lower);
                    }
                }
                // Also remove the canonical name from alias_index if present
                self.alias_index.remove(key);
            }
        }

        count
    }

    /// Resolve an entity name through the alias index.
    fn resolve(&self, entity: &str) -> String {
        let lower = entity.to_lowercase();
        self.alias_index.get(&lower).cloned().unwrap_or(lower)
    }
}

/// Health metrics for the entity graph.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityGraphHealth {
    pub total_entities: usize,
    pub low_confidence: usize,
    pub stale_entities: usize,
}

// ─── Entity Extraction ───────────────────────────────────────────────────────

/// Common stop words to filter out during entity extraction.
const STOP_WORDS: &[&str] = &[
    // English
    "the",
    "a",
    "an",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "do",
    "does",
    "did",
    "will",
    "would",
    "could",
    "should",
    "may",
    "might",
    "can",
    "shall",
    "must",
    "need",
    "i",
    "me",
    "my",
    "you",
    "your",
    "he",
    "she",
    "it",
    "we",
    "they",
    "this",
    "that",
    "these",
    "those",
    "what",
    "which",
    "who",
    "whom",
    "how",
    "when",
    "where",
    "why",
    "if",
    "then",
    "else",
    "so",
    "but",
    "and",
    "or",
    "not",
    "no",
    "yes",
    "all",
    "any",
    "some",
    "every",
    "in",
    "on",
    "at",
    "to",
    "for",
    "of",
    "with",
    "from",
    "by",
    "about",
    "up",
    "out",
    "off",
    "over",
    "under",
    "into",
    "through",
    "show",
    "list",
    "get",
    "set",
    "run",
    "make",
    "let",
    "put",
    "help",
    "want",
    "please",
    "just",
    "now",
    "here",
    "there",
    // Chinese functional words
    "的",
    "了",
    "吗",
    "吧",
    "啊",
    "呢",
    "把",
    "被",
    "给",
    "从",
    "在",
    "和",
    "与",
    "或",
    "也",
    "都",
    "还",
    "就",
    "才",
    "又",
    "我",
    "你",
    "他",
    "她",
    "它",
    "们",
    "这",
    "那",
    "哪",
    "什么",
    "怎么",
    "如何",
    "为什么",
    "是",
    "有",
    "没有",
    "不",
    "要",
    "可以",
    "能",
    "会",
    "想",
    "看看",
    "帮",
    "帮我",
];

/// Extract entity candidates from a query.
///
/// Uses heuristics:
/// - Tokens longer than 2 chars that aren't stop words
/// - CJK sequences (treated as potential proper nouns)
/// - Mixed-case tokens (e.g., "MatrixOrigin")
///
/// Intentionally permissive — better to extract too many candidates
/// and filter via the EntityGraph than to miss entities.
pub fn extract_entities(query: &str) -> Vec<String> {
    let mut entities = Vec::new();

    // Tokenize: split on whitespace and punctuation, keeping CJK as individual sequences
    for token in tokenize_for_entities(query) {
        let lower = token.to_lowercase();

        // Skip stop words
        if STOP_WORDS.contains(&lower.as_str()) {
            continue;
        }

        // Skip very short tokens (< 2 chars for ASCII, < 1 for CJK)
        let is_cjk = token.chars().any(is_cjk_char);
        if !is_cjk && token.len() < 3 {
            continue;
        }
        if is_cjk && token.chars().count() < 2 {
            continue;
        }

        // Skip pure numbers
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        entities.push(lower);
    }

    // Dedup while preserving order
    let mut seen = std::collections::HashSet::new();
    entities.retain(|e| seen.insert(e.clone()));

    entities
}

/// Simple tokenizer for entity extraction.
fn tokenize_for_entities(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_cjk = false;

    for ch in text.chars() {
        let cjk = is_cjk_char(ch);

        if cjk {
            // Flush ASCII token
            if !current.is_empty() && !in_cjk {
                tokens.push(std::mem::take(&mut current));
            }
            current.push(ch);
            in_cjk = true;
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            // Flush CJK token
            if !current.is_empty() && in_cjk {
                tokens.push(std::mem::take(&mut current));
            }
            current.push(ch);
            in_cjk = false;
        } else {
            // Separator — flush whatever we have
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
                in_cjk = false;
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_cjk_char(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}')
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EntityGraph basics ───────────────────────────────────────────────

    #[test]
    fn learn_and_query() {
        let mut graph = EntityGraph::new();
        assert!(graph.is_empty());

        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into()],
            None,
        );
        assert_eq!(graph.len(), 1);
        assert_eq!(graph.domain_for("matrixorigin"), Some(DomainHint::GitHub));
        assert_eq!(graph.domain_for("MatrixOrigin"), Some(DomainHint::GitHub)); // case-insensitive
    }

    #[test]
    fn unknown_entity_returns_none() {
        let graph = EntityGraph::new();
        assert_eq!(graph.domain_for("unknown"), None);
        assert_eq!(graph.confidence_for("unknown"), 0.0);
    }

    #[test]
    fn confidence_increases_with_observations() {
        let mut graph = EntityGraph::new();

        graph.learn("mo", DomainHint::GitHub, &["gh_search".into()], None);
        let conf1 = graph.confidence_for("mo");

        graph.learn("mo", DomainHint::GitHub, &["gh_list_prs".into()], None);
        let conf2 = graph.confidence_for("mo");

        graph.learn("mo", DomainHint::GitHub, &["gh_issues".into()], None);
        let conf3 = graph.confidence_for("mo");

        assert!(
            conf2 > conf1,
            "Confidence should grow: {} > {}",
            conf2,
            conf1
        );
        assert!(
            conf3 > conf2,
            "Confidence should grow: {} > {}",
            conf3,
            conf2
        );
        assert!(conf3 <= 1.0, "Confidence should be capped: {}", conf3);
    }

    #[test]
    fn tools_deduplicated() {
        let mut graph = EntityGraph::new();
        graph.learn("mo", DomainHint::GitHub, &["gh_search".into()], None);
        graph.learn(
            "mo",
            DomainHint::GitHub,
            &["gh_search".into(), "gh_prs".into()],
            None,
        );

        let entry = graph.get("mo").unwrap();
        assert_eq!(entry.associated_tools.len(), 2); // gh_search + gh_prs (deduplicated)
    }

    // ── Aliases ──────────────────────────────────────────────────────────

    #[test]
    fn alias_resolves_to_canonical() {
        let mut graph = EntityGraph::new();
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["gh_search".into()],
            None,
        );
        graph.add_alias("mo", "matrixorigin");

        assert_eq!(graph.domain_for("mo"), Some(DomainHint::GitHub));
        assert_eq!(graph.domain_for("MO"), Some(DomainHint::GitHub)); // case-insensitive
    }

    // ── Boost Terms ──────────────────────────────────────────────────────

    #[test]
    fn boost_for_known_entity() {
        let mut graph = EntityGraph::new();
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search_repos".into()],
            None,
        );

        let terms = graph.boost_for("matrixorigin");
        assert!(!terms.is_empty());
        assert!(terms.contains(&"github".to_string()));
        assert!(terms.contains(&"github_search_repos".to_string()));
    }

    #[test]
    fn boost_for_unknown_entity_empty() {
        let graph = EntityGraph::new();
        assert!(graph.boost_for("unknown").is_empty());
    }

    // ── Merge ────────────────────────────────────────────────────────────

    #[test]
    fn merge_takes_higher_observation_count() {
        let mut graph = EntityGraph::new();
        graph.learn("mo", DomainHint::GitHub, &["gh_search".into()], None);

        // External entry with higher observation count
        let external = EntityKnowledge {
            name: "mo".into(),
            aliases: vec!["matrixorigin".into()],
            domain: Some(DomainHint::GitHub),
            associated_tools: vec!["gh_search".into(), "gh_prs".into()],
            confidence: 0.8,
            observation_count: 10,
            last_observed_at: chrono::Utc::now().timestamp() as u64,
        };
        graph.merge(&[external]);

        let entry = graph.get("mo").unwrap();
        assert_eq!(entry.observation_count, 10);
        assert_eq!(entry.associated_tools.len(), 2);
    }

    #[test]
    fn merge_keeps_local_if_higher() {
        let mut graph = EntityGraph::new();
        // Local with 5 observations
        for _ in 0..5 {
            graph.learn("mo", DomainHint::GitHub, &["gh_search".into()], None);
        }

        // External with only 2 observations
        let external = EntityKnowledge {
            name: "mo".into(),
            aliases: vec![],
            domain: Some(DomainHint::Git),
            associated_tools: vec!["git_log".into()],
            confidence: 0.5,
            observation_count: 2,
            last_observed_at: chrono::Utc::now().timestamp() as u64,
        };
        graph.merge(&[external]);

        // Should keep local (5 > 2)
        let entry = graph.get("mo").unwrap();
        assert_eq!(entry.observation_count, 5);
        assert_eq!(entry.domain, Some(DomainHint::GitHub));
    }

    #[test]
    fn merge_indexes_aliases() {
        let mut graph = EntityGraph::new();
        let external = EntityKnowledge {
            name: "matrixorigin".into(),
            aliases: vec!["mo".into(), "matrixone".into()],
            domain: Some(DomainHint::GitHub),
            associated_tools: vec![],
            confidence: 0.5,
            observation_count: 3,
            last_observed_at: chrono::Utc::now().timestamp() as u64,
        };
        graph.merge(&[external]);

        assert_eq!(graph.domain_for("mo"), Some(DomainHint::GitHub));
        assert_eq!(graph.domain_for("matrixone"), Some(DomainHint::GitHub));
    }

    // ── Export ────────────────────────────────────────────────────────────

    #[test]
    fn export_all_entities() {
        let mut graph = EntityGraph::new();
        graph.learn("mo", DomainHint::GitHub, &[], None);
        graph.learn("linux", DomainHint::System, &[], None);

        let exported = graph.export();
        assert_eq!(exported.len(), 2);
    }

    // ── Entity Extraction ────────────────────────────────────────────────

    #[test]
    fn extract_english_entities() {
        let entities = extract_entities("show me matrixorigin PRs");
        assert!(entities.contains(&"matrixorigin".to_string()));
        // "show" and "PRs" might be filtered
    }

    #[test]
    fn extract_mixed_cn_en() {
        let entities = extract_entities("我关注matrixorigin");
        assert!(entities.contains(&"matrixorigin".to_string()));
        // "关注" is a stop word in our list
    }

    #[test]
    fn extract_filters_stop_words() {
        let entities = extract_entities("show me the latest status");
        // All common words should be filtered
        assert!(!entities.contains(&"the".to_string()));
        assert!(!entities.contains(&"me".to_string()));
        assert!(!entities.contains(&"show".to_string()));
    }

    #[test]
    fn extract_deduplicates() {
        let entities = extract_entities("matrixorigin and matrixorigin again");
        let mo_count = entities.iter().filter(|e| *e == "matrixorigin").count();
        assert_eq!(mo_count, 1);
    }

    #[test]
    fn extract_handles_empty() {
        assert!(extract_entities("").is_empty());
    }

    #[test]
    fn extract_preserves_hyphenated() {
        let entities = extract_entities("check mo-dev-agent status");
        assert!(entities.contains(&"mo-dev-agent".to_string()));
    }

    // NOTE: entity_graph_improves_routing test moved to runtime integration tests
    // (requires RoutingEngine which lives in runtime)

    #[test]
    fn learning_cycle_simulation() {
        // Simulate: query → learn → query again → improved
        let mut graph = EntityGraph::new();

        // Turn 1: unknown entity
        let entities = extract_entities("我关注matrixorigin");
        assert!(entities.contains(&"matrixorigin".to_string()));
        assert_eq!(graph.domain_for("matrixorigin"), None);

        // Agent uses GitHub tools successfully → learn
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search_repos".into(), "github_list_issues".into()],
            None,
        );

        // Turn 2: known entity → domain hint available
        assert_eq!(graph.domain_for("matrixorigin"), Some(DomainHint::GitHub));
        let boost = graph.boost_for("matrixorigin");
        assert!(boost.contains(&"github".to_string()));
        assert!(boost.contains(&"github_search_repos".to_string()));

        // Confidence should be meaningful
        assert!(graph.confidence_for("matrixorigin") > 0.3);
    }

    // ── User Feedback Integration ──

    #[test]
    fn low_feedback_dampens_confidence_growth() {
        let mut graph = EntityGraph::new();

        // First learn with high feedback
        graph.learn(
            "good_entity",
            DomainHint::GitHub,
            &["tool1".into()],
            Some(80),
        );
        let _conf_high = graph.confidence_for("good_entity");

        // Then learn with low feedback (same entity, another observation)
        graph.learn(
            "good_entity",
            DomainHint::GitHub,
            &["tool2".into()],
            Some(20),
        );
        let _conf_low_after = graph.confidence_for("good_entity");

        // Create another entity learned only with low feedback
        graph.learn("bad_entity", DomainHint::Code, &["tool1".into()], Some(20));
        graph.learn("bad_entity", DomainHint::Code, &["tool2".into()], Some(20));
        let conf_bad = graph.confidence_for("bad_entity");

        // Create entity learned only with high feedback (same observation count)
        graph.learn("great_entity", DomainHint::Git, &["tool1".into()], Some(90));
        graph.learn("great_entity", DomainHint::Git, &["tool2".into()], Some(90));
        let conf_great = graph.confidence_for("great_entity");

        assert!(
            conf_great > conf_bad,
            "High feedback should yield higher confidence: {} > {}",
            conf_great,
            conf_bad
        );
    }

    #[test]
    fn no_feedback_uses_normal_confidence() {
        let mut graph = EntityGraph::new();

        graph.learn("entity_a", DomainHint::GitHub, &["tool".into()], None);
        graph.learn("entity_a", DomainHint::GitHub, &["tool".into()], None);

        let conf = graph.confidence_for("entity_a");
        // Normal asymptotic: 1.0 - 1.0/(1+2) ≈ 0.667
        assert!(
            (conf - 0.667).abs() < 0.1,
            "Should use normal confidence formula, got {}",
            conf
        );
    }

    // ── Time Decay Tests ──

    #[test]
    fn entity_time_decay_within_grace_period() {
        // Within grace period (14 days), decayed confidence should equal raw
        let mut entity = EntityKnowledge::new("test");
        entity.confidence = 0.8;
        entity.touch(); // Set to now

        let raw = entity.confidence;
        let decayed = entity.decayed_confidence();
        assert!(
            (raw - decayed).abs() < 0.001,
            "Fresh entity should have no decay"
        );
    }

    #[test]
    fn entity_time_decay_at_half_life() {
        // At exactly one half-life (60 days) past grace period (14 days), confidence should be ~50%
        let mut entity = EntityKnowledge::new("stale");
        entity.confidence = 0.8;
        let now = chrono::Utc::now().timestamp() as u64;
        entity.last_observed_at = now - (74 * 24 * 3600); // 14 grace + 60 half-life

        let raw = entity.confidence;
        let decayed = entity.decayed_confidence();
        let ratio = decayed / raw;
        assert!(
            (ratio - 0.5).abs() < 0.1,
            "At half-life, confidence ratio should be ~0.5, got {}",
            ratio
        );
    }

    #[test]
    fn decayed_confidence_recent_entity() {
        let mut entity = EntityKnowledge::new("test");
        entity.confidence = 0.8;
        entity.touch(); // Set to now

        let raw_conf = entity.confidence;
        let decayed = entity.decayed_confidence();
        assert!(
            (raw_conf - decayed).abs() < 0.001,
            "Recent entity should have same raw and decayed confidence"
        );
    }

    #[test]
    fn decayed_confidence_stale_entity() {
        let mut entity = EntityKnowledge::new("stale");
        entity.confidence = 0.8;
        // Set to 74 days ago (one half-life past grace)
        let now = chrono::Utc::now().timestamp() as u64;
        entity.last_observed_at = now - (74 * 24 * 3600);

        let raw_conf = entity.confidence;
        let decayed = entity.decayed_confidence();
        let expected_decayed = raw_conf * 0.5; // Approximately

        assert!(
            decayed < raw_conf,
            "Stale entity decayed confidence should be less than raw"
        );
        assert!(
            (decayed - expected_decayed).abs() < 0.1,
            "Expected ~{}, got {}",
            expected_decayed,
            decayed
        );
    }

    #[test]
    fn confidence_for_uses_decay() {
        let mut graph = EntityGraph::new();
        graph.learn("fresh_entity", DomainHint::GitHub, &["tool".into()], None);
        graph.learn("fresh_entity", DomainHint::GitHub, &["tool".into()], None);

        // Make the entity stale
        if let Some(entity) = graph.entities.get_mut("fresh_entity") {
            let now = chrono::Utc::now().timestamp() as u64;
            entity.last_observed_at = now - (100 * 24 * 3600); // 100 days ago
        }

        let conf = graph.confidence_for("fresh_entity");
        // Should be significantly less than normal due to decay
        assert!(
            conf < 0.5,
            "Stale entity confidence should be reduced by decay, got {}",
            conf
        );
    }

    #[test]
    fn entity_touch_updates_timestamp() {
        let mut entity = EntityKnowledge::new("test");
        let old_ts = entity.last_observed_at;
        std::thread::sleep(std::time::Duration::from_millis(1100)); // Wait >1 second
        entity.touch();
        assert!(
            entity.last_observed_at > old_ts,
            "touch() should update timestamp"
        );
    }
}
