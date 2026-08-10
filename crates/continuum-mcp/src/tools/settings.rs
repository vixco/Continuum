//! Typed settings request schemas for `mcp__continuum__settings_*`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct SettingsListRequest {
    /// Optional dotted-path keyword, such as `screen`, `voice`, `privacy`, or `chat`.
    pub query: Option<String>,
    /// Maximum matches (default 80, maximum 250).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SettingsGetRequest {
    /// Exact dotted path returned by `settings_list`.
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SettingsSetRequest {
    /// Exact dotted path returned by `settings_list` or `settings_get`.
    pub path: String,
    /// New JSON value. It must match the typed config field.
    pub value: serde_json::Value,
}
