use std::collections::BTreeSet;

use regex::Regex;
use serde_json::Value;

pub struct CloudSkillCandidatePlan {
    pub selected_schemas: Vec<Value>,
    pub cloud_skill_names: BTreeSet<String>,
}

pub fn plan_cloud_skill_candidates(
    cloud_schemas: &[Value],
    edge_tool_names: &BTreeSet<String>,
    user_query: &str,
    max_candidates: usize,
) -> CloudSkillCandidatePlan {
    let query_lower = user_query.to_lowercase();
    let query_tokens = if query_lower.is_empty() {
        BTreeSet::new()
    } else {
        query_lower
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>()
    };
    let query_alpha = Regex::new(r"[a-z]{3,}")
        .expect("query alpha regex should compile")
        .find_iter(&query_lower)
        .map(|m| m.as_str().to_string())
        .collect::<BTreeSet<_>>();

    let mut candidates = Vec::new();
    let mut cloud_skill_names = BTreeSet::new();
    for schema in cloud_schemas {
        let schema_name = schema
            .get("function")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if schema_name.is_empty() || edge_tool_names.contains(schema_name) {
            continue;
        }
        cloud_skill_names.insert(schema_name.to_string());

        let description = schema
            .get("function")
            .and_then(|value| value.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = format!("{} {}", schema_name, description).to_lowercase();
        let mut score = if query_tokens.is_empty() {
            0
        } else {
            query_tokens
                .iter()
                .filter(|token| text.contains(token.as_str()))
                .count() as i32
        };
        let name_parts = schema_name
            .to_lowercase()
            .replace('_', " ")
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        for query_word in &query_alpha {
            for name_part in &name_parts {
                if query_word.contains(name_part) || name_part.contains(query_word) {
                    score += 2;
                }
            }
        }
        candidates.push((schema.clone(), score));
    }

    candidates.sort_by_key(|right| std::cmp::Reverse(right.1));
    CloudSkillCandidatePlan {
        selected_schemas: candidates
            .into_iter()
            .take(max_candidates)
            .map(|(schema, _score)| schema)
            .collect(),
        cloud_skill_names,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(name: &str, desc: &str) -> Value {
        json!({"function": {"name": name, "description": desc}})
    }

    #[test]
    fn empty_schemas() {
        let plan = plan_cloud_skill_candidates(&[], &BTreeSet::new(), "hello", 10);
        assert!(plan.selected_schemas.is_empty());
        assert!(plan.cloud_skill_names.is_empty());
    }

    #[test]
    fn empty_query_returns_all() {
        let schemas = vec![schema("foo", "bar"), schema("baz", "qux")];
        let plan = plan_cloud_skill_candidates(&schemas, &BTreeSet::new(), "", 10);
        assert_eq!(plan.selected_schemas.len(), 2);
        assert_eq!(plan.cloud_skill_names.len(), 2);
    }

    #[test]
    fn edge_tool_names_excluded() {
        let schemas = vec![schema("bash", "run commands"), schema("web", "search")];
        let edge = BTreeSet::from(["bash".to_string()]);
        let plan = plan_cloud_skill_candidates(&schemas, &edge, "run", 10);
        assert_eq!(plan.selected_schemas.len(), 1);
        // bash is NOT in cloud_skill_names because it's an edge tool
        assert!(!plan.cloud_skill_names.contains("bash"));
        assert!(plan.cloud_skill_names.contains("web"));
    }

    #[test]
    fn max_candidates_limits_output() {
        let schemas = vec![schema("a", "x"), schema("b", "y"), schema("c", "z")];
        let plan = plan_cloud_skill_candidates(&schemas, &BTreeSet::new(), "x y z", 2);
        assert_eq!(plan.selected_schemas.len(), 2);
        // all three still tracked as cloud skill names
        assert_eq!(plan.cloud_skill_names.len(), 3);
    }

    #[test]
    fn token_match_scoring() {
        let schemas = vec![
            schema("file_search", "search files on disk"),
            schema("web_fetch", "fetch web pages"),
        ];
        let plan = plan_cloud_skill_candidates(&schemas, &BTreeSet::new(), "search files", 10);
        // file_search should score higher (both tokens match)
        assert_eq!(
            plan.selected_schemas[0]["function"]["name"]
                .as_str()
                .unwrap(),
            "file_search"
        );
    }

    #[test]
    fn substring_overlap_boosts_score() {
        let schemas = vec![
            schema("memory_store", "save data"),
            schema("disk_write", "write to disk"),
        ];
        // "memory" matches name part "memory" via substring overlap → +2 boost
        let plan = plan_cloud_skill_candidates(&schemas, &BTreeSet::new(), "memory", 10);
        assert_eq!(
            plan.selected_schemas[0]["function"]["name"]
                .as_str()
                .unwrap(),
            "memory_store"
        );
    }

    #[test]
    fn schema_without_function_name_skipped() {
        let schemas = vec![json!({"function": {}}), schema("good", "desc")];
        let plan = plan_cloud_skill_candidates(&schemas, &BTreeSet::new(), "good", 10);
        assert_eq!(plan.selected_schemas.len(), 1);
        assert_eq!(plan.cloud_skill_names.len(), 1);
    }

    #[test]
    fn zero_max_candidates() {
        let schemas = vec![schema("a", "b")];
        let plan = plan_cloud_skill_candidates(&schemas, &BTreeSet::new(), "a", 0);
        assert!(plan.selected_schemas.is_empty());
        assert_eq!(plan.cloud_skill_names.len(), 1);
    }
}
