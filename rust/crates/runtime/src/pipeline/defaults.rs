//! Built-in default knowledge for cold-start bootstrap.
//!
//! Provides reasonable starting patterns, entity mappings, and calibration
//! settings so that new users don't start with completely empty learning modules.

use super::calibration::CalibrationExport;
use super::entity::EntityKnowledge;
use super::pattern::ToolChainPattern;
use super::routing::{DomainHint, TaskType};

/// Helper to create a pattern with pre-computed quality_sum from average quality.
fn pattern(
    signature: &str,
    tools: Vec<&str>,
    task_type: TaskType,
    domain: Option<DomainHint>,
    success_count: u32,
    failure_count: u32,
    avg_quality: f64,
) -> ToolChainPattern {
    // quality_sum = avg_quality * success_count
    // We use serde_json roundtrip to set the private quality_sum field
    let quality_sum = avg_quality * success_count as f64;
    let json = serde_json::json!({
        "signature": signature,
        "tools": tools,
        "task_type": task_type,
        "domain": domain,
        "success_count": success_count,
        "failure_count": failure_count,
        "quality_sum": quality_sum
    });
    serde_json::from_value(json).expect("valid pattern JSON")
}

/// Create default tool chain patterns for common scenarios.
///
/// These are "reasonable defaults" based on typical usage patterns.
/// The system will learn and override these as it observes real user behavior.
pub fn default_patterns() -> Vec<ToolChainPattern> {
    vec![
        // GitHub domain patterns
        pattern(
            "github_search",
            vec!["github_search"],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            20,
            1,
            0.87,
        ),
        pattern(
            "github_list_prs",
            vec!["github_list_prs"],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            15,
            1,
            0.90,
        ),
        pattern(
            "github_list_issues",
            vec!["github_list_issues"],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            15,
            1,
            0.88,
        ),
        // Code domain patterns
        pattern(
            "file_read",
            vec!["file_read"],
            TaskType::Code,
            Some(DomainHint::Code),
            30,
            2,
            0.93,
        ),
        pattern(
            "grep",
            vec!["grep"],
            TaskType::Code,
            Some(DomainHint::Code),
            25,
            3,
            0.88,
        ),
        pattern(
            "file_read|str_replace",
            vec!["file_read", "str_replace"],
            TaskType::Code,
            Some(DomainHint::Code),
            20,
            2,
            0.85,
        ),
        pattern(
            "bash",
            vec!["bash"],
            TaskType::Code,
            Some(DomainHint::Code),
            25,
            5,
            0.81,
        ),
        // Git domain patterns
        pattern(
            "git_status",
            vec!["git_status"],
            TaskType::Fetch,
            Some(DomainHint::Git),
            20,
            1,
            0.93,
        ),
        pattern(
            "git_diff",
            vec!["git_diff"],
            TaskType::Fetch,
            Some(DomainHint::Git),
            18,
            1,
            0.89,
        ),
        pattern(
            "git_log",
            vec!["git_log"],
            TaskType::Fetch,
            Some(DomainHint::Git),
            15,
            1,
            0.89,
        ),
    ]
}

/// Create default entity knowledge for well-known terms.
///
/// This helps the system recognize common entities without prior learning.
pub fn default_entities() -> Vec<EntityKnowledge> {
    vec![
        // GitHub-related entities
        EntityKnowledge {
            name: "github".into(),
            domain: Some(DomainHint::GitHub),
            associated_tools: vec![
                "github_search".into(),
                "github_list_prs".into(),
                "github_list_issues".into(),
            ],
            confidence: 0.9,
            observation_count: 50,
            aliases: vec!["gh".into()],
        },
        EntityKnowledge {
            name: "pr".into(),
            domain: Some(DomainHint::GitHub),
            associated_tools: vec!["github_list_prs".into(), "github_search".into()],
            confidence: 0.85,
            observation_count: 30,
            aliases: vec!["pull request".into(), "pull-request".into()],
        },
        EntityKnowledge {
            name: "issue".into(),
            domain: Some(DomainHint::GitHub),
            associated_tools: vec!["github_list_issues".into(), "github_search".into()],
            confidence: 0.85,
            observation_count: 25,
            aliases: vec!["bug".into(), "ticket".into()],
        },
        // Git-related entities
        EntityKnowledge {
            name: "git".into(),
            domain: Some(DomainHint::Git),
            associated_tools: vec![
                "git_status".into(),
                "git_diff".into(),
                "git_log".into(),
                "git_commit".into(),
            ],
            confidence: 0.9,
            observation_count: 40,
            aliases: vec![],
        },
        EntityKnowledge {
            name: "commit".into(),
            domain: Some(DomainHint::Git),
            associated_tools: vec!["git_log".into(), "git_show".into()],
            confidence: 0.85,
            observation_count: 25,
            aliases: vec![],
        },
        EntityKnowledge {
            name: "diff".into(),
            domain: Some(DomainHint::Git),
            associated_tools: vec!["git_diff".into()],
            confidence: 0.8,
            observation_count: 20,
            aliases: vec!["changes".into()],
        },
        // Code-related entities
        EntityKnowledge {
            name: "file".into(),
            domain: Some(DomainHint::Code),
            associated_tools: vec!["file_read".into(), "file_write".into(), "grep".into()],
            confidence: 0.85,
            observation_count: 35,
            aliases: vec![],
        },
        EntityKnowledge {
            name: "code".into(),
            domain: Some(DomainHint::Code),
            associated_tools: vec!["file_read".into(), "grep".into(), "str_replace".into()],
            confidence: 0.8,
            observation_count: 30,
            aliases: vec!["source".into()],
        },
    ]
}

