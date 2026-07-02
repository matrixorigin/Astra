//! Capacity equations and rollout gates for multi-pod runtime scaling.
//!
//! This module deliberately separates capacity *math* from the mechanisms that
//! enforce limits. The mechanisms already live in different places: run
//! admission uses a per-pod semaphore, tool execution uses a process-wide
//! semaphore, endpoint RPC uses immediate rejection, and provider rate limits
//! behave like a circuit breaker. The model below gives operators one place to
//! ask whether a configured pod count is safe before they scale it out.

use astra_turn_core::pipeline_metrics::MetricsRegistry;

const DEFAULT_POD_COUNT: u32 = 1;
const DEFAULT_RUN_SLOTS_PER_POD: u32 = 50;
const DEFAULT_TOOL_SLOTS_PER_POD: u32 = 10;
const DEFAULT_ENDPOINT_RPC_SLOTS_PER_POD: u32 = 128;
const DEFAULT_CONTROL_POLL_INTERVAL_MS: u64 = 500;
const DEFAULT_TARGET_UTILIZATION: f64 = 0.80;

const ENV_POD_COUNT: &str = "ASTRA_CAPACITY_POD_COUNT";
const ENV_RUN_LIMIT: &str = "ASTRA_RUN_CONCURRENCY_LIMIT";
const ENV_TOOL_LIMIT: &str = "ASTRA_MAX_CONCURRENT_TOOL_EXECUTIONS";
const ENV_ENDPOINT_LIMIT: &str = "ASTRA_ENDPOINT_RPC_CONCURRENCY";
const ENV_PROVIDER_RPM: &str = "ASTRA_CAPACITY_PROVIDER_RPM";
const ENV_PROVIDER_TPM: &str = "ASTRA_CAPACITY_PROVIDER_TPM";
const ENV_AVG_TOKENS: &str = "ASTRA_CAPACITY_AVG_TOKENS_PER_LLM_REQUEST";
const ENV_AVG_LLM_REQUESTS_PER_RUN_MIN: &str =
    "ASTRA_CAPACITY_AVG_LLM_REQUESTS_PER_ACTIVE_RUN_PER_MINUTE";
const ENV_CONTROL_POLL_INTERVAL_MS: &str = "ASTRA_CAPACITY_CONTROL_POLL_INTERVAL_MS";
const ENV_MAX_CONTROL_POLL_QPS: &str = "ASTRA_CAPACITY_MAX_CONTROL_POLL_QPS";
const ENV_TARGET_UTILIZATION: &str = "ASTRA_CAPACITY_TARGET_UTILIZATION";

const METRIC_PODS: &str = "astra_capacity_pods_configured";
const METRIC_RUN_SLOTS: &str = "astra_capacity_run_slots_total";
const METRIC_TOOL_SLOTS: &str = "astra_capacity_tool_slots_total";
const METRIC_ENDPOINT_SLOTS: &str = "astra_capacity_endpoint_rpc_slots_total";
const METRIC_PROVIDER_RPM: &str = "astra_capacity_provider_rpm_effective";
const METRIC_PROVIDER_RPM_KNOWN: &str = "astra_capacity_provider_rpm_known";
const METRIC_PROVIDER_RPM_DEMAND: &str = "astra_capacity_provider_rpm_demand_estimate";
const METRIC_PROVIDER_RPM_DEMAND_KNOWN: &str = "astra_capacity_provider_rpm_demand_known";
const METRIC_CONTROL_POLL_QPS: &str = "astra_capacity_control_poll_qps_estimate";
const METRIC_ROLLOUT_ALLOWED: &str = "astra_capacity_rollout_allowed";
const METRIC_ROLLOUT_RISK: &str = "astra_capacity_rollout_risk";
const METRIC_LIMIT_MODE: &str = "astra_capacity_limit_mode";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitMode {
    Wait,
    WaitThenReject,
    Reject,
    CircuitBreaker,
}

impl LimitMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::WaitThenReject => "wait_then_reject",
            Self::Reject => "reject",
            Self::CircuitBreaker => "circuit_breaker",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LimitSemantics {
    pub(crate) name: &'static str,
    pub(crate) scope: &'static str,
    pub(crate) mode: LimitMode,
    pub(crate) env_var: Option<&'static str>,
}

