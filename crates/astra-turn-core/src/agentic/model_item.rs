//! Identity of a physical model attempt across stream delivery and replay.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Start a fresh item before dispatch. The ID remains opaque on the wire and
/// contains the runtime clock of this attempt, including across retries.
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

/// The runtime's model item start time. Earlier random IDs have no clock.
pub fn started_at(model_item_id: &str) -> Option<DateTime<Utc>> {
    let id = Uuid::parse_str(model_item_id).ok()?;
    if id.get_version_num() != 7 {
        return None;
    }
    let (seconds, nanos) = id.get_timestamp()?.to_unix();
    DateTime::from_timestamp(seconds.try_into().ok()?, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_time_ordered_model_items_have_a_start_time() {
        let id = "019a93c5-d340-7000-8000-000000000001";
        let expected = DateTime::from_timestamp_millis(0x019a93c5d340).unwrap();
        assert_eq!(started_at(id), Some(expected));
        assert_eq!(started_at("019a93c5-d340-4000-8000-000000000001"), None);
        assert_eq!(started_at("legacy-answer"), None);
        let first = new_id();
        let retry = new_id();
        assert_ne!(first, retry);
        assert!(started_at(&first).is_some());
        assert!(started_at(&retry).is_some());
    }
}
