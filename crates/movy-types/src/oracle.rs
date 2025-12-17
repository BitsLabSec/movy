use std::fmt::Display;

use serde::{Deserialize, Serialize};

use crate::input::MoveStructTag;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Discussion,
    Informational,
    Minor,
    Medium,
    Major,
    Critical,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OracleFinding {
    pub oracle: String,
    pub severity: Severity,
    pub extra: serde_json::Value,
}

impl Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let severity_str = match self {
            Severity::Discussion => "Discussion",
            Severity::Informational => "Informational",
            Severity::Minor => "Minor",
            Severity::Medium => "Medium",
            Severity::Major => "Major",
            Severity::Critical => "Critical",
        };
        write!(f, "{}", severity_str)
    }
}

impl Display for OracleFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OracleFinding {{ oracle: {}, severity: {}, extra: {} }}",
            self.oracle, self.severity, self.extra
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub ty: MoveStructTag,
    pub contents: Vec<u8>,
}

impl From<sui_types::event::Event> for Event {
    fn from(value: sui_types::event::Event) -> Self {
        Self {
            ty: value.type_.into(),
            contents: value.contents,
        }
    }
}
