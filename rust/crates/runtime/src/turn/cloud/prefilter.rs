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

    candidates.sort_by(|left, right| right.1.cmp(&left.1));
    CloudSkillCandidatePlan {
        selected_schemas: candidates
            .into_iter()
            .take(max_candidates)
            .map(|(schema, _score)| schema)
            .collect(),
        cloud_skill_names,
    }
}
