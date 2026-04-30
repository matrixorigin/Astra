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

/// Score from 0.0 (no) to 1.0 (yes) plus rationale.
///
/// `rationale` is the short version surfaced in inline detail lines
/// (≤ 200 chars). `full_rationale` holds the untruncated judge text
/// so a FAIL report can show everything the judge said — truncating
/// before persistence loses the most valuable diagnostic signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgerScore {
    pub score: f64,
    pub rationale: String,
    /// Full judge text (everything before the `SCORE:` line). Same as
    /// `rationale` when short; preserves diagnostic detail when long.
    #[serde(default)]
    pub full_rationale: String,
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
            // Carry the full judge text into `full_detail` so the
            // report can surface it on FAIL without re-running the
            // judger. Short `rationale` stays in the inline detail.
            let full_detail = if score.full_rationale == score.rationale {
                None
            } else {
                Some(format!(
                    "judger={:.2} (threshold={:.2})\n{}",
                    score.score, threshold, score.full_rationale
                ))
            };
            Some(CriterionResult {
                criterion: criterion.clone(),
                passed,
                detail: format!(
                    "judger={:.2} (threshold={:.2}) — {}",
                    score.score, threshold, score.rationale
                ),
                full_detail,
                score: Some(score.score),
            })
        }
        Err(e) => Some(CriterionResult {
            criterion: criterion.clone(),
            passed: false,
            detail: format!("judger call failed: {e}"),
            full_detail: None,
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

/// Assemble the judger prompt. Structured as: (1) rubric + anti-gaming
/// language, (2) what the agent produced (tool calls + final text),
/// (3) the yes/no question, (4) strict output format. The rubric is
/// explicit about claim-vs-evidence because the common judger failure
/// mode is scoring an agent high for *claiming* a task was done when
/// no supporting evidence exists in the tool calls or text.
fn build_judger_prompt(question: &str, outcome: &RunOutcome) -> String {
    format!(
        "You are a strict, skeptical test judge scoring an agent's run.\n\
         \n\
         ### Scoring rubric\n\
         - 1.0 — Question is fully answered YES, with concrete evidence in the \
         tool calls or final text. A simple factual reply counts as evidence for \
         an information question.\n\
         - 0.7 — Substantially YES but one concrete expectation is missing.\n\
         - 0.4 — Partial: some evidence but the core expectation is not met.\n\
         - 0.0 — NO, OR the agent only *claimed* success without observable evidence.\n\
         \n\
         ### Anti-gaming rules (VERY IMPORTANT)\n\
         - DO NOT reward mere self-reports. If the agent says \"I did X\" but no \
         tool call or output corroborates X, treat that as NO.\n\
         - Fabricated / hallucinated outputs score 0.0 regardless of confidence.\n\
         - If tool calls contradict the final text, prefer what the tool calls show.\n\
         - Extra unrelated output does not raise the score.\n\
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
         First: 2-4 sentences of rationale citing specific tool calls or quoted \
         text — no generic praise.\n\
         Last line, EXACTLY:\n\
         SCORE: <float between 0.0 and 1.0>",
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
        full_rationale: rationale,
    })
}

/// Aggregation mode for a `QuorumJudger`'s N runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuorumAgg {
    /// Median of N scores. Robust to a single outlier when N >= 3.
    Median,
    /// Mean of N scores.
    Mean,
    /// Minimum score. Paranoid: one LOW vote kills the case.
    Min,
    /// Maximum score. Used when multi-run is about "did it EVER agree".
    Max,
}

impl std::str::FromStr for QuorumAgg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "median" => Ok(QuorumAgg::Median),
            "mean" | "avg" | "average" => Ok(QuorumAgg::Mean),
            "min" => Ok(QuorumAgg::Min),
            "max" => Ok(QuorumAgg::Max),
            other => Err(format!("unknown quorum agg {other:?} (median|mean|min|max)")),
        }
    }
}

