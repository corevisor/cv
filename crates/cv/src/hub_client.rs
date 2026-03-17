use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crate::types::{ApprovalStatus, ApproveCheckRequest, ApproveResponse, SearchEndpointsRequest};

/// Trait for checking request approvals against a hub.
#[async_trait]
pub trait ApprovalChecker: Send + Sync {
    async fn check_approval(
        &self,
        profile_id: &str,
        domain: &str,
        method: &str,
        path: &str,
        context: Option<&str>,
    ) -> Result<ApproveResponse>;

    async fn poll_approval(
        &self,
        profile_id: &str,
        approval_id: &str,
        timeout: Duration,
    ) -> Result<ApprovalStatus>;
}

/// HTTP client for the Corevisor Hub.
#[derive(Clone)]
pub struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    oauth_token: String,
}

/// Profile summary returned by the hub.
#[derive(Debug, serde::Deserialize)]
pub struct ProfileResponse {
    pub id: String,
    pub name: String,
}

impl HubClient {
    pub fn new(hub_url: String, oauth_token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            hub_url,
            oauth_token,
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.oauth_token)
    }

    /// Device-code OAuth login flow. Returns the access token.
    pub async fn oauth_login(&self) -> Result<String> {
        let resp = self
            .http
            .post(format!("{}/oauth/device", self.hub_url))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("failed to create device session: {body}");
        }

        #[derive(serde::Deserialize)]
        struct DeviceSession {
            session_id: String,
            user_code: String,
            login_url: String,
            poll_interval: u64,
        }
        let session: DeviceSession = resp.json().await?;

        let full_login_url = format!("{}{}", self.hub_url, session.login_url);
        let _ = open::that(&full_login_url);
        eprintln!("Opening browser for authentication...");
        eprintln!("If the browser didn't open, visit this URL:\n  {full_login_url}");
        eprintln!("\nVerification code: {}\n", session.user_code);
        eprintln!("Waiting for authentication...");

        let poll_url = format!(
            "{}/oauth/device/poll?session_id={}",
            self.hub_url, session.session_id
        );
        let interval = std::time::Duration::from_secs(session.poll_interval);

        loop {
            tokio::time::sleep(interval).await;

            let resp = self.http.get(&poll_url).send().await?;

            if resp.status() == reqwest::StatusCode::GONE {
                anyhow::bail!("device session expired — please try again");
            }

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("poll failed: {body}");
            }

            #[derive(serde::Deserialize)]
            struct PollResponse {
                status: String,
                access_token: Option<String>,
            }
            let poll: PollResponse = resp.json().await?;

            if poll.status == "authorized" {
                let token = poll
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("authorized but no token returned"))?;
                return Ok(token);
            }
        }
    }

    /// Fetch all profiles for the authenticated user.
    pub async fn get_profiles(&self) -> Result<Vec<ProfileResponse>> {
        let resp = self
            .http
            .get(format!("{}/profiles", self.hub_url))
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("get profiles failed: {body}");
        }

        Ok(resp.json().await?)
    }

    /// Search API endpoints via the hub registry.
    pub async fn search_api_docs(
        &self,
        pattern: &str,
        domain: Option<&str>,
        slug: Option<&str>,
    ) -> Result<serde_json::Value> {
        let body = SearchEndpointsRequest {
            pattern: pattern.to_string(),
            domain: domain.map(String::from),
            slug: slug.map(String::from),
            mode: None,
        };

        let resp = self
            .http
            .post(format!(
                "{}/registry/search/endpoints",
                self.hub_url
            ))
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("search failed: {body}");
        }

        Ok(resp.json().await?)
    }

}

#[async_trait]
impl ApprovalChecker for HubClient {
    async fn check_approval(
        &self,
        profile_id: &str,
        domain: &str,
        method: &str,
        path: &str,
        context: Option<&str>,
    ) -> Result<ApproveResponse> {
        let body = ApproveCheckRequest {
            domain: domain.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            context: context.map(String::from),
        };

        let resp = self
            .http
            .post(format!(
                "{}/profiles/{}/approve",
                self.hub_url, profile_id
            ))
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("approval check failed: {body}");
        }

        Ok(resp.json().await?)
    }

    async fn poll_approval(
        &self,
        profile_id: &str,
        approval_id: &str,
        timeout: Duration,
    ) -> Result<ApprovalStatus> {
        let poll_url = format!(
            "{}/profiles/{}/approvals/{}",
            self.hub_url, profile_id, approval_id
        );
        let poll_interval = Duration::from_secs(2);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() >= timeout {
                return Ok(ApprovalStatus::Expired);
            }

            tokio::time::sleep(poll_interval).await;

            let resp = self
                .http
                .get(&poll_url)
                .header("Authorization", self.auth_header())
                .send()
                .await?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("poll approval failed: {body}");
            }

            #[derive(serde::Deserialize)]
            struct PollResponse {
                status: ApprovalStatus,
            }
            let poll: PollResponse = resp.json().await?;

            match poll.status {
                ApprovalStatus::Pending => continue,
                status => return Ok(status),
            }
        }
    }
}
