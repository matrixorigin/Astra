//! LLM-based skill auto-route judging.
//!
//! This module owns the only natural-language decision allowed to pre-load a
//! skill before the main model turn. Runtime code supplies the pure user query
//! and visible skill catalog; the judge returns either one canonical skill name
//! or `None`. There is no keyword/alias fallback.

use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillAutoRouteCandidate {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillAutoRouteJudgeContext {
    pub query: String,
    pub visible_skills: Vec<SkillAutoRouteCandidate>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillAutoRouteJudgeError {
    #[error("Prompt encoding failure: {0}")]
    PromptEncoding(String),
    #[error("LLM transport failure: {0}")]
    Transport(String),
    #[error("LLM returned malformed response: {raw}")]
    Malformed { raw: String },
    #[error("LLM rejected: {0}")]
    Rejected(String),
}

#[async_trait]
pub trait SkillAutoRouteJudge: Send + Sync {
    async fn judge(
        &self,
        ctx: &SkillAutoRouteJudgeContext,
    ) -> Result<Option<String>, SkillAutoRouteJudgeError>;
}

pub fn build_skill_auto_route_prompt(
    ctx: &SkillAutoRouteJudgeContext,
) -> Result<String, SkillAutoRouteJudgeError> {
    let catalog = ctx
        .visible_skills
        .iter()
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "when_to_use": skill.when_to_use,
                "aliases": skill.aliases,
            })
        })
        .collect::<Vec<_>>();
    let catalog = serde_json::to_string(&catalog).map_err(|error| {
        SkillAutoRouteJudgeError::PromptEncoding(format!(
            "catalog JSON serialization failed: {error}"
        ))
    })?;
    let query = serde_json::to_string(&ctx.query).map_err(|error| {
        SkillAutoRouteJudgeError::PromptEncoding(format!(
            "query JSON serialization failed: {error}"
        ))
    })?;

    Ok(format!(
        "You are a skill pre-routing judge for an agentic coding assistant.\n\
         Decide whether the user's current request should load exactly one of the visible skills before the main assistant turn.\n\
         \n\
         Output ONLY a JSON object with this shape:\n\
         {{\"skill_name\": <canonical skill name | null>}}\n\
         \n\
         Rules:\n\
         - Choose a skill only when the request clearly asks for that workflow and exactly one visible skill is appropriate.\n\
         - Use only a canonical name from the visible catalog. Never invent or paraphrase names.\n\
         - Return null when the request is broad, ambiguous, asks for multiple workflows, or the normal assistant turn should decide.\n\
         - Return null when no visible skill is a clear fit.\n\
         - Return ONLY JSON. No prose, no markdown fences.\n\
         \n\
         Visible skills JSON:\n{catalog}\n\
         \n\
         User query JSON:\n{query}\n",
    ))
}

pub fn skill_auto_route_judge_messages(
    ctx: &SkillAutoRouteJudgeContext,
) -> Result<Vec<Value>, SkillAutoRouteJudgeError> {
    Ok(vec![
        json!({
            "role": "system",
            "content": "You output ONLY a JSON object as described in the user message. No prose. No markdown fences."
        }),
        json!({
            "role": "user",
            "content": build_skill_auto_route_prompt(ctx)?,
        }),
    ])
}