/// Decorator that runs an inner `Judger` N times per call and
/// aggregates the results. Reduces single-call variance, which is the
/// dominant source of flakiness when scoring stochastic model output.
///
/// The aggregated `score` drives pass/fail. The `rationale` of the
/// first-run is kept for the inline detail; `full_rationale` stitches
/// together every run's text labelled `--- run N ---` so a FAIL report
/// can show the dissenting votes.
pub struct QuorumJudger<J: Judger> {
    pub inner: J,
    pub n: u32,
    pub agg: QuorumAgg,
}

impl<J: Judger> QuorumJudger<J> {
    pub fn new(inner: J, n: u32, agg: QuorumAgg) -> Self {
        let n = n.max(1);
        Self { inner, n, agg }
    }
}

#[async_trait]
impl<J: Judger> Judger for QuorumJudger<J> {
    async fn score(
        &self,
        question: &str,
        model_override: Option<&str>,
        outcome: &RunOutcome,
    ) -> Result<JudgerScore, String> {
        let mut scores: Vec<f64> = Vec::with_capacity(self.n as usize);
        let mut rationales: Vec<String> = Vec::with_capacity(self.n as usize);
        let mut first_short: Option<String> = None;
        let mut errors: Vec<String> = Vec::new();
        for i in 0..self.n {
            match self.inner.score(question, model_override, outcome).await {
                Ok(s) => {
                    if first_short.is_none() {
                        first_short = Some(s.rationale.clone());
                    }
                    scores.push(s.score);
                    rationales.push(format!(
                        "--- run {}/{} (score={:.2}) ---\n{}",
                        i + 1,
                        self.n,
                        s.score,
                        s.full_rationale
                    ));
                }
                Err(e) => errors.push(format!("run {}/{}: {e}", i + 1, self.n)),
            }
        }
        if scores.is_empty() {
            return Err(format!(
                "all {} judge runs failed: {}",
                self.n,
                errors.join("; ")
            ));
        }
        let aggregated = aggregate_scores(&scores, self.agg);
        let mut full = rationales.join("\n\n");
        if !errors.is_empty() {
            full.push_str("\n\n--- errors ---\n");
            full.push_str(&errors.join("\n"));
        }
        Ok(JudgerScore {
            score: aggregated,
            rationale: first_short.unwrap_or_default(),
            full_rationale: full,
        })
    }
}

fn aggregate_scores(scores: &[f64], agg: QuorumAgg) -> f64 {
    debug_assert!(!scores.is_empty(), "aggregate_scores on empty slice");
    match agg {
        QuorumAgg::Mean => scores.iter().sum::<f64>() / scores.len() as f64,
        QuorumAgg::Min => scores.iter().cloned().fold(f64::INFINITY, f64::min),
        QuorumAgg::Max => scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        QuorumAgg::Median => {
            let mut s = scores.to_vec();
            // NaN shouldn't appear (parser clamps + rejects), but total_cmp
            // defends against future callers.
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = s.len() / 2;
            if s.len().is_multiple_of(2) {
                (s[mid - 1] + s[mid]) / 2.0
            } else {
                s[mid]
            }
        }
    }
}

/// Best-effort guess of a model's "family" for same-family warnings.
/// Matching is substring-based and conservative — we'd rather miss a
/// warning than fabricate a mismatch.
///
/// Returns `None` for unknown models; callers treat that as "can't tell".
pub fn model_family(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    for (needle, family) in &[
        ("claude", "anthropic"),
        ("sonnet", "anthropic"),
        ("opus", "anthropic"),
        ("haiku", "anthropic"),
        ("gpt", "openai"),
        ("o1", "openai"),
        ("o3", "openai"),
        ("qwen", "alibaba"),
        ("minimax", "minimax"),
        ("deepseek", "deepseek"),
        ("glm", "zhipu"),
        ("gemini", "google"),
    ] {
        if n.contains(needle) {
            return Some(family);
        }
    }
    None
}

