//! Session auto-reset policies.
//!
//! Controls when a chat session is automatically reset (cleared):
//! - Daily: at a configured hour (e.g. 4 AM)
//! - Idle: after N hours of no activity
//! - Both: whichever triggers first
//! - None: manual reset only

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResetPolicy {
    #[default]
    None,
    Daily {
        hour: u32,
    },
    Idle {
        hours: u32,
    },
    Both {
        daily_hour: u32,
        idle_hours: u32,
    },
}

impl ResetPolicy {
    /// Check if a session should be reset given its last activity time.
    pub fn should_reset(
        &self,
        last_active: chrono::DateTime<chrono::Utc>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        match self {
            Self::None => false,
            Self::Daily { hour } => crossed_daily_boundary(last_active, now, *hour),
            Self::Idle { hours } => {
                let idle_secs = (now - last_active).num_seconds();
                idle_secs > (*hours as i64) * 3600
            }
            Self::Both {
                daily_hour,
                idle_hours,
            } => {
                crossed_daily_boundary(last_active, now, *daily_hour)
                    || (now - last_active).num_seconds() > (*idle_hours as i64) * 3600
            }
        }
    }
}

fn crossed_daily_boundary(
    last_active: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    reset_hour: u32,
) -> bool {
    if last_active >= now {
        return false;
    }
    // Find the most recent reset boundary before `now`
    let today_reset = now.date_naive().and_hms_opt(reset_hour, 0, 0);
    let yesterday_reset = (now - chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(reset_hour, 0, 0);

    if let (Some(today), Some(yesterday)) = (today_reset, yesterday_reset) {
        let boundary = if now.naive_utc() >= today {
            today
        } else {
            yesterday
        };
        last_active.naive_utc() < boundary
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn none_never_resets() {
        let policy = ResetPolicy::None;
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        assert!(!policy.should_reset(last, now));
    }

    #[test]
    fn daily_resets_after_boundary() {
        let policy = ResetPolicy::Daily { hour: 4 };
        // Last active at 3 AM, now is 5 AM same day → crossed 4 AM boundary
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 3, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 5, 0, 0).unwrap();
        assert!(policy.should_reset(last, now));
    }

    #[test]
    fn daily_no_reset_before_boundary() {
        let policy = ResetPolicy::Daily { hour: 4 };
        // Last active at 5 AM, now is 10 AM same day → haven't crossed next 4 AM
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 5, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        assert!(!policy.should_reset(last, now));
    }

    #[test]
    fn daily_resets_next_day() {
        let policy = ResetPolicy::Daily { hour: 4 };
        // Last active at 10 PM day 1, now is 5 AM day 2 → crossed 4 AM boundary
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 22, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 5, 0, 0).unwrap();
        assert!(policy.should_reset(last, now));
    }

    #[test]
    fn idle_resets_after_timeout() {
        let policy = ResetPolicy::Idle { hours: 24 };
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 1, 0, 0).unwrap(); // 25 hours later
        assert!(policy.should_reset(last, now));
    }

    #[test]
    fn idle_no_reset_within_timeout() {
        let policy = ResetPolicy::Idle { hours: 24 };
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 23, 0, 0).unwrap(); // 23 hours later
        assert!(!policy.should_reset(last, now));
    }

    #[test]
    fn both_resets_on_daily() {
        let policy = ResetPolicy::Both {
            daily_hour: 4,
            idle_hours: 48,
        };
        // Last active yesterday, now is past 4 AM → daily triggers even though idle hasn't
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 5, 0, 0).unwrap(); // 19 hours, < 48h idle
        assert!(policy.should_reset(last, now));
    }

    #[test]
    fn both_resets_on_idle() {
        let policy = ResetPolicy::Both {
            daily_hour: 4,
            idle_hours: 2,
        };
        // Last active 3 hours ago, same day, haven't crossed 4 AM → idle triggers
        let last = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 13, 1, 0).unwrap();
        assert!(policy.should_reset(last, now));
    }

    #[test]
    fn serde_roundtrip() {
        // Verify we can serialize and parse back
        let policy = ResetPolicy::Idle { hours: 12 };
        let yaml = serde_yaml_ng::to_string(&policy).unwrap();
        let parsed: ResetPolicy = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn serde_daily() {
        let policy = ResetPolicy::Daily { hour: 4 };
        let yaml = serde_yaml_ng::to_string(&policy).unwrap();
        let parsed: ResetPolicy = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn serde_none() {
        let policy = ResetPolicy::None;
        let yaml = serde_yaml_ng::to_string(&policy).unwrap();
        let parsed: ResetPolicy = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn default_is_none() {
        assert_eq!(ResetPolicy::default(), ResetPolicy::None);
    }
}
