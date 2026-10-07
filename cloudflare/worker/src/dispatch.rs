//! Starting the crawl on GitHub Actions from the Worker's crons
//! (`workflow_dispatch` on `crawl.yml` with input `mode`). GitHub's own
//! `schedule:` is best effort; a Cloudflare cron fires on time.
//!
//! Off unless all are set:
//!   GITHUB_DISPATCH_TOKEN  secret  fine-grained, this repository only, Actions read/write
//!   GITHUB_REPO            var     "owner/name"
//!   GITHUB_WORKFLOW        var     workflow file name, e.g. "crawl.yml"
//!
//! Plain Rust: it shapes the request; `entry.rs` sends it.

use serde_json::{json, Value};

pub const QUICK_CRON: &str = "*/30 * * * *";
pub const FULL_CRON: &str = "0 11 * * *";
/// Daily at 13:15 UTC: away from the 11:00 full sweep and between the
/// quick runs at :00 and :30.
pub const SOLD_CRON: &str = "15 13 * * *";

/// The crawl `mode` a cron starts (CONTRACT.md "Worker crons").
pub fn mode_for(cron: &str) -> Option<&'static str> {
    match cron.trim() {
        QUICK_CRON => Some("quick"),
        FULL_CRON => Some("full"),
        SOLD_CRON => Some("sold"),
        _ => None,
    }
}

/// The daily cron also expires listings unseen for EXPIRE_AFTER_DAYS.
pub fn expires(cron: &str) -> bool {
    cron.trim() == FULL_CRON
}

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchConfig {
    pub token: String,
    pub repo: String,
    pub workflow: String,
    pub git_ref: String,
    pub mode: String,
}

impl DispatchConfig {
    /// `None` unless the token, a well-formed "owner/name" and the workflow are set.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>, mode: &str) -> Option<DispatchConfig> {
        let val = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let repo = val("GITHUB_REPO")?;
        let (owner, name) = repo.split_once('/')?;
        // Letters, digits, "-", "_" and "."; a leading dot could walk the URL path.
        let ok = |s: &str| {
            !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        let workflow = val("GITHUB_WORKFLOW")?;
        if !ok(owner) || !ok(name) || !ok(&workflow) {
            return None;
        }
        // A GitHub token is printable ASCII without spaces; anything else
        // (a pasted command, a line break) would only fail as a bad header.
        let token = val("GITHUB_DISPATCH_TOKEN")?;
        if !token.chars().all(|c| c.is_ascii_graphic()) {
            return None;
        }
        Some(DispatchConfig {
            token,
            repo,
            workflow,
            git_ref: val("GITHUB_REF").unwrap_or_else(|| "main".to_string()),
            mode: mode.to_string(),
        })
    }

    pub fn url(&self) -> String {
        format!("https://api.github.com/repos/{}/actions/workflows/{}/dispatches", self.repo, self.workflow)
    }

    /// GitHub's REST API refuses requests without a User-Agent.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", self.token)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ("User-Agent", "housedeals-api-worker".to_string()),
            ("Content-Type", "application/json".to_string()),
        ]
    }

    pub fn body(&self) -> Value {
        json!({ "ref": self.git_ref, "inputs": { "mode": self.mode } })
    }
}

/// GitHub answers 204 No Content when the run is created.
pub fn started(status: u16, body: &str) -> Result<(), String> {
    if status == 204 {
        return Ok(());
    }
    let why = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());
    Err(format!("GitHub {status}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(k: &str) -> Option<String> {
        match k {
            "GITHUB_DISPATCH_TOKEN" => Some("tok".into()),
            "GITHUB_REPO" => Some("manrajtoor/housing-deal-finder".into()),
            "GITHUB_WORKFLOW" => Some("crawl.yml".into()),
            _ => None,
        }
    }

    #[test]
    fn crons_map_to_modes() {
        assert_eq!(mode_for("*/30 * * * *"), Some("quick"));
        assert_eq!(mode_for("0 11 * * *"), Some("full"));
        assert_eq!(mode_for("15 13 * * *"), Some("sold"));
        assert_eq!(mode_for("0 12 * * SUN"), None, "the weekly sold cron is gone");
        assert!(!expires("15 13 * * *"));
        assert_eq!(mode_for("* * * * *"), None);
        assert!(expires("0 11 * * *") && !expires("*/30 * * * *"));
    }

    #[test]
    fn request_shape() {
        let c = DispatchConfig::from_vars(vars, "full").unwrap();
        assert_eq!(c.url(), "https://api.github.com/repos/manrajtoor/housing-deal-finder/actions/workflows/crawl.yml/dispatches");
        assert_eq!(c.body(), json!({"ref": "main", "inputs": {"mode": "full"}}));
        let h = c.headers();
        assert!(h.contains(&("X-GitHub-Api-Version", "2022-11-28".to_string())));
        assert!(h.contains(&("Authorization", "Bearer tok".to_string())));
        assert!(h.iter().any(|(k, _)| *k == "User-Agent"));
    }

    #[test]
    fn off_with_a_malformed_token() {
        let pasted = |k: &str| if k == "GITHUB_DISPATCH_TOKEN" { Some("pbpaste | npx wrangler\nsecret".into()) } else { vars(k) };
        assert!(DispatchConfig::from_vars(pasted, "quick").is_none());
    }

    #[test]
    fn off_without_token_or_with_a_bad_repo() {
        assert!(DispatchConfig::from_vars(|k| if k == "GITHUB_DISPATCH_TOKEN" { None } else { vars(k) }, "quick").is_none());
        assert!(DispatchConfig::from_vars(|k| if k == "GITHUB_REPO" { Some("../x".into()) } else { vars(k) }, "quick").is_none());
    }

    #[test]
    fn only_204_is_success() {
        assert!(started(204, "").is_ok());
        assert_eq!(started(401, r#"{"message":"Bad credentials"}"#).unwrap_err(), "GitHub 401: Bad credentials");
        assert!(started(200, "").is_err());
    }
}