pub(crate) const LIMIT_SEMANTICS: &[LimitSemantics] = &[
    LimitSemantics {
        name: "run_admission",
        scope: "per_pod",
        mode: LimitMode::WaitThenReject,
        env_var: Some(ENV_RUN_LIMIT),
    },
    LimitSemantics {
        name: "tool_execution",
        scope: "per_process",
        mode: LimitMode::Wait,
        env_var: Some(ENV_TOOL_LIMIT),
    },
    LimitSemantics {
        name: "registered_endpoint_rpc",
        scope: "per_endpoint_per_pod",
        mode: LimitMode::Reject,
        env_var: Some(ENV_ENDPOINT_LIMIT),
    },
    LimitSemantics {
        name: "provider_rate_limit",
        scope: "global_external",
        mode: LimitMode::CircuitBreaker,
        env_var: None,
    },
    LimitSemantics {
        name: "per_user_quota",
        scope: "per_user",
        mode: LimitMode::Reject,
        env_var: None,
    },
];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CapacityInput {
    pub(crate) pod_count: u32,
    pub(crate) run_slots_per_pod: u32,
    pub(crate) tool_slots_per_pod: u32,
    pub(crate) endpoint_rpc_slots_per_pod: u32,
    pub(crate) provider_rpm_budget: Option<u64>,
    pub(crate) provider_tpm_budget: Option<u64>,
    pub(crate) avg_tokens_per_llm_request: Option<u64>,
    pub(crate) avg_llm_requests_per_active_run_per_minute: Option<f64>,
    pub(crate) control_poll_interval_ms: u64,
    pub(crate) max_control_poll_qps: Option<f64>,
    pub(crate) target_utilization: f64,
}

impl Default for CapacityInput {
    fn default() -> Self {
        Self {
            pod_count: DEFAULT_POD_COUNT,
            run_slots_per_pod: DEFAULT_RUN_SLOTS_PER_POD,
            tool_slots_per_pod: DEFAULT_TOOL_SLOTS_PER_POD,
            endpoint_rpc_slots_per_pod: DEFAULT_ENDPOINT_RPC_SLOTS_PER_POD,
            provider_rpm_budget: None,
            provider_tpm_budget: None,
            avg_tokens_per_llm_request: None,
            avg_llm_requests_per_active_run_per_minute: None,
            control_poll_interval_ms: DEFAULT_CONTROL_POLL_INTERVAL_MS,
            max_control_poll_qps: None,
            target_utilization: DEFAULT_TARGET_UTILIZATION,
        }
    }
}

impl CapacityInput {
    pub(crate) fn from_env() -> Self {
        Self {
            pod_count: env_u32(ENV_POD_COUNT, DEFAULT_POD_COUNT),
            run_slots_per_pod: env_u32(ENV_RUN_LIMIT, DEFAULT_RUN_SLOTS_PER_POD),
            tool_slots_per_pod: env_u32(ENV_TOOL_LIMIT, DEFAULT_TOOL_SLOTS_PER_POD),
            endpoint_rpc_slots_per_pod: env_u32(
                ENV_ENDPOINT_LIMIT,
                DEFAULT_ENDPOINT_RPC_SLOTS_PER_POD,
            ),
            provider_rpm_budget: env_optional_u64(ENV_PROVIDER_RPM),
            provider_tpm_budget: env_optional_u64(ENV_PROVIDER_TPM),
            avg_tokens_per_llm_request: env_optional_u64(ENV_AVG_TOKENS),
            avg_llm_requests_per_active_run_per_minute: env_optional_f64(
                ENV_AVG_LLM_REQUESTS_PER_RUN_MIN,
            ),
            control_poll_interval_ms: env_u64(
                ENV_CONTROL_POLL_INTERVAL_MS,
                DEFAULT_CONTROL_POLL_INTERVAL_MS,
            ),
            max_control_poll_qps: env_optional_f64(ENV_MAX_CONTROL_POLL_QPS),
            target_utilization: env_optional_f64(ENV_TARGET_UTILIZATION)
                .filter(|v| *v > 0.0 && *v <= 1.0)
                .unwrap_or(DEFAULT_TARGET_UTILIZATION),
        }
    }

