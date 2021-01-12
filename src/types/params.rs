use crate::types::Value;
use serde::{Deserialize, Serialize};
use serde_json::Map as JsonMap;

/// Request parameters
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Params {
    /// No parameters
    None,
    /// Array of values
    Array(Vec<Value>),
    /// Map of values
    Map(JsonMap<String, Value>),
}
