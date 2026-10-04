use serde_json::{Value, json};

use crate::RuntimeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernedConfigPath {
    CompressionThreshold,
    RetrievalTopK,
}

impl GovernedConfigPath {
    pub const ALL: [Self; 2] = [Self::CompressionThreshold, Self::RetrievalTopK];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompressionThreshold => "compression.compression_threshold",
            Self::RetrievalTopK => "memory.retrieval_top_k",
        }
    }

    const fn limits(self) -> (f64, f64, bool) {
        match self {
            Self::CompressionThreshold => (0.5, 0.98, false),
            Self::RetrievalTopK => (1.0, 20.0, true),
        }
    }
}

impl TryFrom<&str> for GovernedConfigPath {
    type Error = ();

    fn try_from(path: &str) -> Result<Self, Self::Error> {
        match path {
            "compression.compression_threshold" => Ok(Self::CompressionThreshold),
            "memory.retrieval_top_k" => Ok(Self::RetrievalTopK),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GovernedConfigValueType {
    Integer,
    Number,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GovernedConfigMutationError {
    UnsupportedPath {
        path: String,
    },
    InvalidType {
        path: &'static str,
        expected: GovernedConfigValueType,
    },
    OutOfRange {
        path: &'static str,
        min: f64,
        max: f64,
    },
    DriftCeilingExceeded {
        path: &'static str,
        old: Value,
        new: Value,
        drift: f64,
        ceiling: f64,
    },
    InvalidCompressionBounds {
        compression_threshold: f64,
    },
}

impl GovernedConfigMutationError {
    pub fn to_json(&self) -> Value {
        match self {
            Self::UnsupportedPath { path } => json!({
                "error": "Unsupported config path",
                "path": path,
                "supported_paths": GovernedConfigPath::ALL.map(GovernedConfigPath::as_str),
            }),
            Self::InvalidType { expected, .. } => match expected {
                GovernedConfigValueType::Integer => json!({"error": "value must be an integer"}),
                GovernedConfigValueType::Number => json!({"error": "value must be a number"}),
            },
            Self::OutOfRange { path, min, max } => json!({
                "error": format!("{path} must be within [{min}, {max}]"),
            }),
            Self::DriftCeilingExceeded {
                path,
                old,
                new,
                drift,
                ceiling,
            } => json!({
                "error": "config_drift_ceiling_exceeded",
                "path": path,
                "old": old,
                "new": new,
                "drift": drift,
                "ceiling": ceiling,
            }),
            Self::InvalidCompressionBounds {
                compression_threshold,
            } => json!({
                "error": "invalid_runtime_config",
                "invariant": "compression_bounds",
                "compression_threshold": compression_threshold,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernedConfigInvariantValidation {
    pub compression_bounds: bool,
}

impl GovernedConfigInvariantValidation {
    pub const fn is_valid(self) -> bool {
        self.compression_bounds
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GovernedConfigMutation {
    pub path: GovernedConfigPath,
    pub old_value: Value,
    pub new_value: Value,
    pub drift: Option<f64>,
}

pub fn normalized_config_drift(old: f64, new: f64) -> Option<f64> {
    if !old.is_finite() || !new.is_finite() {
        return None;
    }
    let denominator = old.abs().max(new.abs());
    if denominator < f64::EPSILON {
        return Some(0.0);
    }
    Some((new - old).abs() / denominator)
}

pub fn governed_config_invariant_validation(
    config: &RuntimeConfig,
) -> GovernedConfigInvariantValidation {
    GovernedConfigInvariantValidation {
        compression_bounds: (0.0..=1.0).contains(&config.compression.compression_threshold),
    }
}

/// Validate cross-field invariants required by every governed durable writer.
pub fn validate_governed_config_candidate(
    config: &RuntimeConfig,
) -> Result<(), GovernedConfigMutationError> {
    let validation = governed_config_invariant_validation(config);
    if !validation.compression_bounds {
        return Err(GovernedConfigMutationError::InvalidCompressionBounds {
            compression_threshold: config.compression.compression_threshold,
        });
    }
    Ok(())
}

pub fn apply_governed_config_mutation(
    config: &mut RuntimeConfig,
    path: &str,
    value: &Value,
    force: bool,
    drift_ceiling: f64,
) -> Result<GovernedConfigMutation, GovernedConfigMutationError> {
    let mut candidate = config.clone();
    let field = GovernedConfigPath::try_from(path).map_err(|()| {
        GovernedConfigMutationError::UnsupportedPath {
            path: path.to_string(),
        }
    })?;
    let (min, max, integer) = field.limits();
    let new_number = if integer {
        value
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .map(f64::from)
    } else {
        value.as_f64()
    }
    .ok_or(GovernedConfigMutationError::InvalidType {
        path: field.as_str(),
        expected: if integer {
            GovernedConfigValueType::Integer
        } else {
            GovernedConfigValueType::Number
        },
    })?;
    if !(min..=max).contains(&new_number) {
        return Err(GovernedConfigMutationError::OutOfRange {
            path: field.as_str(),
            min,
            max,
        });
    }

    let (old_number, old_value, new_value) = match field {
        GovernedConfigPath::CompressionThreshold => {
            let old = candidate.compression.compression_threshold;
            (old, json!(old), json!(new_number))
        }
        GovernedConfigPath::RetrievalTopK => {
            let old = candidate.memory.retrieval_top_k;
            (f64::from(old), json!(old), json!(new_number as u32))
        }
    };
    let drift = normalized_config_drift(old_number, new_number);
    if let Some(drift_value) = drift
        && !force
        && drift_value > drift_ceiling
    {
        return Err(GovernedConfigMutationError::DriftCeilingExceeded {
            path: field.as_str(),
            old: old_value,
            new: new_value,
            drift: drift_value,
            ceiling: drift_ceiling,
        });
    }
    match field {
        GovernedConfigPath::CompressionThreshold => {
            candidate.compression.compression_threshold = new_number;
        }
        GovernedConfigPath::RetrievalTopK => {
            candidate.memory.retrieval_top_k = new_number as u32;
        }
    }
    validate_governed_config_candidate(&candidate)?;
    *config = candidate;
    Ok(GovernedConfigMutation {
        path: field,
        old_value,
        new_value,
        drift,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_governed_paths_share_typed_boundaries_and_drift() {
        let cases = [
            (GovernedConfigPath::CompressionThreshold, 0.5, 0.98, false),
            (GovernedConfigPath::RetrievalTopK, 1.0, 20.0, true),
        ];
        for (path, min, max, integer) in cases {
            for boundary in [min, max] {
                let value = if integer {
                    json!(boundary as u32)
                } else {
                    json!(boundary)
                };
                let mut config = RuntimeConfig::default();
                let applied =
                    apply_governed_config_mutation(&mut config, path.as_str(), &value, true, 0.3)
                        .unwrap();
                assert_eq!(applied.path, path);
                assert!(applied.drift.is_some());
            }
            let above_max = if integer {
                json!((max as u64) + 1)
            } else {
                json!(max + 0.01)
            };
            assert!(matches!(
                apply_governed_config_mutation(
                    &mut RuntimeConfig::default(),
                    path.as_str(),
                    &above_max,
                    true,
                    0.3,
                ),
                Err(GovernedConfigMutationError::OutOfRange { .. })
            ));
            assert!(matches!(
                apply_governed_config_mutation(
                    &mut RuntimeConfig::default(),
                    path.as_str(),
                    &Value::String("invalid".into()),
                    true,
                    0.3,
                ),
                Err(GovernedConfigMutationError::InvalidType { .. })
            ));
            let mut drift_config = RuntimeConfig::default();
            let old = crate::read_existing_json_path(
                &serde_json::to_value(&drift_config).unwrap(),
                path.as_str(),
            )
            .unwrap()
            .as_f64()
            .unwrap();
            let changed_boundary = if (old - min).abs() > f64::EPSILON {
                min
            } else {
                max
            };
            let drift_value = if integer {
                json!(changed_boundary as u32)
            } else {
                json!(changed_boundary)
            };
            assert!(matches!(
                apply_governed_config_mutation(
                    &mut drift_config,
                    path.as_str(),
                    &drift_value,
                    false,
                    0.0,
                ),
                Err(GovernedConfigMutationError::DriftCeilingExceeded { .. })
            ));
        }
    }

    #[test]
    fn compression_invariant_failure_leaves_candidate_unchanged() {
        let mut config = RuntimeConfig::default();
        config.compression.compression_threshold = 1.1;
        let before = config.memory.retrieval_top_k;
        let changed = (1..=20).find(|value| *value != before).unwrap();

        assert!(matches!(
            apply_governed_config_mutation(
                &mut config,
                GovernedConfigPath::RetrievalTopK.as_str(),
                &json!(changed),
                true,
                0.3,
            ),
            Err(GovernedConfigMutationError::InvalidCompressionBounds { .. })
        ));
        assert_eq!(config.memory.retrieval_top_k, before);
    }
}
