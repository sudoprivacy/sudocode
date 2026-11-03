//! Bounded, plain-text user advice. Producers choose urgency; renderers own style.
use serde::{Deserialize, Serialize};

pub const MAX_TIP_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipPriority {
    Normal,
    Important,
    Urgent,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tip {
    pub priority: TipPriority,
    pub message: String,
}

impl Tip {
    pub fn validate(&self) -> Result<(), String> {
        if self.message.chars().count() > MAX_TIP_CHARS {
            return Err(format!(
                "message must be at most {MAX_TIP_CHARS} characters"
            ));
        }
        if self.message.chars().any(char::is_control) {
            return Err(
                "message must be a single plain-text line without control characters".into(),
            );
        }
        Ok(())
    }
}
