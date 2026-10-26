//! Process-local byte/token-weighted multi-tenant admission.
//!
//! Billing quotas remain separate. This controller protects resident memory,
//! provider concurrency, CPU-heavy serialization/compaction, and canonical
//! I/O using deterministic work estimates. Each server owns its capacity;
//! there is no SQL reservation, renewal, or cluster-wide exact quota.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionWork {
    pub resident_bytes: u64,
    pub context_tokens: u64,
    pub provider_slots: u32,
    pub cpu_units: u64,
    pub io_bytes: u64,
}

impl AdmissionWork {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            resident_bytes: self.resident_bytes.checked_add(other.resident_bytes)?,
            context_tokens: self.context_tokens.checked_add(other.context_tokens)?,
            provider_slots: self.provider_slots.checked_add(other.provider_slots)?,
            cpu_units: self.cpu_units.checked_add(other.cpu_units)?,
            io_bytes: self.io_bytes.checked_add(other.io_bytes)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            resident_bytes: self.resident_bytes.checked_sub(other.resident_bytes)?,
            context_tokens: self.context_tokens.checked_sub(other.context_tokens)?,
            provider_slots: self.provider_slots.checked_sub(other.provider_slots)?,
            cpu_units: self.cpu_units.checked_sub(other.cpu_units)?,
            io_bytes: self.io_bytes.checked_sub(other.io_bytes)?,
        })
    }

    pub fn fits_within(self, capacity: Self) -> bool {
        self.resident_bytes <= capacity.resident_bytes
            && self.context_tokens <= capacity.context_tokens
            && self.provider_slots <= capacity.provider_slots
            && self.cpu_units <= capacity.cpu_units
            && self.io_bytes <= capacity.io_bytes
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeightedAdmissionLimits {
    pub global: AdmissionWork,
    /// Hard burst ceiling for one owner. Keeping this below `global` reserves
    /// capacity for another tenant even when the first tenant is noisy.
    pub per_owner: AdmissionWork,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum WeightedAdmissionError {
    #[error("requested work exceeds the global admission capacity")]
    RequestExceedsGlobal,
    #[error("requested work exceeds the per-owner admission capacity")]
    RequestExceedsOwner,
    #[error("global weighted admission capacity is currently exhausted")]
    GlobalExhausted,
    #[error("owner weighted admission share is currently exhausted")]
    OwnerExhausted,
}

#[derive(Default)]
struct AdmissionState {
    global_used: AdmissionWork,
    owner_used: HashMap<String, AdmissionWork>,
}

struct WeightedAdmissionInner {
    limits: WeightedAdmissionLimits,
    state: Mutex<AdmissionState>,
}

#[derive(Clone)]
pub struct WeightedAdmissionController {
    inner: Arc<WeightedAdmissionInner>,
}

impl WeightedAdmissionController {
    pub fn new(limits: WeightedAdmissionLimits) -> Result<Self, WeightedAdmissionError> {
        if !limits.per_owner.fits_within(limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        Ok(Self {
            inner: Arc::new(WeightedAdmissionInner {
                limits,
                state: Mutex::new(AdmissionState::default()),
            }),
        })
    }

    pub fn try_admit(
        &self,
        owner_user_id: impl Into<String>,
        work: AdmissionWork,
    ) -> Result<WeightedAdmissionPermit, WeightedAdmissionError> {
        if !work.fits_within(self.inner.limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        if !work.fits_within(self.inner.limits.per_owner) {
            return Err(WeightedAdmissionError::RequestExceedsOwner);
        }
        let owner_user_id = owner_user_id.into();
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let global_next = state
            .global_used
            .checked_add(work)
            .ok_or(WeightedAdmissionError::GlobalExhausted)?;
        if !global_next.fits_within(self.inner.limits.global) {
            return Err(WeightedAdmissionError::GlobalExhausted);
        }
        let owner_next = state
            .owner_used
            .get(&owner_user_id)
            .copied()
            .unwrap_or_default()
            .checked_add(work)
            .ok_or(WeightedAdmissionError::OwnerExhausted)?;
        if !owner_next.fits_within(self.inner.limits.per_owner) {
            return Err(WeightedAdmissionError::OwnerExhausted);
        }
        state.global_used = global_next;
        state.owner_used.insert(owner_user_id.clone(), owner_next);
        Ok(WeightedAdmissionPermit {
            inner: Arc::clone(&self.inner),
            owner_user_id,
            work,
        })
    }

    pub fn usage(&self, owner_user_id: &str) -> (AdmissionWork, AdmissionWork) {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            state.global_used,
            state
                .owner_used
                .get(owner_user_id)
                .copied()
                .unwrap_or_default(),
        )
    }
}

pub struct WeightedAdmissionPermit {
    inner: Arc<WeightedAdmissionInner>,
    owner_user_id: String,
    work: AdmissionWork,
}

impl Drop for WeightedAdmissionPermit {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.global_used = state
            .global_used
            .checked_sub(self.work)
            .expect("admission permit cannot release more than it acquired");
        let remove_owner = {
            let owner = state
                .owner_used
                .get_mut(&self.owner_user_id)
                .expect("admitted owner usage must exist");
            *owner = owner
                .checked_sub(self.work)
                .expect("owner permit cannot release more than it acquired");
            *owner == AdmissionWork::default()
        };
        if remove_owner {
            state.owner_used.remove(&self.owner_user_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> WeightedAdmissionLimits {
        WeightedAdmissionLimits {
            global: AdmissionWork {
                resident_bytes: 1_000,
                context_tokens: 1_000,
                provider_slots: 10,
                cpu_units: 1_000,
                io_bytes: 1_000,
            },
            per_owner: AdmissionWork {
                resident_bytes: 750,
                context_tokens: 750,
                provider_slots: 8,
                cpu_units: 750,
                io_bytes: 750,
            },
        }
    }

    #[test]
    fn noisy_owner_cannot_consume_another_owners_reserved_share() {
        let controller = WeightedAdmissionController::new(limits()).unwrap();
        let noisy = controller
            .try_admit(
                "owner-a",
                AdmissionWork {
                    resident_bytes: 750,
                    context_tokens: 750,
                    provider_slots: 8,
                    cpu_units: 750,
                    io_bytes: 750,
                },
            )
            .unwrap();
        assert!(matches!(
            controller.try_admit(
                "owner-a",
                AdmissionWork {
                    resident_bytes: 1,
                    context_tokens: 1,
                    provider_slots: 1,
                    cpu_units: 1,
                    io_bytes: 1,
                }
            ),
            Err(WeightedAdmissionError::OwnerExhausted)
        ));
        let other = controller
            .try_admit(
                "owner-b",
                AdmissionWork {
                    resident_bytes: 250,
                    context_tokens: 250,
                    provider_slots: 2,
                    cpu_units: 250,
                    io_bytes: 250,
                },
            )
            .expect("another owner retains its configured share");
        drop((noisy, other));
        assert_eq!(
            controller.usage("owner-a"),
            (AdmissionWork::default(), AdmissionWork::default())
        );
    }

    #[test]
    fn large_context_consumes_proportional_units() {
        let controller = WeightedAdmissionController::new(limits()).unwrap();
        let _small = controller
            .try_admit(
                "owner-a",
                AdmissionWork {
                    context_tokens: 100,
                    resident_bytes: 100,
                    provider_slots: 1,
                    cpu_units: 100,
                    io_bytes: 100,
                },
            )
            .unwrap();
        assert!(matches!(
            controller.try_admit(
                "owner-a",
                AdmissionWork {
                    context_tokens: 700,
                    resident_bytes: 700,
                    provider_slots: 1,
                    cpu_units: 700,
                    io_bytes: 700,
                }
            ),
            Err(WeightedAdmissionError::OwnerExhausted)
        ));
    }
}
