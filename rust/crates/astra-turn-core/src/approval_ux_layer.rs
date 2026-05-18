//! Product-facing permission approval prompts.

use crate::permission_engine::{DecisionEnvelope, DecisionSource, HardDecision, RiskTag};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimplifiedApprovalChoice {
    AllowOnce,
    AlwaysAllowSimilarInWorkspace,
    Reject,
    More,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRiskLevel {
    Normal,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimplifiedApprovalPrompt {
    pub primary_question: String,
    pub affected_resource: Option<String>,
    pub choices: Vec<SimplifiedApprovalChoice>,
    pub risk_level: ApprovalRiskLevel,
}

#[must_use]
pub fn simplify_approval_prompt(envelope: &DecisionEnvelope) -> Option<SimplifiedApprovalPrompt> {
    let HardDecision::NeedExternal { prompt } = &envelope.decision else {
        return None;
    };
    let risk_level = if is_high_risk(envelope) {
        ApprovalRiskLevel::High
    } else {
        ApprovalRiskLevel::Normal
    };
    let mut choices = vec![
        SimplifiedApprovalChoice::AllowOnce,
        SimplifiedApprovalChoice::Reject,
    ];
    if risk_level == ApprovalRiskLevel::Normal && envelope.will_save.is_some() {
        choices.insert(1, SimplifiedApprovalChoice::AlwaysAllowSimilarInWorkspace);
    }
    choices.push(SimplifiedApprovalChoice::More);
    Some(SimplifiedApprovalPrompt {
        primary_question: format!("Allow {} to proceed?", prompt.tool),
        affected_resource: prompt.detail.clone(),
        choices,
        risk_level,
    })
}

fn is_high_risk(envelope: &DecisionEnvelope) -> bool {
    envelope.risk_tags.iter().any(|tag| {
        matches!(
            tag,
            RiskTag::WritesOutsideWorkspace
                | RiskTag::WritesSensitiveFile
                | RiskTag::CredentialAccess
                | RiskTag::GitDestructive
                | RiskTag::SqlDestructive
                | RiskTag::SandboxExpansion
        )
    }) || matches!(
        envelope.source,
        DecisionSource::GitSafety { .. }
            | DecisionSource::SensitivePath { .. }
            | DecisionSource::ExecuteHardDeny { .. }
            | DecisionSource::SafetyMiddleware { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_engine::{ApprovalPrompt, DecisionEnvelope};

    fn envelope(source: DecisionSource, will_save: Option<String>) -> DecisionEnvelope {
        DecisionEnvelope {
            decision: HardDecision::NeedExternal {
                prompt: ApprovalPrompt {
                    tool: "bash".into(),
                    header: "Run command".into(),
                    detail: Some("git status".into()),
                    reason: "needs approval".into(),
                    risk_tags: vec![],
                },
            },
            source,
            trace: vec![],
            will_save,
            risk_tags: vec![],
        }
    }

    #[test]
    fn normal_prompt_uses_workspace_trust_choices_without_internal_terms() {
        let prompt = simplify_approval_prompt(&envelope(
            DecisionSource::ExplicitApprovalGate {
                reason: "execute".into(),
            },
            Some("allow bash prefix git".into()),
        ))
        .unwrap();

        assert_eq!(prompt.risk_level, ApprovalRiskLevel::Normal);
        assert_eq!(
            prompt.choices,
            vec![
                SimplifiedApprovalChoice::AllowOnce,
                SimplifiedApprovalChoice::AlwaysAllowSimilarInWorkspace,
                SimplifiedApprovalChoice::Reject,
                SimplifiedApprovalChoice::More,
            ]
        );
        let rendered = format!("{prompt:?}");
        for forbidden in ["Turn", "Project", "User", "Exact", "Prefix"] {
            assert!(
                !rendered.contains(forbidden),
                "{forbidden} leaked in {rendered}"
            );
        }
    }

    #[test]
    fn high_risk_prompt_does_not_offer_persistent_allow() {
        let prompt = simplify_approval_prompt(&envelope(
            DecisionSource::SensitivePath {
                path: "/etc/passwd".into(),
            },
            Some("allow sensitive".into()),
        ))
        .unwrap();

        assert_eq!(prompt.risk_level, ApprovalRiskLevel::High);
        assert!(
            !prompt
                .choices
                .contains(&SimplifiedApprovalChoice::AlwaysAllowSimilarInWorkspace)
        );
    }
}
