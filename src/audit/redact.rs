//! Secret redaction for audit events and logs. Pattern-based and therefore
//! best-effort: it lowers the chance of persisting credentials, it does not
//! prove their absence.

use std::sync::OnceLock;

use regex::Regex;

pub const REDACTED: &str = "[REDACTED]";

struct Rules {
    patterns: Vec<(Regex, &'static str)>,
    sensitive_key: Regex,
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| {
        let p = |s: &str| Regex::new(s).expect("valid redaction regex");
        Rules {
            patterns: vec![
                // PEM private key blocks (multi-line).
                (
                    p(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----"),
                    "[REDACTED PRIVATE KEY]",
                ),
                // Authorization headers.
                (p(r"(?i)(authorization\s*[:=]\s*)(basic|bearer|token)?\s*[^\s,;]+"), "${1}[REDACTED]"),
                (p(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}"), "Bearer [REDACTED]"),
                // Anthropic before OpenAI (sk-ant- also matches sk-).
                (p(r"sk-ant-[A-Za-z0-9_-]{10,}"), "[REDACTED:anthropic]"),
                (p(r"sk-(proj-)?[A-Za-z0-9_-]{20,}"), "[REDACTED:openai]"),
                // GitHub tokens.
                (p(r"gh[pousr]_[A-Za-z0-9]{20,}"), "[REDACTED:github]"),
                (p(r"github_pat_[A-Za-z0-9_]{20,}"), "[REDACTED:github]"),
                // AWS access key ids and secret keys in assignments.
                (p(r"\b(AKIA|ASIA)[A-Z0-9]{16}\b"), "[REDACTED:aws-key-id]"),
                (
                    p(r"(?i)(aws_secret_access_key\s*[:=]\s*)[A-Za-z0-9/+=]{30,}"),
                    "${1}[REDACTED]",
                ),
                // Slack / generic high-signal tokens.
                (p(r"xox[abprs]-[A-Za-z0-9-]{10,}"), "[REDACTED:slack]"),
                // password=… / secret: … / token=… style assignments.
                (
                    p(r#"(?i)\b([A-Z0-9_]*(PASSWORD|PASSWD|SECRET|TOKEN|API_KEY|APIKEY|PRIVATE_KEY|CREDENTIALS?)[A-Z0-9_]*)(\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s"',;]+)"#),
                    "${1}${3}[REDACTED]",
                ),
                // URLs with embedded credentials.
                (p(r"(?i)(https?://)[^/\s:@]+:[^/\s@]+@"), "${1}[REDACTED]@"),
            ],
            sensitive_key: p(
                r"(?i)(password|passwd|secret|token|api[_-]?key|authorization|private[_-]?key|credential|cookie)",
            ),
        }
    })
}

/// Redact likely secrets in free text.
pub fn redact_str(s: &str) -> String {
    let mut out = s.to_string();
    for (re, rep) in &rules().patterns {
        if re.is_match(&out) {
            out = re.replace_all(&out, *rep).into_owned();
        }
    }
    out
}

/// True when a map key name suggests its value is a secret.
pub fn is_sensitive_key(k: &str) -> bool {
    rules().sensitive_key.is_match(k)
}

/// Recursively redact a JSON value: sensitive keys lose their values, all
/// strings are pattern-scrubbed.
pub fn redact_value(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            let r = redact_str(s);
            if r != *s {
                *s = r;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact_value),
        Value::Object(o) => {
            for (k, val) in o.iter_mut() {
                // Only string values can carry secrets; counts such as
                // `cached_tokens: 42` must stay readable.
                if is_sensitive_key(k) && matches!(val, Value::String(_)) {
                    *val = Value::String(REDACTED.into());
                } else {
                    redact_value(val);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_secrets() {
        let cases = [
            ("Authorization: Bearer abcdefghijklmnop", "abcdefghijklmnop"),
            ("curl -H 'authorization: token ghp_abcdefghijklmnopqrstuvwxyz0123'", "ghp_"),
            ("key AKIAABCDEFGHIJKLMNOP here", "AKIAABCDEFGHIJKLMNOP"),
            ("OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwx", "sk-proj-abc"),
            ("x sk-ant-api03-abcdefghijklmnop y", "sk-ant-api03"),
            ("github_pat_11ABCDEFGHIJKLMNOPQRSTUV_xyz", "github_pat_11ABC"),
            ("DB_PASSWORD=hunter2", "hunter2"),
            ("password: \"s3cr3t value\"", "s3cr3t"),
            ("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "wJalr"),
            ("https://user:pa55@example.com/x", "pa55"),
        ];
        for (input, secret) in cases {
            let out = redact_str(input);
            assert!(!out.contains(secret), "{input:?} -> {out:?}");
            assert!(out.contains("REDACTED"), "{input:?} -> {out:?}");
        }
    }

    #[test]
    fn redacts_private_keys() {
        let k = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3Nza\nzzz\n-----END OPENSSH PRIVATE KEY-----";
        let out = redact_str(&format!("before\n{k}\nafter"));
        assert!(!out.contains("AAAAB3Nza"));
        assert!(out.contains("before") && out.contains("after"));
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let s = "cargo test --all && git status; token count 42";
        assert_eq!(redact_str("cargo test --all && git status"), "cargo test --all && git status");
        // "token count 42" is not an assignment.
        assert_eq!(redact_str(s), s);
    }

    #[test]
    fn redacts_sensitive_json_keys() {
        let mut v = serde_json::json!({"env": {"GITHUB_TOKEN": "abc", "PATH": "/bin"}, "argv": ["x", "--password=zz"], "usage": {"cached_tokens": 42}});
        redact_value(&mut v);
        assert_eq!(v["usage"]["cached_tokens"], 42);
        assert_eq!(v["env"]["GITHUB_TOKEN"], REDACTED);
        assert_eq!(v["env"]["PATH"], "/bin");
        assert!(!v["argv"][1].as_str().unwrap().contains("zz"));
    }
}
