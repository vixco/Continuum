//! Gateway error type. Every variant maps to an actionable user-facing
//! message via [`GatewayError::user_message`].

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("endpoint unreachable: {url}")]
    Unreachable {
        url: String,
        #[source]
        source: Option<reqwest::Error>,
    },
    #[error("rate limited")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("request timed out")]
    Timeout,
    #[error("claude CLI not found")]
    CliNotFound,
    #[error("claude CLI not logged in")]
    CliNotLoggedIn,
    #[error("bad response: {detail}")]
    BadResponse { detail: String },
    #[error("cancelled")]
    Cancelled,
}

impl GatewayError {
    /// Actionable message shown in the UI. Never includes secrets.
    pub fn user_message(&self) -> String {
        match self {
            Self::Unauthorized => {
                "API key rejected (401). Check the key for this provider in Settings → Integrations.".into()
            }
            Self::Unreachable { url, .. } => format!(
                "Could not reach {url}. Is the server running? For LM Studio check the local server is started; for Ollama run `ollama serve`."
            ),
            Self::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(s) => format!("Rate limited by the provider. Try again in {s} seconds."),
                None => "Rate limited by the provider. Try again shortly.".into(),
            },
            Self::Timeout => "The provider did not respond in time. Try again or pick a smaller model.".into(),
            Self::CliNotFound => {
                "Claude Code CLI not found. Install it: npm install -g @anthropic-ai/claude-code".into()
            }
            Self::CliNotLoggedIn => "Claude Code is not logged in. Run: claude login".into(),
            Self::BadResponse { detail } => format!("Provider returned an unexpected response: {detail}"),
            Self::Cancelled => "Stopped.".into(),
        }
    }

    /// Whether a retry with the same input could plausibly succeed.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. }
                | Self::RateLimited { .. }
                | Self::Timeout
                | Self::BadResponse { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_messages_are_actionable_and_secret_free() {
        let cases: Vec<(GatewayError, &str)> = vec![
            (GatewayError::Unauthorized, "Settings"),
            (GatewayError::CliNotFound, "npm install"),
            (GatewayError::CliNotLoggedIn, "claude login"),
            (
                GatewayError::RateLimited {
                    retry_after_secs: Some(30),
                },
                "30",
            ),
            (
                GatewayError::Unreachable {
                    url: "http://localhost:1234/v1".into(),
                    source: None,
                },
                "localhost:1234",
            ),
        ];
        for (err, needle) in cases {
            assert!(
                err.user_message().contains(needle),
                "{err:?} -> {}",
                err.user_message()
            );
        }
        assert!(GatewayError::RateLimited {
            retry_after_secs: None
        }
        .retryable());
        assert!(!GatewayError::Unauthorized.retryable());
    }

    #[test]
    fn provider_connection_serde_roundtrip_has_no_secret_fields() {
        let conn = crate::types::ProviderConnection {
            id: "p1".into(),
            display_name: "LM Studio".into(),
            kind: crate::types::ProviderKind::OpenAiCompat,
            base_url: Some("http://localhost:1234/v1".into()),
            catalog_id: Some("lmstudio".into()),
            models: vec!["qwen3-8b".into()],
            default_model: Some("qwen3-8b".into()),
            roles: vec![],
            requires_key: false,
            last_tested_at: None,
            last_test_ok: Some(true),
        };
        let json = serde_json::to_string(&conn).expect("serialize");
        for banned in ["key", "token", "secret"] {
            // field *names* must not suggest secret storage ("requires_key" is the one allowed hit)
            assert_eq!(
                json.matches(banned).count(),
                usize::from(banned == "key"),
                "{json}"
            );
        }
        let back: crate::types::ProviderConnection =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, conn);
    }
}
