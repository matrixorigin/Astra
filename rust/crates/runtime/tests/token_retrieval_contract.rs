use std::{fs, path::PathBuf};

use serde::Deserialize;
use serde_json::Value;

// ─── Fixture types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Contract {
    context_budget_cases: Vec<ContextBudgetCase>,
    compaction_tier_cases: Vec<CompactionTierCase>,
    token_estimation_cases: Vec<TokenEstimationCase>,
    retrieval_cases: Vec<RetrievalCase>,
    adaptive_budget_cases: Vec<AdaptiveBudgetCase>,
}

#[derive(Deserialize)]
struct ContextBudgetCase {
    name: String,
    model: String,
    expected_model_limit: usize,
    expected_output_reserve_min: f64,
    expected_output_reserve_max: f64,
}

#[derive(Deserialize)]
struct CompactionTierCase {
    name: String,
    usage_ratio: f64,
    expected_tier: String,
}

#[derive(Deserialize)]
struct TokenEstimationCase {
    name: String,
    messages: Vec<Value>,
    expected_min: usize,
    expected_max: usize,
}

#[derive(Deserialize)]
struct RetrievalCase {
    name: String,
    query: String,
    history: Vec<Value>,
    recent_count: usize,
    expect_retrieval: bool,
    expect_contains: Option<String>,
}

#[derive(Deserialize)]
struct AdaptiveBudgetCase {
    name: String,
    query: String,
    expected_min: usize,
    expected_max: usize,
}

fn load_contract() -> Contract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/token_retrieval_contract.json");
    let content = fs::read_to_string(path).expect("fixture should exist");
    serde_json::from_str(&content).expect("fixture should be valid JSON")
}

// ─── Token estimation tests ─────────────────────────────────────────────────

#[test]
fn token_estimation_matches_contract() {
    use mo_agent_runtime::prompts::estimate_tokens;

    let contract = load_contract();
    for case in &contract.token_estimation_cases {
        let tokens = estimate_tokens(&case.messages);
        assert!(
            tokens >= case.expected_min && tokens <= case.expected_max,
            "{}: estimated {} tokens, expected {}..{}",
            case.name,
            tokens,
            case.expected_min,
            case.expected_max,
        );
    }
}

// ─── Context budget model-aware tests ───────────────────────────────────────

#[test]
fn context_budget_model_limits_match_contract() {
    use mo_agent_runtime::prompts::budget_for_model;

    let contract = load_contract();
    for case in &contract.context_budget_cases {
        let budget = budget_for_model(Some(&case.model));
        assert_eq!(
            budget.model_limit, case.expected_model_limit,
            "{}: model_limit mismatch",
            case.name,
        );
        assert!(
            budget.output_reserve_ratio >= case.expected_output_reserve_min
                && budget.output_reserve_ratio <= case.expected_output_reserve_max,
            "{}: output_reserve_ratio {} not in {}..{}",
            case.name,
            budget.output_reserve_ratio,
            case.expected_output_reserve_min,
            case.expected_output_reserve_max,
        );
    }
}

#[test]
fn effective_input_limit_less_than_model_limit() {
    use mo_agent_runtime::prompts::budget_for_model;

    let contract = load_contract();
    for case in &contract.context_budget_cases {
        let budget = budget_for_model(Some(&case.model));
        let effective = budget.effective_input_limit();
        assert!(
            effective < budget.model_limit,
            "{}: effective_input_limit {} should be less than model_limit {}",
            case.name,
            effective,
            budget.model_limit,
        );
        assert!(
            effective > budget.model_limit / 2,
            "{}: effective_input_limit {} too small (less than half of {})",
            case.name,
            effective,
            budget.model_limit,
        );
    }
}

// ─── Compaction tier tests ──────────────────────────────────────────────────

#[test]
fn compaction_tiers_match_contract() {
    use mo_agent_runtime::prompts::{CompactionTier, budget_for_model};

    let contract = load_contract();
    let budget = budget_for_model(Some("gpt-4o"));
    let effective = budget.effective_input_limit();

    for case in &contract.compaction_tier_cases {
        let tokens = (effective as f64 * case.usage_ratio) as usize;
        let tier = budget.compaction_tier(tokens);
        let tier_name = match tier {
            CompactionTier::Normal => "Normal",
            CompactionTier::TrimSchemas => "TrimSchemas",
            CompactionTier::CompactHistory => "CompactHistory",
            CompactionTier::AggressivePrune => "AggressivePrune",
        };
        assert_eq!(
            tier_name,
            case.expected_tier,
            "{}: at {}% usage ({} tokens of {} effective), expected {} got {}",
            case.name,
            (case.usage_ratio * 100.0) as u32,
            tokens,
            effective,
            case.expected_tier,
            tier_name,
        );
    }
}

// ─── Retrieval tests ────────────────────────────────────────────────────────

#[test]
fn retrieval_extraction_matches_contract() {
    use mo_agent_runtime::turn::retrieval::{RETRIEVAL_BUDGET_CHARS, rule_based_extraction};
    use serde_json::Map;

    let contract = load_contract();
    for case in &contract.retrieval_cases {
        let history: Vec<Map<String, Value>> = case
            .history
            .iter()
            .filter_map(|v| v.as_object().cloned())
            .collect();
        let recent = history[history.len().saturating_sub(case.recent_count)..].to_vec();

        let result = rule_based_extraction(&history, &recent, &case.query, RETRIEVAL_BUDGET_CHARS);

        if case.expect_retrieval {
            assert!(
                result.is_some(),
                "{}: expected retrieval but got None",
                case.name,
            );
            if let Some(ref pattern) = case.expect_contains {
                assert!(
                    result.as_ref().unwrap().contains(pattern),
                    "{}: result should contain '{}', got: {}",
                    case.name,
                    pattern,
                    result.unwrap(),
                );
            }
        } else {
            assert!(
                result.is_none(),
                "{}: expected no retrieval but got: {:?}",
                case.name,
                result,
            );
        }
    }
}

// ─── Adaptive budget tests ──────────────────────────────────────────────────

#[test]
fn adaptive_budget_matches_contract() {
    use mo_agent_runtime::turn::retrieval::adaptive_budget_chars;

    let contract = load_contract();
    for case in &contract.adaptive_budget_cases {
        let budget = adaptive_budget_chars(&case.query);
        assert!(
            budget >= case.expected_min && budget <= case.expected_max,
            "{}: budget {} not in {}..{}",
            case.name,
            budget,
            case.expected_min,
            case.expected_max,
        );
    }
}