pub fn parse_skill_auto_route_response(
    raw: &str,
    allowed_skill_names: &[String],
) -> Result<Option<String>, SkillAutoRouteJudgeError> {
    let trimmed = raw.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(trimmed)
        .trim();

    let value = match serde_json::from_str::<Value>(unfenced) {
        Ok(value) => value,
        Err(first_error) => {
            if let (Some(start), Some(end)) = (unfenced.find('{'), unfenced.rfind('}'))
                && start < end
            {
                serde_json::from_str::<Value>(&unfenced[start..=end]).map_err(|error| {
                    SkillAutoRouteJudgeError::Malformed {
                        raw: format!(
                            "invalid wrapped JSON: {error}; initial parse: {first_error}; raw: {}",
                            truncate(raw, 256)
                        ),
                    }
                })?
            } else {
                return Err(SkillAutoRouteJudgeError::Malformed {
                    raw: format!("invalid JSON: {first_error}; raw: {}", truncate(raw, 256)),
                });
            }
        }
    };

    let Some(skill_name) = value.get("skill_name") else {
        return Err(SkillAutoRouteJudgeError::Malformed {
            raw: "missing skill_name".to_string(),
        });
    };
    if skill_name.is_null() {
        return Ok(None);
    }

    let skill_name = skill_name
        .as_str()
        .ok_or_else(|| SkillAutoRouteJudgeError::Malformed {
            raw: format!("skill_name not a string or null: {skill_name}"),
        })?
        .trim();
    if skill_name.is_empty() {
        return Ok(None);
    }
    if allowed_skill_names
        .iter()
        .any(|allowed| allowed == skill_name)
    {
        Ok(Some(skill_name.to_string()))
    } else {
        Err(SkillAutoRouteJudgeError::Malformed {
            raw: format!("skill_name outside visible catalog: {skill_name}"),
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str) -> SkillAutoRouteCandidate {
        SkillAutoRouteCandidate {
            name: name.to_string(),
            description: format!("Workflow for {name}"),
            when_to_use: Some(format!("Use {name} when appropriate")),
            aliases: vec![format!("{name} alias")],
        }
    }

    #[test]
    fn prompt_contains_structured_catalog_and_no_execution_contract() {
        let ctx = SkillAutoRouteJudgeContext {
            query: "review the current branch".into(),
            visible_skills: vec![candidate("review-changes")],
        };

        let prompt = build_skill_auto_route_prompt(&ctx).unwrap();

        assert!(prompt.contains("\"name\":\"review-changes\""));
        assert!(prompt.contains("\"skill_name\": <canonical skill name | null>"));
        assert!(prompt.contains("Return null when the request is broad, ambiguous"));
        assert!(prompt.contains("User query JSON:\n\"review the current branch\""));
    }

    #[test]
    fn parser_accepts_visible_skill_and_null() {
        let allowed = vec!["review-changes".to_string()];

        assert_eq!(
            parse_skill_auto_route_response("{\"skill_name\":\"review-changes\"}", &allowed)
                .unwrap(),
            Some("review-changes".to_string())
        );
        assert_eq!(
            parse_skill_auto_route_response("{\"skill_name\":null}", &allowed).unwrap(),
            None
        );
        assert_eq!(
            parse_skill_auto_route_response("{\"skill_name\":\"\"}", &allowed).unwrap(),
            None
        );
    }

    #[test]
    fn parser_rejects_unknown_or_malformed_skill_names() {
        let allowed = vec!["review-changes".to_string()];

        assert!(matches!(
            parse_skill_auto_route_response("{\"skill_name\":\"other\"}", &allowed),
            Err(SkillAutoRouteJudgeError::Malformed { .. })
        ));
        assert!(matches!(
            parse_skill_auto_route_response("{\"skill_name\":42}", &allowed),
            Err(SkillAutoRouteJudgeError::Malformed { .. })
        ));
        assert!(matches!(
            parse_skill_auto_route_response("{\"other\":null}", &allowed),
            Err(SkillAutoRouteJudgeError::Malformed { .. })
        ));
    }

    #[test]
    fn parser_extracts_fenced_or_wrapped_json() {
        let allowed = vec!["review-changes".to_string()];

        assert_eq!(
            parse_skill_auto_route_response(
                "```json\n{\"skill_name\":\"review-changes\"}\n```",
                &allowed,
            )
            .unwrap(),
            Some("review-changes".to_string())
        );
        assert_eq!(
            parse_skill_auto_route_response(
                "decision: {\"skill_name\":\"review-changes\"}",
                &allowed,
            )
            .unwrap(),
            Some("review-changes".to_string())
        );
    }

    #[test]
    fn parser_reports_invalid_json_without_unwrap_control_flow() {
        let allowed = vec!["review-changes".to_string()];

        let result = parse_skill_auto_route_response("not json", &allowed);

        match result {
            Err(error @ SkillAutoRouteJudgeError::Malformed { .. }) => {
                assert!(error.to_string().contains("malformed response"));
            }
            other => panic!("expected malformed error, got {other:?}"),
        }
    }
}
