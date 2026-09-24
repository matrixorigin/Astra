//! Canonical persisted token evidence. Absence is unknown, never measured zero.
use serde_json::{Map, Value};

/// Validated disjoint token lanes shared by the runtime and journal writers.
/// `Some` of an empty value represents an observed but unavailable sample;
/// callers retain `None` for absence of an accounting sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalTokenUsage {
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    output_tokens: Option<i64>,
    input_column: Option<i64>,
    total_column: Option<i64>,
}

impl serde::Serialize for CanonicalTokenUsage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.to_json(), serializer)
    }
}

impl<'de> serde::Deserialize<'de> for CanonicalTokenUsage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <Value as serde::Deserialize>::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

impl CanonicalTokenUsage {
    /// Combine distinct attempts, never updates to the same attempt. Unknown
    /// lanes are absorbing: a later report cannot complete an earlier gap.
    pub fn checked_add(self, other: Self) -> Result<Self, String> {
        let add = |left: Option<u64>, right: Option<u64>| -> Result<Option<u64>, String> {
            left.zip(right)
                .map(|(left, right)| {
                    left.checked_add(right)
                        .ok_or_else(|| "token_usage aggregate overflow".to_owned())
                })
                .transpose()
        };
        Self::new(
            add(self.input_tokens(), other.input_tokens())?,
            add(self.cached_input_tokens(), other.cached_input_tokens())?,
            add(self.cache_creation_tokens(), other.cache_creation_tokens())?,
            add(self.output_tokens(), other.output_tokens())?,
        )
    }

