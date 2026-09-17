//! One OpenAI-chat thinking wire contract for probes and inference.
//! Unknown protocol is not evidence that reasoning is disabled.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingProtocol {
    #[default]
    Unknown,
    EnableThinking,
    ThinkingObject,
    ReasoningEffort,
    /// Moonshot's binary toggle, without an additional effort field. Sampling
    /// capability is independent of the toggle and is not inferred here.
    Moonshot,
}

impl ThinkingProtocol {
    pub const REVISION: u32 = 1;

    pub fn can_disable(self) -> bool {
        matches!(
            self,
            Self::EnableThinking | Self::ThinkingObject | Self::Moonshot
        )
    }

    /// Apply known protocols *after* generic overrides. Unknown preserves the
    /// existing assembly contract. `effort` is canonical low/medium/high/max.
    pub fn apply(self, body: &mut Value, enabled: bool, effort: Option<&str>) {
        // Unknown is absence of adapter knowledge, not authority to erase
        // established provider behavior or explicit administrator overrides.
        if self == Self::Unknown {
            return;
        }
        let Some(object) = body.as_object_mut() else {
            return;
        };
        for key in [
            "thinking",
            "enable_thinking",
            "reasoning_effort",
            "reasoning",
        ] {
            object.remove(key);
        }
        if let Some(config) = object
            .get_mut("output_config")
            .and_then(Value::as_object_mut)
        {
            config.remove("effort");
            config.remove("reasoning_effort");
        }
        match self {
            Self::Unknown => unreachable!("unknown protocols preserve the body above"),
            Self::EnableThinking => {
                body["enable_thinking"] = json!(enabled);
            }
            Self::ThinkingObject | Self::Moonshot => {
                body["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
                if self == Self::ThinkingObject
                    && enabled
                    && let Some(effort) = effort
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            Self::ReasoningEffort if enabled => {
                if let Some(effort) = effort {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            Self::ReasoningEffort => {}
        }
    }
}

/// Transitional zero-temperature defaults apply only to maintained native
/// endpoint contracts, never a provider label attached to an arbitrary gateway.
/// Other deployments (including Bedrock gateways) need an explicit capability.
pub fn canonical_zero_temperature(provider: &str, base_url: &str) -> bool {
    let expected = match provider {
        "openai" => "api.openai.com",
        "anthropic" => "api.anthropic.com",
        "deepseek" => "api.deepseek.com",
        _ => return false,
    };
    // Only OpenAI has an implicit endpoint in the summary transport.
    if base_url.is_empty() {
        return provider == "openai";
    }
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some(expected)
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path().trim_end_matches('/'), "" | "/v1")
}

/// Maintained adapter defaults, never substring matching arbitrary URLs.
/// Explicit per-Offering protocol configuration takes precedence at admission.
pub fn canonical_thinking_protocol(
    provider: &str,
    base_url: &str,
    model: &str,
) -> ThinkingProtocol {
    let url = reqwest::Url::parse(base_url).ok();
    let host = url.as_ref().and_then(|url| url.host_str()).unwrap_or("");
    let host = host.trim_end_matches('.');
    let provider = provider.trim().to_ascii_lowercase();
    if matches!(provider.as_str(), "anthropic" | "bedrock") {
        return ThinkingProtocol::Unknown; // Those adapters own their native envelopes.
    }
    if provider == "deepseek"
        || provider.starts_with("deepseek-")
        || host == "api.deepseek.com"
        || host.ends_with(".deepseek.com")
    {
        return ThinkingProtocol::ThinkingObject;
    }
    if is_dashscope_provider(&provider)
        || host == "dashscope.aliyuncs.com"
        || host.ends_with(".dashscope.aliyuncs.com")
        || host == "dashscope-intl.aliyuncs.com"
        || host == "dashscope-us.aliyuncs.com"
    {
        return ThinkingProtocol::EnableThinking;
    }
    // https://platform.kimi.com/docs/guide/kimi-k2-6-quickstart
    // Do not infer protocol from a Kimi model name on an unrelated gateway.
    if matches!(host, "api.moonshot.cn" | "api.moonshot.ai")
        && url.as_ref().is_some_and(|u| u.scheme() == "https" && u.port_or_known_default() == Some(443) && u.path().trim_end_matches('/') == "/v1")
        // K3 toggle is covered by the opt-in real-provider contract, rather
        // than extrapolated from its name (verified 2026-09-09).
        && matches!(model, "kimi-k2.5" | "kimi-k2.6" | "kimi-k3")
    {
        return ThinkingProtocol::Moonshot;
    }
    if provider == "openai" && (host == "api.openai.com" || base_url.is_empty()) {
        return ThinkingProtocol::ReasoningEffort;
    }
    ThinkingProtocol::Unknown
}

/// Preserve the established provider-alias contract in one owner.
pub fn is_dashscope_provider(provider: &str) -> bool {
    let provider = provider.to_ascii_lowercase();
    provider.contains("dashscope") || provider.contains("aliyun") || provider.contains("alibaba")
}

/// Shared main/auxiliary Offering temperature configuration. Protocol-specific
/// adapters still own native restrictions; a thinking toggle is not a sampling
/// capability and must not invent one.
pub fn configured_temperature(
    overrides: Option<&serde_json::Map<String, Value>>,
    fixed: Option<f64>,
) -> Result<Option<f64>, String> {
    let configured = overrides
        .and_then(|o| o.get("temperature"))
        .map(|value| {
            value.as_f64().ok_or_else(|| {
                "request_body_overrides.temperature must be a finite non-negative number"
                    .to_string()
            })
        })
        .transpose()?;
    for (source, value) in [
        ("request_body_overrides.temperature", configured),
        ("fixed_temperature", fixed),
    ] {
        if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
            return Err(format!("{source} must be a finite non-negative number"));
        }
    }
    match (configured, fixed) {
        (Some(configured), Some(fixed)) if configured != fixed => Err(format!(
            "request_body_overrides.temperature ({configured}) conflicts with fixed_temperature ({fixed})"
        )),
        (Some(value), _) => Ok(Some(value)),
        (None, fixed) => Ok(fixed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_temperature_requires_native_endpoint_authority() {
        assert!(canonical_zero_temperature(
            "openai",
            "https://api.openai.com/v1"
        ));
        for url in [
            "https://api.openai.com.evil.test/v1",
            "http://api.openai.com/v1",
            "https://api.openai.com:8443/v1",
            "https://api.openai.com/proxy",
            "https://api.openai.com/v1?gateway=1",
            "https://user@api.openai.com/v1",
            "https://api.moonshot.cn/v1",
        ] {
            assert!(!canonical_zero_temperature("openai", url), "{url}");
        }
    }

    #[test]
    fn toggle_preserves_explicit_sampling_in_both_modes() {
        for enabled in [false, true] {
            let mut body = json!({"temperature":0.7,"top_p":0.9,"presence_penalty":0.1,"frequency_penalty":0.2});
            let original = body.clone();
            ThinkingProtocol::Moonshot.apply(&mut body, enabled, Some("low"));
            for key in [
                "temperature",
                "top_p",
                "presence_penalty",
                "frequency_penalty",
            ] {
                assert_eq!(body[key], original[key]);
            }
            assert!(body.get("reasoning_effort").is_none());
        }
    }

    #[test]
    fn unknown_preserves_existing_controls_and_dashscope_aliases_agree() {
        let mut body = json!({"reasoning_effort":"high","enable_thinking":false,"thinking":{"type":"enabled"}});
        let original = body.clone();
        ThinkingProtocol::Unknown.apply(&mut body, true, Some("low"));
        assert_eq!(body, original);
        for provider in ["dashscope-intl", "alibaba-cloud", "aliyun"] {
            assert_eq!(
                canonical_thinking_protocol(provider, "https://gateway.example/v1", "m"),
                ThinkingProtocol::EnableThinking
            );
        }
    }

    #[test]
    fn shared_temperature_checks_explicit_conflicts_only() {
        assert_eq!(configured_temperature(None, Some(0.6)).unwrap(), Some(0.6));
        let values = json!({"temperature":0.7});
        assert_eq!(
            configured_temperature(values.as_object(), None).unwrap(),
            Some(0.7)
        );
        assert!(configured_temperature(values.as_object(), Some(0.6)).is_err());
        assert!(configured_temperature(None, Some(f64::NAN)).is_err());
    }

    #[test]
    fn canonical_contract_rejects_gateway_name_and_url_spoofing() {
        for url in [
            "https://api.moonshot.cn.evil.test/v1",
            "https://api.moonshot.cn@evil.test/v1",
            "https://evil.test/api.moonshot.cn/v1",
            "https://api.moonshot.cn:8443/v1",
            "https://api.moonshot.cn/proxy",
        ] {
            assert_eq!(
                canonical_thinking_protocol("openai-compatible", url, "kimi-k2.6"),
                ThinkingProtocol::Unknown
            );
        }
        assert_eq!(
            canonical_thinking_protocol(
                "openai-compatible",
                "https://api.moonshot.cn/v1",
                "unknown"
            ),
            ThinkingProtocol::Unknown
        );
        assert_eq!(
            canonical_thinking_protocol(
                "openai-compatible",
                "https://api.moonshot.cn/v1",
                "kimi-k2.6"
            ),
            ThinkingProtocol::Moonshot
        );
    }

    #[test]
    fn controls_are_exclusive_and_unknown_emits_nothing() {
        for protocol in [
            ThinkingProtocol::EnableThinking,
            ThinkingProtocol::ThinkingObject,
            ThinkingProtocol::Moonshot,
            ThinkingProtocol::ReasoningEffort,
        ] {
            let mut body = json!({"thinking": {}, "enable_thinking": true, "reasoning_effort": "max", "reasoning": {"effort":"high"}, "output_config":{"effort":"high", "reasoning_effort":"high", "format":"json"}, "temperature": 0});
            protocol.apply(&mut body, false, None);
            assert!(body.get("reasoning").is_none());
            assert!(body["output_config"].get("effort").is_none());
            assert!(body["output_config"].get("reasoning_effort").is_none());
            assert_eq!(body["output_config"]["format"], "json");
            assert!(body.get("reasoning_effort").is_none());
            assert_eq!(
                body.get("enable_thinking").is_some(),
                protocol == ThinkingProtocol::EnableThinking
            );
            assert_eq!(
                body.get("thinking").is_some(),
                matches!(
                    protocol,
                    ThinkingProtocol::ThinkingObject | ThinkingProtocol::Moonshot
                )
            );
            if protocol == ThinkingProtocol::Moonshot {
                assert_eq!(body["temperature"], 0);
            }
        }
    }
}
