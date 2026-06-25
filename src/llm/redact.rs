//! Redact common secret shapes before shipping source snippets to an LLM.
//!
//! Defense in depth: the same source already lives on disk and in git; redaction
//! exists so a developer who runs `fermut suggest` or `--equiv-llm` against a
//! file that *accidentally* embeds a real key (test fixture, hardcoded creds,
//! demo snippet) doesn't leak it to a third-party API.
//!
//! Patterns are deliberately conservative — false positives turn into
//! `[REDACTED]` tokens in the prompt, which at worst weakens the model's
//! context. False negatives (a missed secret) are the failure mode we care
//! about; if a vendor adds a new key prefix, add it here.

use std::sync::OnceLock;

use regex::Regex;

/// Replace likely secrets in `text` with `[REDACTED]`. Pure; no I/O.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for pat in patterns() {
        out = pat.replace_all(&out, "[REDACTED]").into_owned();
    }
    out
}

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw = [
            // Anthropic / OpenAI-style keys.
            r"sk-[A-Za-z0-9_\-]{20,}",
            // OpenAI project keys.
            r"sk-proj-[A-Za-z0-9_\-]{20,}",
            // GitHub PATs and tokens.
            r"gh[pousr]_[A-Za-z0-9]{30,}",
            // AWS access key IDs.
            r"AKIA[0-9A-Z]{16}",
            // Slack tokens.
            r"xox[abprs]-[A-Za-z0-9\-]{10,}",
            // Google API keys.
            r"AIza[0-9A-Za-z_\-]{30,}",
            // Generic JWT: three base64url segments joined by dots.
            r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
            // Hex-encoded secrets >= 32 chars (covers MD5/SHA1/SHA256 hashes
            // and many bearer tokens). Anchored on word boundaries to avoid
            // gobbling neighboring code.
            r"\b[A-Fa-f0-9]{32,}\b",
            // `name = "value"` style: redact the value of obvious secret
            // names. Captures both single and double quoted strings on the
            // same logical line. The `[^"'\n=]{0,40}` between keyword and
            // `=` swallows optional Python type annotations like
            // `api_key: str =` without crossing the assignment operator.
            r#"(?i)\b(?:password|passwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret|bearer)\b[^"'\n=]{0,40}=\s*["'][^"'\n]{4,}["']"#,
        ];
        raw.iter()
            .map(|p| Regex::new(p).expect("redaction regex compiles"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_anthropic_key() {
        let s = "key = sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let r = redact(s);
        assert!(!r.contains("sk-ant"));
        assert!(r.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_github_pat() {
        let s = "GH = ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ";
        assert!(redact(s).contains("[REDACTED]"));
    }

    #[test]
    fn redacts_aws_access_key_id() {
        assert!(redact("AKIAIOSFODNN7EXAMPLE").contains("[REDACTED]"));
    }

    #[test]
    fn redacts_jwt() {
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        assert!(redact(jwt).contains("[REDACTED]"));
    }

    #[test]
    fn redacts_keyword_assignment() {
        let src = "PASSWORD = \"hunter2hunter2\"";
        let r = redact(src);
        assert!(!r.contains("hunter2"));
    }

    #[test]
    fn redacts_keyword_assignment_in_python() {
        let src = "api_key: str = 'my-real-secret-value-12345'";
        let r = redact(src);
        assert!(!r.contains("my-real-secret-value"));
    }

    #[test]
    fn leaves_innocuous_code_alone() {
        let src = "def add(a: int, b: int) -> int:\n    return a + b\n";
        assert_eq!(redact(src), src);
    }

    #[test]
    fn redacts_long_hex_token() {
        let s = "token: 0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(redact(s).contains("[REDACTED]"));
    }

    #[test]
    fn short_hex_kept() {
        // Short hex (e.g. color literals) shouldn't be redacted.
        let s = "color = 0xff00aa";
        assert_eq!(redact(s), s);
    }
}