    pub fn new(
        input: Option<u64>,
        cached: Option<u64>,
        creation: Option<u64>,
        output: Option<u64>,
    ) -> Result<Self, String> {
        let checked = |value: Option<u64>| {
            value
                .map(i64::try_from)
                .transpose()
                .map_err(|_| "token_usage exceeds i64::MAX".to_owned())
        };
        let input_tokens = checked(input)?;
        let cached_input_tokens = checked(cached)?;
        let cache_creation_tokens = checked(creation)?;
        let output_tokens = checked(output)?;
        // Unknown non-negative lanes cannot make an overflowing known subtotal
        // representable. Validate that lower bound even for partial samples.
        [
            input_tokens,
            cached_input_tokens,
            cache_creation_tokens,
            output_tokens,
        ]
        .into_iter()
        .flatten()
        .try_fold(0_i64, i64::checked_add)
        .ok_or("token_usage known subtotal overflow")?;
        let input_column = match (input, cached, creation) {
            (Some(input), Some(cached), Some(creation)) => Some(
                crate::NormalizedPromptCacheUsage::new(input, cached, creation)
                    .checked_total_input_tokens()
                    .and_then(|value| i64::try_from(value).ok())
                    .ok_or("token_usage input column overflow")?,
            ),
            _ => None,
        };
        let total_column = match (input_column, output_tokens) {
            (Some(input), Some(output)) => Some(
                input
                    .checked_add(output)
                    .ok_or("token_usage total column overflow")?,
            ),
            _ => None,
        };
        Ok(Self {
            input_tokens,
            cached_input_tokens,
            cache_creation_tokens,
            output_tokens,
            input_column,
            total_column,
        })
    }

    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("token_usage must be a canonical JSON object")?;
        let keys = [
            "input_tokens",
            "cached_input_tokens",
            "cache_creation_tokens",
            "output_tokens",
            "total_tokens",
        ];
        // An alternate dialect is not an unavailable canonical sample.
        if !object.is_empty() && !keys.iter().any(|key| object.contains_key(*key)) {
            return Err("token_usage has no canonical input_tokens/output_tokens fields".into());
        }
        let read = |key: &str| -> Result<Option<u64>, String> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                    format!("token_usage field `{key}` must be a non-negative integer")
                }),
            }
        };
        let usage = Self::new(
            read(keys[0])?,
            read(keys[1])?,
            read(keys[2])?,
            read(keys[3])?,
        )?;
        if let Some(total) = read(keys[4])? {
            let expected = usage.total_column.ok_or(
                "token_usage total_tokens requires complete input_tokens and output_tokens",
            )?;
            if u64::try_from(expected).ok() != Some(total) {
                return Err(format!(
                    "token_usage total_tokens mismatch: expected {expected}, got {total}"
                ));
            }
        }
        Ok(usage)
    }

    pub fn to_json(self) -> Value {
        let mut object = Map::new();
        for (key, value) in [
            ("input_tokens", self.input_tokens),
            ("cached_input_tokens", self.cached_input_tokens),
            ("cache_creation_tokens", self.cache_creation_tokens),
            ("output_tokens", self.output_tokens),
            ("total_tokens", self.total_column),
        ] {
            if let Some(value) = value {
                object.insert(key.into(), value.into());
            }
        }
        Value::Object(object)
    }

    pub fn input_column(self) -> Option<i64> {
        self.input_column
    }
    pub fn input_tokens(self) -> Option<u64> {
        self.input_tokens.map(|value| value as u64)
    }
    pub fn cached_input_tokens(self) -> Option<u64> {
        self.cached_input_tokens.map(|value| value as u64)
    }
    pub fn cache_creation_tokens(self) -> Option<u64> {
        self.cache_creation_tokens.map(|value| value as u64)
    }
    pub fn output_tokens(self) -> Option<u64> {
        self.output_tokens.map(|value| value as u64)
    }
    pub fn total_tokens(self) -> Option<u64> {
        self.total_column.map(|value| value as u64)
    }
    pub fn output_column(self) -> Option<i64> {
        self.output_tokens
    }
    pub fn total_column(self) -> Option<i64> {
        self.total_column
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn distinct_attempt_merge_keeps_unknown_absorbing_and_zero_known() {
        let partial = CanonicalTokenUsage::new(Some(10), None, Some(0), Some(2)).unwrap();
        let complete = CanonicalTokenUsage::new(Some(20), Some(5), Some(0), Some(3)).unwrap();
        let expected = CanonicalTokenUsage::new(Some(30), None, Some(0), Some(5)).unwrap();
        assert_eq!(partial.checked_add(complete).unwrap(), expected);
        assert_eq!(complete.checked_add(partial).unwrap(), expected);
        let unknown = CanonicalTokenUsage::new(None, None, None, None).unwrap();
        assert_eq!(unknown.checked_add(complete).unwrap(), unknown);
        let zero = CanonicalTokenUsage::new(Some(0), Some(0), Some(0), Some(0)).unwrap();
        assert_eq!(zero.checked_add(complete).unwrap(), complete);
        assert_eq!(zero.checked_add(zero).unwrap().total_tokens(), Some(0));
        let max =
            CanonicalTokenUsage::new(Some(i64::MAX as u64), Some(0), Some(0), Some(0)).unwrap();
        assert!(max.checked_add(complete).is_err());
        let output = CanonicalTokenUsage::new(Some(0), Some(0), Some(0), Some(1)).unwrap();
        assert!(max.checked_add(output).is_err());
    }

    #[test]
    fn partial_lanes_and_explicit_zero_remain_distinct() {
        for (raw, canonical, columns) in [
            (json!({}), json!({}), (None, None, None)),
            (
                json!({"input_tokens":null,"output_tokens":7,"total_tokens":null}),
                json!({"output_tokens":7}),
                (None, Some(7), None),
            ),
            (
                json!({"input_tokens":0,"cached_input_tokens":0,"cache_creation_tokens":0,"output_tokens":0}),
                json!({"input_tokens":0,"cached_input_tokens":0,"cache_creation_tokens":0,"output_tokens":0,"total_tokens":0}),
                (Some(0), Some(0), Some(0)),
            ),
        ] {
            let usage = CanonicalTokenUsage::from_json(&raw).unwrap();
            assert_eq!(usage.to_json(), canonical);
            assert_eq!(serde_json::to_value(usage).unwrap(), canonical);
            assert_eq!(
                serde_json::from_value::<CanonicalTokenUsage>(canonical.clone()).unwrap(),
                usage
            );
            assert_eq!(
                (
                    usage.input_column(),
                    usage.output_column(),
                    usage.total_column()
                ),
                columns
            );
            assert_eq!(
                CanonicalTokenUsage::from_json(&usage.to_json()).unwrap(),
                usage
            );
        }
    }

    #[test]
    fn complete_inputs_derive_totals_and_reject_contradictions() {
        let mut raw = json!({"input_tokens":10,"cached_input_tokens":4,"cache_creation_tokens":3,"output_tokens":5});
        let usage = CanonicalTokenUsage::from_json(&raw).unwrap();
        assert_eq!(usage.input_column(), Some(17));
        assert_eq!(usage.total_column(), Some(22));
        raw["total_tokens"] = json!(22);
        assert_eq!(CanonicalTokenUsage::from_json(&raw).unwrap(), usage);
        raw["total_tokens"] = json!(21);
        assert!(CanonicalTokenUsage::from_json(&raw).is_err());
        assert!(serde_json::from_value::<CanonicalTokenUsage>(raw.clone()).is_err());
        assert!(
            CanonicalTokenUsage::from_json(&json!({"output_tokens":5,"total_tokens":5})).is_err()
        );
        for value in [json!(-1), json!(1.5), json!("7"), json!(u64::MAX)] {
            assert!(
                serde_json::from_value::<CanonicalTokenUsage>(json!({"output_tokens":value}))
                    .is_err()
            );
            assert!(CanonicalTokenUsage::from_json(&json!({"output_tokens":value})).is_err());
        }
        assert!(CanonicalTokenUsage::new(Some(i64::MAX as u64), Some(1), Some(0), None).is_err());
        assert!(
            CanonicalTokenUsage::new(Some(i64::MAX as u64), Some(0), Some(0), Some(1)).is_err()
        );
        assert!(CanonicalTokenUsage::from_json(&json!({"prompt":7})).is_err());
    }

    #[test]
    fn partial_samples_validate_known_subtotal_without_inventing_totals() {
        let max = i64::MAX as u64;
        for lanes in [
            [Some(max), Some(1), None, None],
            [None, Some(max), Some(1), None],
            [None, None, Some(max), Some(1)],
            [Some(max), None, None, Some(1)],
        ] {
            assert!(CanonicalTokenUsage::new(lanes[0], lanes[1], lanes[2], lanes[3]).is_err());
        }
        let usage = CanonicalTokenUsage::new(Some(max - 1), None, None, Some(1)).unwrap();
        assert_eq!(usage.input_column(), None);
        assert_eq!(usage.total_column(), None);
        assert_eq!(usage.output_column(), Some(1));
    }
}
