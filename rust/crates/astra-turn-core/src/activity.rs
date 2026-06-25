use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionActivityUpdatePlan {
    pub last_event_id: Option<String>,
}
