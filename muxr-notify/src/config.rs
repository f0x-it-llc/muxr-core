//! Environment-driven configuration for the relay (v1 has no config file).

use anyhow::{Context, Result, bail};
use std::net::SocketAddr;

/// How the FCM sender behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcmMode {
    /// Mint a real OAuth2 token and POST to FCM HTTP v1.
    Send,
    /// Skip auth + HTTP entirely; log the would-be message. Used by the dev rig,
    /// CI, and all handler tests — needs no Firebase artifacts.
    Log,
}

/// Fully-resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Plain-HTTP listen address (TLS terminates at the fronting proxy).
    pub listen: SocketAddr,
    /// SQLite file path.
    pub db_path: String,
    pub fcm_mode: FcmMode,
    /// Path to the service-account JSON (required when `fcm_mode == Send`).
    pub service_account: Option<String>,
    /// Firebase project id; derived from the SA JSON `project_id` when unset.
    pub project_id: Option<String>,
    /// Per-handle notifications/day cap.
    pub daily_cap: u32,
    /// When true, take the client IP from `X-Forwarded-For` (left-most) rather
    /// than the socket peer address.
    pub trust_proxy: bool,
}

impl Config {
    /// Resolve configuration from the environment, applying defaults and
    /// fail-fast validation (e.g. `FCM_MODE=send` without a service account).
    pub fn from_env() -> Result<Self> {
        let listen_raw = env_or("NOTIFY_LISTEN", "127.0.0.1:8080");
        let listen: SocketAddr = listen_raw
            .parse()
            .with_context(|| format!("invalid NOTIFY_LISTEN '{listen_raw}'"))?;

        let db_path = env_or("NOTIFY_DB", "./notify.db");

        let fcm_mode = match env_or("FCM_MODE", "send").as_str() {
            "send" => FcmMode::Send,
            "log" => FcmMode::Log,
            other => bail!("invalid FCM_MODE '{other}' (expected 'send' or 'log')"),
        };

        let service_account = non_empty_var("FCM_SERVICE_ACCOUNT");
        let project_id = non_empty_var("FCM_PROJECT_ID");

        let daily_cap_raw = env_or("NOTIFY_DAILY_CAP", "150");
        let daily_cap: u32 = daily_cap_raw
            .parse()
            .with_context(|| format!("invalid NOTIFY_DAILY_CAP '{daily_cap_raw}'"))?;

        let trust_proxy = env_or("NOTIFY_TRUST_PROXY", "0") == "1";

        if fcm_mode == FcmMode::Send && service_account.is_none() {
            bail!(
                "FCM_MODE=send requires FCM_SERVICE_ACCOUNT (path to the service-account JSON); \
                 set FCM_MODE=log to run without any Firebase artifacts"
            );
        }

        Ok(Self {
            listen,
            db_path,
            fcm_mode,
            service_account,
            project_id,
            daily_cap,
            trust_proxy,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    non_empty_var(key).unwrap_or_else(|| default.to_string())
}

fn non_empty_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env is process-global; serialize the two env-touching tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear() {
        for k in [
            "NOTIFY_LISTEN",
            "NOTIFY_DB",
            "FCM_MODE",
            "FCM_SERVICE_ACCOUNT",
            "FCM_PROJECT_ID",
            "NOTIFY_DAILY_CAP",
            "NOTIFY_TRUST_PROXY",
        ] {
            // SAFETY: single-threaded within the ENV_LOCK guard.
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn send_mode_without_service_account_fails() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::set_var("FCM_MODE", "send") };
        let err = Config::from_env().unwrap_err().to_string();
        assert!(err.contains("FCM_SERVICE_ACCOUNT"), "got: {err}");
    }

    #[test]
    fn log_mode_needs_no_artifacts() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::set_var("FCM_MODE", "log") };
        let cfg = Config::from_env().expect("log mode must resolve with no Firebase config");
        assert_eq!(cfg.fcm_mode, FcmMode::Log);
        assert_eq!(cfg.daily_cap, 150);
        assert!(!cfg.trust_proxy);
    }
}
