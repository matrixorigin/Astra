//! Agent judger — scores free-form success criteria by asking an
//! LLM "did the agent do X?" and parsing the response for a
//! floating-point score in [0.0, 1.0].
//!
//! ## Layout
//!
//! - [`Judger`] trait — injectable scoring backend. Tests use
//!   `FakeJudger`; production uses [`AstraCliJudger`].
//! - [`AstraCliJudger`] — shells out to `astra chat -m <prompt>
//!   --model <m> --json --quiet` and parses the response.
//! - [`parse_score_from_response`] — pure parser for `SCORE: <f>`.
//!   Separated so a flaky judger response can be debugged without
//!   re-invoking the provider.
//!
//! ## Why a subprocess judger?
//!
//! No new HTTP client, no new auth surface — the judger inherits the
//! same tool-restriction gate as the regular CLI, so it can't
//! accidentally spawn sub-agents or mutate state while scoring.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::criteria::{Criterion, CriterionResult};
use crate::runner::RunOutcome;

/// Score from 0.0 (no) to 1.0 (yes) plus one-line rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgerScore {
    pub score: f64,
    pub rationale: String,
}

#[derive(Debug, Clone)]
pub struct JudgerConfig {
    pub astra_bin: PathBuf,
    /// Default model to use when a Judger criterion doesn't
    /// specify its own.
    pub default_model: String,
    /// Timeout for each judger call.
    pub timeout_seconds: u64,
}

impl JudgerConfig {
    pub fn new(astra_bin: impl Into<PathBuf>, default_model: impl Into<String>) -> Self {
        Self {
            astra_bin: astra_bin.into(),
            default_model: default_model.into(),
            timeout_seconds: 60,
        }
    }
}

/// Injectable scoring backend. Tests use a fake impl; production uses
/// [`AstraCliJudger`]. Returning a `Result<JudgerScore, String>` keeps
/// error messages human-readable in the report.
#[async_trait]
pub trait Judger: Send + Sync {
    async fn score(
        &self,
        question: &str,
        model_override: Option<&str>,
        outcome: &RunOutcome,
    ) -> Result<JudgerScore, String>;
}

/// Run one Judger criterion against an outcome using the provided
/// scoring backend. Non-Judger variants return None so callers can
/// skip them.
pub async fn evaluate_judger(
    judger: &dyn Judger,
    criterion: &Criterion,
    outcome: &RunOutcome,
) -> Option<CriterionResult> {
    let Criterion::Judger {
        question,
        threshold,
        model,
    } = criterion
    else {
        return None;
    };

    let result = judger.score(question, model.as_deref(), outcome).await;
    match result {
        Ok(score) => {
            let passed = score.score >= *threshold;
            Some(CriterionResult {
                criterion: criterion.clone(),
                passed,
                detail: format!(
                    "judger={:.2} (threshold={:.2}) — {}",
                    score.score, threshold, score.rationale
                ),
                score: Some(score.score),
            })
        }
        Err(e) => Some(CriterionResult {
            criterion: criterion.clone(),
            passed: false,
            detail: format!("judger call failed: {e}"),
            score: None,
        }),
    }
}

/// Production `Judger` impl — shells out to the astra CLI.
pub struct AstraCliJudger {
    pub cfg: JudgerConfig,
}