/// Emit a warning to stderr when the judger model is in the same
/// family as any tested model. Returns true when a warning was
/// printed — mostly for tests.
pub fn warn_if_same_family(judger_model: &str, tested_models: &[String]) -> bool {
    let Some(judge_fam) = model_family(judger_model) else {
        return false;
    };
    let colluding: Vec<&String> = tested_models
        .iter()
        .filter(|m| model_family(m) == Some(judge_fam))
        .collect();
    if colluding.is_empty() {
        return false;
    }
    eprintln!(
        "[astra-test] WARNING: judger model {judger_model:?} is in the same \
         family ({judge_fam}) as tested model(s) {colluding:?}. Same-family \
         judging tends to inflate scores — consider --judger-model from a \
         different family for cleaner signal."
    );
    true
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
                full_rationale: "looks good".into(),
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
                full_rationale: "meh".into(),
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
                full_rationale: "n/a".into(),
            }),
        };
        let c = Criterion::ExitCode { code: 0 };
        assert!(evaluate_judger(&j, &c, &dummy_outcome()).await.is_none());
    }

    #[test]
    fn parse_score_preserves_full_rationale() {
        let body = "short\nvery long rationale that exceeds the 200 character \
                    truncation limit for the inline detail line, containing important \
                    debugging detail about what the judge actually observed so this \
                    must survive into full_rationale even when rationale is clipped.\n\
                    SCORE: 0.42";
        let s = parse_score_from_response(body).unwrap();
        // Inline field must be ≤ 200 chars for report compactness.
        assert!(s.rationale.chars().count() <= 200);
        // Full text must survive truncation — failing FAIL reports without
        // the full message is the 1 regression this guards against.
        assert!(s.full_rationale.contains("debugging detail"));
        assert!(s.full_rationale.len() > s.rationale.len());
    }

    // ── aggregate_scores pure-function tests ──

    #[test]
    fn aggregate_median_odd_and_even() {
        // Odd: exact middle.
        assert!((aggregate_scores(&[0.2, 0.5, 0.9], QuorumAgg::Median) - 0.5).abs() < 1e-9);
        // Even: average of two middles.
        assert!((aggregate_scores(&[0.2, 0.4, 0.6, 0.8], QuorumAgg::Median) - 0.5).abs() < 1e-9);
        // Single value: returns that value.
        assert!((aggregate_scores(&[0.77], QuorumAgg::Median) - 0.77).abs() < 1e-9);
    }

    #[test]
    fn aggregate_mean_min_max() {
        let s = &[0.2, 0.5, 0.9];
        assert!((aggregate_scores(s, QuorumAgg::Mean) - (1.6 / 3.0)).abs() < 1e-9);
        assert!((aggregate_scores(s, QuorumAgg::Min) - 0.2).abs() < 1e-9);
        assert!((aggregate_scores(s, QuorumAgg::Max) - 0.9).abs() < 1e-9);
    }

    // ── QuorumJudger tests ──

    /// Inner judger that cycles through a canned sequence of results per call.
    struct ScriptedJudger {
        results: std::sync::Mutex<std::collections::VecDeque<Result<JudgerScore, String>>>,
    }
    impl ScriptedJudger {
        fn new(seq: Vec<Result<JudgerScore, String>>) -> Self {
            Self {
                results: std::sync::Mutex::new(seq.into_iter().collect()),
            }
        }
    }
    #[async_trait]
    impl Judger for ScriptedJudger {
        async fn score(
            &self,
            _q: &str,
            _m: Option<&str>,
            _o: &RunOutcome,
        ) -> Result<JudgerScore, String> {
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("scripted sequence exhausted".into()))
        }
    }

    fn score_of(x: f64, r: &str) -> Result<JudgerScore, String> {
        Ok(JudgerScore {
            score: x,
            rationale: r.into(),
            full_rationale: r.into(),
        })
    }

    #[tokio::test]
    async fn quorum_median_survives_single_outlier() {
        // Two-of-three agree near 0.9; one outlier at 0.1. Median wins.
        // Single-judge flake would have failed this case with 0.1.
        let inner = ScriptedJudger::new(vec![
            score_of(0.9, "ok-1"),
            score_of(0.1, "flake"),
            score_of(0.85, "ok-2"),
        ]);
        let q = QuorumJudger::new(inner, 3, QuorumAgg::Median);
        let s = q.score("q", None, &dummy_outcome()).await.unwrap();
        assert!(s.score >= 0.85);
        // All 3 votes should be stitched into full_rationale so a reviewer
        // can see the outlier without rerunning.
        assert!(s.full_rationale.contains("run 1/3"));
        assert!(s.full_rationale.contains("run 2/3"));
        assert!(s.full_rationale.contains("run 3/3"));
        assert!(s.full_rationale.contains("flake"));
    }

    #[tokio::test]
    async fn quorum_min_paranoid_one_low_vote_kills_case() {
        let inner = ScriptedJudger::new(vec![
            score_of(0.9, "ok"),
            score_of(0.9, "ok"),
            score_of(0.3, "doubt"),
        ]);
        let q = QuorumJudger::new(inner, 3, QuorumAgg::Min);
        let s = q.score("q", None, &dummy_outcome()).await.unwrap();
        assert!((s.score - 0.3).abs() < 1e-9);
    }

    #[tokio::test]
    async fn quorum_tolerates_partial_errors_and_returns_available_scores() {
        // 1st run errors, 2nd + 3rd succeed. Median of 2 = (0.5+0.9)/2 = 0.7.
        let inner = ScriptedJudger::new(vec![
            Err("transient network blip".into()),
            score_of(0.5, "maybe"),
            score_of(0.9, "yes"),
        ]);
        let q = QuorumJudger::new(inner, 3, QuorumAgg::Median);
        let s = q.score("q", None, &dummy_outcome()).await.unwrap();
        assert!((s.score - 0.7).abs() < 1e-9);
        assert!(s.full_rationale.contains("--- errors ---"));
        assert!(s.full_rationale.contains("transient network blip"));
    }

    #[tokio::test]
    async fn quorum_fails_when_all_runs_fail() {
        let inner = ScriptedJudger::new(vec![
            Err("timeout".into()),
            Err("rate limit".into()),
        ]);
        let q = QuorumJudger::new(inner, 2, QuorumAgg::Median);
        let res = q.score("q", None, &dummy_outcome()).await;
        let err = res.unwrap_err();
        assert!(err.contains("all 2 judge runs failed"));
        assert!(err.contains("timeout"));
        assert!(err.contains("rate limit"));
    }

    // ── model_family + warn_if_same_family tests ──

    #[test]
    fn model_family_recognizes_common_providers() {
        assert_eq!(model_family("claude-sonnet-4-6"), Some("anthropic"));
        assert_eq!(model_family("us.anthropic.claude-opus-4-7"), Some("anthropic"));
        assert_eq!(model_family("gpt-4o"), Some("openai"));
        assert_eq!(model_family("qwen-flash"), Some("alibaba"));
        assert_eq!(model_family("MiniMax-M2.7"), Some("minimax"));
        assert_eq!(model_family("some-obscure-model"), None);
    }

    #[test]
    fn warn_if_same_family_flags_collusion() {
        // Judger sonnet scoring sonnet → warning fires.
        assert!(warn_if_same_family(
            "claude-sonnet-4-6",
            &["us.anthropic.claude-opus-4-7".into()],
        ));
        // Cross-family: no warning.
        assert!(!warn_if_same_family(
            "claude-sonnet-4-6",
            &["qwen-flash".into()],
        ));
        // Unknown judger: no warning (we can't tell).
        assert!(!warn_if_same_family(
            "some-unknown-model",
            &["claude-sonnet-4-6".into()],
        ));
    }
}