    pub(crate) fn evaluate(&self) -> CapacityPlan {
        let total_run_slots = self.pod_count.saturating_mul(self.run_slots_per_pod);
        let total_tool_slots = self.pod_count.saturating_mul(self.tool_slots_per_pod);
        let total_endpoint_rpc_slots = self
            .pod_count
            .saturating_mul(self.endpoint_rpc_slots_per_pod);
        let poll_interval_ms = self.control_poll_interval_ms.max(1);
        let estimated_control_poll_qps = total_run_slots as f64 * 1000.0 / poll_interval_ms as f64;
        let effective_provider_rpm = effective_provider_rpm(
            self.provider_rpm_budget,
            self.provider_tpm_budget,
            self.avg_tokens_per_llm_request,
        );
        let estimated_provider_rpm_demand = self
            .avg_llm_requests_per_active_run_per_minute
            .map(|requests_per_run| total_run_slots as f64 * requests_per_run);

        let decision = rollout_decision(
            effective_provider_rpm,
            estimated_provider_rpm_demand,
            estimated_control_poll_qps,
            self.max_control_poll_qps,
            self.target_utilization,
        );

        CapacityPlan {
            pod_count: self.pod_count,
            total_run_slots,
            total_tool_slots,
            total_endpoint_rpc_slots,
            effective_provider_rpm,
            estimated_provider_rpm_demand,
            estimated_control_poll_qps,
            decision,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CapacityPlan {
    pub(crate) pod_count: u32,
    pub(crate) total_run_slots: u32,
    pub(crate) total_tool_slots: u32,
    pub(crate) total_endpoint_rpc_slots: u32,
    pub(crate) effective_provider_rpm: Option<f64>,
    pub(crate) estimated_provider_rpm_demand: Option<f64>,
    pub(crate) estimated_control_poll_qps: f64,
    pub(crate) decision: RolloutDecision,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RolloutDecision {
    pub(crate) allowed: bool,
    pub(crate) risks: Vec<RolloutRisk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RolloutRisk {
    ProviderEvidenceMissing,
    ProviderOversubscribed,
    ControlPollOversubscribed,
}

impl RolloutRisk {
    fn as_str(self) -> &'static str {
        match self {
            Self::ProviderEvidenceMissing => "provider_evidence_missing",
            Self::ProviderOversubscribed => "provider_oversubscribed",
            Self::ControlPollOversubscribed => "control_poll_oversubscribed",
        }
    }
}

pub(crate) fn register_capacity_metrics(registry: &MetricsRegistry) {
    registry.register_gauge(
        METRIC_PODS,
        "Configured runtime pod count used for capacity math.",
    );
    registry.register_gauge(
        METRIC_RUN_SLOTS,
        "Total run admission slots across configured pods.",
    );
    registry.register_gauge(
        METRIC_TOOL_SLOTS,
        "Total tool execution slots across configured pods.",
    );
    registry.register_gauge(
        METRIC_ENDPOINT_SLOTS,
        "Total registered endpoint RPC slots per endpoint across configured pods.",
    );
    registry.register_gauge(
        METRIC_PROVIDER_RPM,
        "Effective provider request-per-minute budget after RPM/TPM constraints.",
    );
    registry.register_gauge(
        METRIC_PROVIDER_RPM_KNOWN,
        "1 when provider RPM capacity evidence is configured, else 0.",
    );
    registry.register_gauge(
        METRIC_PROVIDER_RPM_DEMAND,
        "Estimated provider request-per-minute demand at full run-slot occupancy.",
    );
    registry.register_gauge(
        METRIC_PROVIDER_RPM_DEMAND_KNOWN,
        "1 when provider demand estimate inputs are configured, else 0.",
    );
    registry.register_gauge(
        METRIC_CONTROL_POLL_QPS,
        "Estimated DB/control-plane poll QPS at full run-slot occupancy.",
    );
    registry.register_gauge(
        METRIC_ROLLOUT_ALLOWED,
        "1 when current capacity evidence passes rollout gates, else 0.",
    );
    registry.register_gauge(
        METRIC_ROLLOUT_RISK,
        "Per-risk rollout gate status; 1 means the risk is active.",
    );
    registry.register_gauge(
        METRIC_LIMIT_MODE,
        "Configured limit semantics by limit name, scope, and enforcement mode.",
    );
}

pub(crate) fn scrape_capacity_metrics_from_env(registry: &MetricsRegistry) {
    scrape_capacity_metrics(registry, &CapacityInput::from_env().evaluate());
}

pub(crate) fn scrape_capacity_metrics(registry: &MetricsRegistry, plan: &CapacityPlan) {
    register_capacity_metrics(registry);
    registry.set_gauge(METRIC_PODS, &[], plan.pod_count as f64);
    registry.set_gauge(METRIC_RUN_SLOTS, &[], plan.total_run_slots as f64);
    registry.set_gauge(METRIC_TOOL_SLOTS, &[], plan.total_tool_slots as f64);
    registry.set_gauge(
        METRIC_ENDPOINT_SLOTS,
        &[],
        plan.total_endpoint_rpc_slots as f64,
    );
    set_optional_gauge(
        registry,
        METRIC_PROVIDER_RPM,
        METRIC_PROVIDER_RPM_KNOWN,
        plan.effective_provider_rpm,
    );
    set_optional_gauge(
        registry,
        METRIC_PROVIDER_RPM_DEMAND,
        METRIC_PROVIDER_RPM_DEMAND_KNOWN,
        plan.estimated_provider_rpm_demand,
    );
    registry.set_gauge(
        METRIC_CONTROL_POLL_QPS,
        &[],
        plan.estimated_control_poll_qps,
    );
    registry.set_gauge(
        METRIC_ROLLOUT_ALLOWED,
        &[],
        if plan.decision.allowed { 1.0 } else { 0.0 },
    );

    for risk in [
        RolloutRisk::ProviderEvidenceMissing,
        RolloutRisk::ProviderOversubscribed,
        RolloutRisk::ControlPollOversubscribed,
    ] {
        let active = plan.decision.risks.contains(&risk);
        registry.set_gauge(
            METRIC_ROLLOUT_RISK,
            &[("risk", risk.as_str())],
            if active { 1.0 } else { 0.0 },
        );
    }

    for limit in LIMIT_SEMANTICS {
        registry.set_gauge(
            METRIC_LIMIT_MODE,
            &[
                ("limit", limit.name),
                ("scope", limit.scope),
                ("mode", limit.mode.as_str()),
                ("env_var", limit.env_var.unwrap_or("")),
            ],
            1.0,
        );
    }
}

fn rollout_decision(
    effective_provider_rpm: Option<f64>,
    estimated_provider_rpm_demand: Option<f64>,
    estimated_control_poll_qps: f64,
    max_control_poll_qps: Option<f64>,
    target_utilization: f64,
) -> RolloutDecision {
    let mut risks = Vec::new();
    match (effective_provider_rpm, estimated_provider_rpm_demand) {
        (Some(provider_rpm), Some(demand_rpm)) => {
            if demand_rpm > provider_rpm * target_utilization {
                risks.push(RolloutRisk::ProviderOversubscribed);
            }
        }
        _ => risks.push(RolloutRisk::ProviderEvidenceMissing),
    }
    if let Some(max_qps) = max_control_poll_qps
        && estimated_control_poll_qps > max_qps * target_utilization
    {
        risks.push(RolloutRisk::ControlPollOversubscribed);
    }
    RolloutDecision {
        allowed: risks.is_empty(),
        risks,
    }
}

fn effective_provider_rpm(
    provider_rpm_budget: Option<u64>,
    provider_tpm_budget: Option<u64>,
    avg_tokens_per_llm_request: Option<u64>,
) -> Option<f64> {
    let rpm = provider_rpm_budget.map(|value| value as f64);
    let tpm_as_rpm = provider_tpm_budget.and_then(|tpm| {
        avg_tokens_per_llm_request
            .filter(|tokens| *tokens > 0)
            .map(|tokens| tpm as f64 / tokens as f64)
    });
    match (rpm, tpm_as_rpm) {
        (Some(rpm), Some(tpm_rpm)) => Some(rpm.min(tpm_rpm)),
        (Some(rpm), None) => Some(rpm),
        (None, Some(tpm_rpm)) => Some(tpm_rpm),
        (None, None) => None,
    }
}

fn set_optional_gauge(
    registry: &MetricsRegistry,
    value_metric: &str,
    known_metric: &str,
    value: Option<f64>,
) {
    registry.set_gauge(known_metric, &[], if value.is_some() { 1.0 } else { 0.0 });
    registry.set_gauge(value_metric, &[], value.unwrap_or(0.0));
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_optional_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn env_optional_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> CapacityInput {
        CapacityInput {
            pod_count: 2,
            run_slots_per_pod: 20,
            tool_slots_per_pod: 8,
            endpoint_rpc_slots_per_pod: 64,
            provider_rpm_budget: Some(1_000),
            provider_tpm_budget: None,
            avg_tokens_per_llm_request: None,
            avg_llm_requests_per_active_run_per_minute: Some(5.0),
            control_poll_interval_ms: 500,
            max_control_poll_qps: Some(200.0),
            target_utilization: 0.8,
        }
    }

    #[test]
    fn capacity_equation_multiplies_per_pod_limits() {
        let plan = CapacityInput {
            pod_count: 3,
            run_slots_per_pod: 50,
            tool_slots_per_pod: 10,
            endpoint_rpc_slots_per_pod: 128,
            control_poll_interval_ms: 500,
            ..input()
        }
        .evaluate();

        assert_eq!(plan.total_run_slots, 150);
        assert_eq!(plan.total_tool_slots, 30);
        assert_eq!(plan.total_endpoint_rpc_slots, 384);
        assert_eq!(plan.estimated_control_poll_qps, 300.0);
    }

    #[test]
    fn provider_tpm_budget_constrains_effective_rpm() {
        let plan = CapacityInput {
            provider_rpm_budget: Some(1_000),
            provider_tpm_budget: Some(120_000),
            avg_tokens_per_llm_request: Some(200),
            ..input()
        }
        .evaluate();

        assert_eq!(plan.effective_provider_rpm, Some(600.0));
    }

    #[test]
    fn rollout_gate_fails_closed_without_provider_evidence() {
        let plan = CapacityInput {
            provider_rpm_budget: None,
            provider_tpm_budget: None,
            avg_llm_requests_per_active_run_per_minute: None,
            ..input()
        }
        .evaluate();

        assert!(!plan.decision.allowed);
        assert_eq!(
            plan.decision.risks,
            vec![RolloutRisk::ProviderEvidenceMissing]
        );
    }

    #[test]
    fn rollout_gate_rejects_provider_oversubscription() {
        let plan = CapacityInput {
            pod_count: 6,
            run_slots_per_pod: 80,
            provider_rpm_budget: Some(1_000),
            avg_llm_requests_per_active_run_per_minute: Some(3.0),
            max_control_poll_qps: None,
            ..input()
        }
        .evaluate();

        assert_eq!(plan.estimated_provider_rpm_demand, Some(1_440.0));
        assert!(!plan.decision.allowed);
        assert_eq!(
            plan.decision.risks,
            vec![RolloutRisk::ProviderOversubscribed]
        );
    }

    #[test]
    fn rollout_gate_allows_when_provider_and_control_budgets_cover_demand() {
        let plan = input().evaluate();

        assert_eq!(plan.estimated_provider_rpm_demand, Some(200.0));
        assert_eq!(plan.estimated_control_poll_qps, 80.0);
        assert!(plan.decision.allowed);
        assert!(plan.decision.risks.is_empty());
    }

    #[test]
    fn limit_semantics_separate_wait_reject_and_breaker_modes() {
        let run = LIMIT_SEMANTICS
            .iter()
            .find(|limit| limit.name == "run_admission")
            .expect("run limit semantics");
        let endpoint = LIMIT_SEMANTICS
            .iter()
            .find(|limit| limit.name == "registered_endpoint_rpc")
            .expect("endpoint limit semantics");
        let provider = LIMIT_SEMANTICS
            .iter()
            .find(|limit| limit.name == "provider_rate_limit")
            .expect("provider limit semantics");

        assert_eq!(run.mode, LimitMode::WaitThenReject);
        assert_eq!(endpoint.mode, LimitMode::Reject);
        assert_eq!(provider.mode, LimitMode::CircuitBreaker);
    }

    #[test]
    fn metrics_scrape_exports_capacity_gate_and_limit_modes() {
        let registry = MetricsRegistry::new();
        let plan = CapacityInput {
            provider_rpm_budget: None,
            avg_llm_requests_per_active_run_per_minute: None,
            ..input()
        }
        .evaluate();

        scrape_capacity_metrics(&registry, &plan);
        let rendered = registry.render_prometheus();

        assert!(rendered.contains("astra_capacity_run_slots_total 40"));
        assert!(rendered.contains("astra_capacity_rollout_allowed 0"));
        assert!(
            rendered.contains("astra_capacity_rollout_risk{risk=\"provider_evidence_missing\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_capacity_limit_mode{env_var=\"ASTRA_ENDPOINT_RPC_CONCURRENCY\",limit=\"registered_endpoint_rpc\",mode=\"reject\",scope=\"per_endpoint_per_pod\"} 1"
            ),
            "{rendered}"
        );
    }
}