impl AstraCliJudger {
    pub fn new(cfg: JudgerConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Judger for AstraCliJudger {
    async fn score(
        &self,
        question: &str,
        model_override: Option<&str>,
        outcome: &RunOutcome,
    ) -> Result<JudgerScore, String> {
        let judger_model = model_override
            .unwrap_or(self.cfg.default_model.as_str())
            .to_string();
        let prompt = build_judger_prompt(question, outcome);
        run_judger_call(&self.cfg, &judger_model, &prompt).await
    }
}

/// Assemble the judger prompt. Structured as: (1) what the agent
/// was asked, (2) what the agent produced, (3) the yes/no
/// question, (4) explicit scoring rubric with a machine-readable
/// line format the parser expects.
fn build_judger_prompt(question: &str, outcome: &RunOutcome) -> String {
    format!(
        "You are a strict test judge. Score an agent's behavior on a yes/no question.\n\
         \n\
         ### Agent tool calls\n\
         {tools_used:?} (total {tool_calls_count} calls)\n\
         \n\
         ### Agent final output (text)\n\
         {text}\n\
         \n\
         ### Question\n\
         {question}\n\
         \n\
         ### Output format (STRICT)\n\
         End your response with EXACTLY this line (no other formatting):\n\
         SCORE: <float between 0.0 and 1.0>\n\
         Precede that with one short sentence of rationale.",
        tools_used = outcome.tools_used,
        tool_calls_count = outcome.tool_calls_count,
        text = outcome.text,
        question = question,
    )
}

/// Actually invoke the judger model via the astra CLI and parse
/// the response. Kept as a plain async fn so tests can mock it by
/// substituting `astra_bin`.
async fn run_judger_call(
    cfg: &JudgerConfig,
    model: &str,
    prompt: &str,
) -> Result<JudgerScore, String> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    let mut cmd = Command::new(&cfg.astra_bin);
    cmd.arg("chat")
        .arg("-m")
        .arg(prompt)
        .arg("--model")
        .arg(model)
        .arg("--json")
        .arg("--quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut buf = String::new();
    let timeout = Duration::from_secs(cfg.timeout_seconds);
    let read_fut = async move {
        let _ = stdout.read_to_string(&mut buf).await;
        let _ = child.wait().await;
        buf
    };

    let stdout_body = tokio::time::timeout(timeout, read_fut)
        .await
        .map_err(|_| format!("judger timeout after {}s", timeout.as_secs()))?;

    parse_score_from_response(&stdout_body)
}

/// Extract `SCORE: <f>` from a judger response. We parse the
/// astra CLI's JSON envelope first and scan its `text` field.
pub(crate) fn parse_score_from_response(stdout_body: &str) -> Result<JudgerScore, String> {
    let trimmed = stdout_body.trim();
    // astra chat --json returns an object with text; fall back to
    // raw body if parse fails.
    let text = serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| trimmed.to_string());

    // Scan for the last line matching "SCORE: <f>". Last-line wins
    // in case the model repeats itself.
    let re = regex::Regex::new(r"(?i)SCORE:\s*([0-9]+(?:\.[0-9]+)?)")
        .map_err(|e| format!("regex compile: {e}"))?;
    let captured = re
        .captures_iter(&text)
        .last()
        .and_then(|c| c.get(1))
        .ok_or_else(|| format!("no SCORE: line in judger response; text={text:?}"))?;
    let score: f64 = captured
        .as_str()
        .parse()
        .map_err(|e| format!("parse score {:?}: {e}", captured.as_str()))?;
    // Clamp in case the model returned >1.0.
    let clamped = score.clamp(0.0, 1.0);
    // Rationale = everything before the SCORE line, last sentence.
    let rationale = text
        .rsplit_once("SCORE:")
        .map(|(prefix, _)| prefix.trim().to_string())
        .unwrap_or_default();
    Ok(JudgerScore {
        score: clamped,
        rationale: rationale.chars().take(200).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_score_happy_path() {
        let body = r#"{"text":"The agent did invoke the tool correctly.\nSCORE: 0.85"}"#;
        let s = parse_score_from_response(body).unwrap();
        assert!((s.score - 0.85).abs() < 1e-9);
        assert!(s.rationale.contains("did invoke"));
    }

    #[test]
    fn parse_score_clamps_above_one() {
        let body = r#"{"text":"OK\nSCORE: 1.5"}"#;
        let s = parse_score_from_response(body).unwrap();
        assert!((s.score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_score_last_wins() {
        // The model may re-state SCORE accidentally; take the last
        // one so a self-correcting response scores as intended.
        let body = r#"{"text":"initial thought\nSCORE: 0.3\nactually wait\nSCORE: 0.9"}"#;
        let s = parse_score_from_response(body).unwrap();
        assert!((s.score - 0.9).abs() < 1e-9);
    }

    #[test]
    fn parse_score_missing_line_fails() {
        let body = r#"{"text":"I have no opinion"}"#;
        assert!(parse_score_from_response(body).is_err());
    }

    #[test]
    fn parse_score_accepts_raw_body_without_json_envelope() {
        // Judger model might print raw text for some reason. Don't
        // fail — just scan the raw body.
        let body = "whatever\nSCORE: 0.55";
        let s = parse_score_from_response(body).unwrap();
        assert!((s.score - 0.55).abs() < 1e-9);
    }

    fn dummy_outcome() -> RunOutcome {
        RunOutcome {
            model: "m".into(),
            exit_code: 0,
            text: "agent output".into(),
            stderr: String::new(),
            session_id: None,
            run_id: None,
            tool_calls_count: 0,
            tools_used: vec![],
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: 0,
        }
    }

    /// In-memory Judger impl that returns canned scores. Lives in
    /// tests because no production caller should want canned scores.
    pub struct FakeJudger {
        pub result: Result<JudgerScore, String>,
    }

    #[async_trait]
    impl Judger for FakeJudger {
        async fn score(
            &self,
            _question: &str,
            _model_override: Option<&str>,
            _outcome: &RunOutcome,
        ) -> Result<JudgerScore, String> {
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn evaluate_judger_passes_when_score_meets_threshold() {
        let j = FakeJudger {
            result: Ok(JudgerScore {
                score: 0.9,
                rationale: "looks good".into(),
            }),
        };
        let c = Criterion::Judger {
            question: "q?".into(),
            threshold: 0.7,
            model: None,
        };
        let r = evaluate_judger(&j, &c, &dummy_outcome()).await.unwrap();
        assert!(r.passed);
        assert!(r.detail.contains("0.90"));
        assert!(r.detail.contains("looks good"));
    }

    #[tokio::test]
    async fn evaluate_judger_fails_below_threshold() {
        let j = FakeJudger {
            result: Ok(JudgerScore {
                score: 0.5,
                rationale: "meh".into(),
            }),
        };
        let c = Criterion::Judger {
            question: "q?".into(),
            threshold: 0.7,
            model: None,
        };
        let r = evaluate_judger(&j, &c, &dummy_outcome()).await.unwrap();
        assert!(!r.passed);
    }

    #[tokio::test]
    async fn evaluate_judger_fails_on_backend_error() {
        let j = FakeJudger {
            result: Err("rate limit".into()),
        };
        let c = Criterion::Judger {
            question: "q?".into(),
            threshold: 0.7,
            model: None,
        };
        let r = evaluate_judger(&j, &c, &dummy_outcome()).await.unwrap();
        assert!(!r.passed);
        assert!(r.detail.contains("judger call failed"));
        assert!(r.detail.contains("rate limit"));
    }

    #[tokio::test]
    async fn evaluate_judger_returns_none_for_non_judger_variant() {
        let j = FakeJudger {
            result: Ok(JudgerScore {
                score: 1.0,
                rationale: "n/a".into(),
            }),
        };
        let c = Criterion::ExitCode { code: 0 };
        assert!(evaluate_judger(&j, &c, &dummy_outcome()).await.is_none());
    }
}
