// Shared types between CLI and Hub.
// Hub-side copy: https://github.com/flynnbody/corevisor (crates/common/src/lib.rs)
// Keep in sync when modifying wire-protocol types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub domain: String,
    pub catalog_id: Option<i64>,
    pub header_name: String,
}

// -- Credential entry (used in local store) --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialEntry {
    pub profile_id: String,
    pub domain: String,
    pub header_name: String,
    pub header_value: String,
}

// -- Approval system types --

/// Action a rule can specify. Defaults to `Allow`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    #[default]
    Allow,
    Deny,
    RequireApproval,
}

impl RuleAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::RequireApproval => "require_approval",
        }
    }
}

impl std::fmt::Display for RuleAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RuleAction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            "require_approval" => Ok(Self::RequireApproval),
            _ => Err(format!("invalid rule action: {s}")),
        }
    }
}

/// Status of a pending approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }
}

impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ApprovalStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            "expired" => Ok(Self::Expired),
            _ => Err(format!("invalid approval status: {s}")),
        }
    }
}

/// Request body sent by the CLI to check whether a request is approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveCheckRequest {
    pub domain: String,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Response from the hub's approval check endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveResponse {
    pub action: RuleAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Request body for searching API endpoint documentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchEndpointsRequest {
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}