/// Create default calibration settings.
///
/// Returns calibration export with reasonable initial corrections to
/// prevent over-aggressive auto-routing on cold start.
pub fn default_calibration() -> CalibrationExport {
    use std::collections::HashMap;

    CalibrationExport {
        base_threshold: 0.70,
        per_intent: HashMap::new(), // Start with no intent-specific adjustments
        per_domain: HashMap::new(), // Start with no domain-specific adjustments
        per_task: HashMap::new(),   // Start with no task-type adjustments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_patterns_not_empty() {
        let patterns = default_patterns();
        assert!(!patterns.is_empty(), "should have default patterns");
        assert!(patterns.len() >= 5, "should have at least 5 patterns");

        // Verify patterns have reasonable data
        for p in &patterns {
            assert!(!p.signature.is_empty(), "signature should not be empty");
            assert!(!p.tools.is_empty(), "tools should not be empty");
            assert!(p.success_count > 0, "should have success count");
        }
    }

    #[test]
    fn default_patterns_have_quality() {
        let patterns = default_patterns();
        for p in &patterns {
            let q = p.avg_quality();
            assert!(q > 0.0, "pattern {} should have quality > 0", p.signature);
            assert!(q <= 1.0, "pattern {} quality should be <= 1", p.signature);
        }
    }

    #[test]
    fn default_entities_not_empty() {
        let entities = default_entities();
        assert!(!entities.is_empty(), "should have default entities");
        assert!(entities.len() >= 5, "should have at least 5 entities");

        // Verify entities have reasonable data
        for e in &entities {
            assert!(!e.name.is_empty(), "name should not be empty");
            assert!(e.domain.is_some(), "domain should be set");
            assert!(!e.associated_tools.is_empty(), "should have associated tools");
            assert!(e.confidence > 0.0, "confidence should be positive");
        }
    }

    #[test]
    fn default_calibration_reasonable_threshold() {
        let cal = default_calibration();
        assert!(
            (0.5..=0.9).contains(&cal.base_threshold),
            "threshold {} should be reasonable",
            cal.base_threshold
        );
    }

    #[test]
    fn patterns_cover_main_domains() {
        let patterns = default_patterns();
        let domains: Vec<_> = patterns.iter().filter_map(|p| p.domain.as_ref()).collect();

        assert!(
            domains.iter().any(|d| matches!(d, DomainHint::GitHub)),
            "should have GitHub patterns"
        );
        assert!(
            domains.iter().any(|d| matches!(d, DomainHint::Code)),
            "should have Code patterns"
        );
        assert!(
            domains.iter().any(|d| matches!(d, DomainHint::Git)),
            "should have Git patterns"
        );
    }

    #[test]
    fn entities_cover_main_domains() {
        let entities = default_entities();
        let domains: Vec<_> = entities.iter().filter_map(|e| e.domain.as_ref()).collect();

        assert!(
            domains.iter().any(|d| matches!(d, DomainHint::GitHub)),
            "should have GitHub entities"
        );
        assert!(
            domains.iter().any(|d| matches!(d, DomainHint::Code)),
            "should have Code entities"
        );
        assert!(
            domains.iter().any(|d| matches!(d, DomainHint::Git)),
            "should have Git entities"
        );
    }
}
