//! Best-effort local sensitivity detection.
//!
//! Findings report categories and counts only. Echoing matched values would turn a
//! protective diagnostic into another place where secrets can leak.

use crate::domain::{PortableEvent, PortableSessionV1};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensitivityFinding {
    pub category: String,
    pub count: usize,
}

pub fn scan(session: &PortableSessionV1) -> Vec<SensitivityFinding> {
    let patterns = [
        (
            "private key",
            r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----",
        ),
        (
            "API credential",
            r#"(?i)(?:api[_-]?key|token|secret)[\s"'=:\\]+[A-Za-z0-9_./+\-=]{16,}"#,
        ),
        (
            "provider key",
            r"(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,})",
        ),
        ("password assignment", r"(?i)password\s*[=:]\s*[^\s,;]{8,}"),
    ];
    let mut haystack = String::new();
    for event in &session.events {
        match event {
            PortableEvent::Message { text, .. }
            | PortableEvent::Summary { text, .. }
            | PortableEvent::Error { text, .. } => {
                haystack.push_str(text);
                haystack.push('\n');
            }
            PortableEvent::ToolCall { input, .. } => {
                haystack.push_str(&input.to_string());
                haystack.push('\n');
            }
            PortableEvent::ToolResult { output, .. } => {
                haystack.push_str(output);
                haystack.push('\n');
            }
        }
    }
    if let Some(git) = &session.git {
        haystack.push_str(&git.staged_diff);
        haystack.push_str(&git.unstaged_diff);
    }
    patterns
        .into_iter()
        .filter_map(|(category, pattern)| {
            let count = Regex::new(pattern).ok()?.find_iter(&haystack).count();
            (count > 0).then(|| SensitivityFinding {
                category: category.into(),
                count,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Harness, PortableEvent, Role, SessionSummary};
    use std::path::PathBuf;

    #[test]
    fn reports_a_category_without_repeating_the_secret() {
        let secret = "ExampleCredentialValue123456";
        let mut session = PortableSessionV1::new(SessionSummary::new(
            Harness::Codex,
            "test".into(),
            PathBuf::new(),
        ));
        session.events.push(PortableEvent::Message {
            id: None,
            parent_id: None,
            role: Role::User,
            text: format!("api_key={secret}"),
            timestamp: None,
        });

        let serialized = serde_json::to_string(&scan(&session)).unwrap();

        assert!(serialized.contains("API credential"));
        assert!(!serialized.contains(secret));
    }
}
