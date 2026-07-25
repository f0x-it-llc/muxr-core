//! FCM HTTP v1 sender (hand-rolled: `gcp_auth` mints the OAuth2 token, `reqwest`
//! POSTs the message). No FCM crate dependency.

use crate::config::{Config, FcmMode};
use anyhow::{Context, Result};
use gcp_auth::{CustomServiceAccount, TokenProvider};
use log::{error, info, warn};
use serde_json::json;

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const ANDROID_CHANNEL: &str = "muxr_agent";

/// Outcome of a send attempt, mapped to HTTP status by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// FCM accepted the message (or we are in log mode).
    Delivered,
    /// FCM says the token is UNREGISTERED/NOT_FOUND — prune it.
    Unregistered,
    /// FCM returned 429/5xx — transient upstream failure → 502 to the caller.
    UpstreamError,
}

/// The sender, either live (real FCM) or a no-op logger.
pub enum FcmSender {
    Log,
    Live {
        // Boxed: CustomServiceAccount is large; keeps the enum compact.
        service_account: Box<CustomServiceAccount>,
        project_id: String,
        client: reqwest::Client,
    },
}

impl FcmSender {
    /// Build the sender from resolved config. In `send` mode this loads the
    /// service account (failing fast on a missing/invalid file) and derives the
    /// project id from `FCM_PROJECT_ID` or the SA JSON. In `log` mode it touches
    /// no Firebase artifacts.
    pub fn build(config: &Config) -> Result<Self> {
        match config.fcm_mode {
            FcmMode::Log => Ok(FcmSender::Log),
            FcmMode::Send => {
                let path = config
                    .service_account
                    .as_deref()
                    .context("FCM_MODE=send requires FCM_SERVICE_ACCOUNT")?;
                let service_account = CustomServiceAccount::from_file(path)
                    .with_context(|| format!("loading service account from '{path}'"))?;
                let project_id = match &config.project_id {
                    Some(p) => p.clone(),
                    None => service_account.project_id().map(str::to_string).context(
                        "FCM_PROJECT_ID unset and the service-account JSON has no project_id",
                    )?,
                };
                let client = reqwest::Client::builder()
                    .build()
                    .context("building reqwest client")?;
                Ok(FcmSender::Live {
                    service_account: Box::new(service_account),
                    project_id,
                    client,
                })
            }
        }
    }

    /// Send a notification. `title`/`body` are user-visible; `kind` rides the flat
    /// string `data` map for tap-routing.
    pub async fn send(
        &self,
        fcm_token: &str,
        kind: &str,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<SendOutcome> {
        match self {
            FcmSender::Log => {
                info!(
                    "FCM(log): would send kind={kind} token={} title={:?} body={:?}",
                    token_prefix(fcm_token),
                    title,
                    body,
                );
                Ok(SendOutcome::Delivered)
            }
            FcmSender::Live {
                service_account,
                project_id,
                client,
            } => {
                let token = service_account
                    .token(&[FCM_SCOPE])
                    .await
                    .context("minting FCM access token")?;

                let mut notification = serde_json::Map::new();
                if let Some(t) = title {
                    notification.insert("title".into(), json!(t));
                }
                if let Some(b) = body {
                    notification.insert("body".into(), json!(b));
                }

                let message = json!({
                    "message": {
                        "token": fcm_token,
                        "notification": notification,
                        "android": {
                            "priority": "HIGH",
                            "notification": { "channel_id": ANDROID_CHANNEL }
                        },
                        "data": { "kind": kind }
                    }
                });

                let url =
                    format!("https://fcm.googleapis.com/v1/projects/{project_id}/messages:send");
                let resp = client
                    .post(&url)
                    .bearer_auth(token.as_str())
                    .json(&message)
                    .send()
                    .await
                    .context("POST to FCM")?;

                let status = resp.status();
                if status.is_success() {
                    return Ok(SendOutcome::Delivered);
                }
                let text = resp.text().await.unwrap_or_default();
                if status.as_u16() == 404
                    || text.contains("UNREGISTERED")
                    || text.contains("NOT_FOUND")
                {
                    warn!(
                        "FCM pruned token={} (status={status})",
                        token_prefix(fcm_token)
                    );
                    Ok(SendOutcome::Unregistered)
                } else {
                    error!(
                        "FCM upstream error token={} status={status} body={text}",
                        token_prefix(fcm_token)
                    );
                    Ok(SendOutcome::UpstreamError)
                }
            }
        }
    }
}

/// Log-safe 8-char prefix of a secret (handle or FCM token). Never log the full
/// value.
pub fn token_prefix(s: &str) -> &str {
    let end = s.char_indices().nth(8).map_or(s.len(), |(i, _)| i);
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_truncates_to_eight() {
        assert_eq!(token_prefix("0123456789abcdef"), "01234567");
        assert_eq!(token_prefix("short"), "short");
    }
}
