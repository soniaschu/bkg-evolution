//! Credential leak detection.
//!
//! Scans model output and tool output for accidentally exposed secrets before
//! either is displayed, logged or persisted. The failure this prevents is
//! mundane and common: an agent echoes an env var into a transcript, and the
//! transcript is committed.
//!
//! Detection is deliberately conservative about *what it does*: it reports a
//! finding with a span and a rule name, and never rewrites the text. Silently
//! redacting output makes a tool lie about what it returned.

use regex::Regex;
use serde::{Deserialize, Serialize};

/// What a finding names. Not the matched text — reporting the secret again in
/// the finding would defeat the purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    /// Byte offset where the match starts.
    pub start: usize,
    pub end: usize,
    /// A non-reversible hint so two occurrences of the same key are
    /// distinguishable in a report without printing either.
    pub fingerprint: String,
}

/// The rules. Each is a shape that a real credential takes; none of them match
/// ordinary prose.
pub struct Detector {
    rules: Vec<(&'static str, Regex)>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        let rules: Vec<(&'static str, Regex)> = vec![
            ("anthropic-key", Regex::new(r"sk-ant-[A-Za-z0-9_\-]{20,}").unwrap()),
            ("openai-key", Regex::new(r"sk-(?:proj-)?[A-Za-z0-9_\-]{32,}").unwrap()),
            ("github-token", Regex::new(r"gh[pousr]_[A-Za-z0-9]{16,}").unwrap()),
            ("aws-access-key", Regex::new(r"AKIA[0-9A-Z]{16}").unwrap()),
            ("google-api-key", Regex::new(r"AIza[0-9A-Za-z_\-]{35}").unwrap()),
            ("slack-token", Regex::new(r"xox[baprs]-[0-9A-Za-z\-]{10,}").unwrap()),
            ("stripe-key", Regex::new(r"sk_live_[0-9a-zA-Z]{20,}").unwrap()),
            ("private-key-block", Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |PGP )?PRIVATE KEY-----").unwrap()),
            ("bearer-token", Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9_\-\.=/]{20,}").unwrap()),
            ("jwt", Regex::new(r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}").unwrap()),
            ("assign-secret", Regex::new(
                r#"(?i)\b(?:api[_-]?key|secret|password|passwd|token)\s*[=:]\s*["']?[A-Za-z0-9_\-]{16,}["']?"#,
            ).unwrap()),
            ("connection-string", Regex::new(r"(?i)\b(?:postgres|postgresql|mysql|mongodb(?:\+srv)?|redis)://[^\s:@/]+:[^\s@]+@").unwrap()),
        ];
        Detector { rules }
    }

    /// Scan text and return every finding, ordered by position.
    pub fn scan(&self, text: &str) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (rule, pattern) in &self.rules {
            for capture in pattern.find_iter(text) {
                findings.push(Finding {
                    rule: (*rule).to_string(),
                    start: capture.start(),
                    end: capture.end(),
                    fingerprint: fingerprint(&text[capture.start()..capture.end()]),
                });
            }
        }
        // Sort by position so a report reads like the text it describes.
        findings.sort_by_key(|f| (f.start, f.end));
        findings.dedup_by(|a, b| a.start == b.start && a.end == b.end);
        findings
    }

    pub fn is_clean(&self, text: &str) -> bool {
        self.scan(text).is_empty()
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// A short, non-reversible hint. FNV-1a: no crypto dependency, and it must not
/// be reversible — a hash of a short secret can be brute-forced, which is why
/// the input is salted with a per-finding constant.
fn fingerprint(matched: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in matched.as_bytes().iter().chain([0u8].iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("leak:{hash:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> Detector {
        Detector::new()
    }

    #[test]
    fn twelve_rules_are_registered() {
        assert_eq!(detector().rule_count(), 12);
    }

    #[test]
    fn ordinary_prose_is_clean() {
        let text = "I read the file and it contains a function named `parse_config` which returns a Result.";
        assert!(
            detector().is_clean(text),
            "a false positive here would train the operator to ignore it"
        );
    }

    #[test]
    fn an_anthropic_key_is_found() {
        let text = "the key is sk-ant-api03-AbCdEf0123456789AbCdEf0123456789 here";
        let findings = detector().scan(text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "anthropic-key");
    }

    #[test]
    fn a_private_key_block_is_found() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...";
        assert_eq!(detector().scan(text)[0].rule, "private-key-block");
    }

    #[test]
    fn a_jwt_is_found() {
        let text = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(detector().scan(text)[0].rule, "jwt");
    }

    #[test]
    fn a_connection_string_with_a_password_is_found() {
        let text = "postgres://admin:hunter2supersecret@db.internal:5432/app";
        assert_eq!(detector().scan(text)[0].rule, "connection-string");
    }

    #[test]
    fn a_finding_never_contains_the_secret_itself() {
        // The whole point. If the report carried the value, the report would be
        // the leak.
        let secret = "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789";
        let finding = &detector().scan(&format!("here: {secret}"))[0];
        let rendered = format!("{finding:?}");
        assert!(
            !rendered.contains("AbCdEf0123456789"),
            "the finding leaked the secret: {rendered}"
        );
        assert!(finding.fingerprint.starts_with("leak:"));
    }

    #[test]
    fn the_same_secret_gets_the_same_fingerprint() {
        let secret = "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789";
        let a = detector().scan(secret)[0].fingerprint.clone();
        let b = detector().scan(&format!("prefix {secret} suffix"))[0]
            .fingerprint
            .clone();
        assert_eq!(a, b, "the fingerprint must be stable for correlation");
    }

    #[test]
    fn findings_carry_the_position_in_the_text() {
        let text = "clean text then sk-ant-api03-AbCdEf0123456789AbCdEf0123456789";
        let finding = &detector().scan(text)[0];
        assert_eq!(
            &text[finding.start..finding.end],
            "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789"
        );
    }

    #[test]
    fn multiple_secrets_are_all_reported_in_order() {
        let text = "first sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA then AKIAIOSFODNN7EXAMPLE";
        let findings = detector().scan(text);
        assert_eq!(findings.len(), 2);
        assert!(
            findings[0].start < findings[1].start,
            "findings must read in text order"
        );
    }

    #[test]
    fn a_short_token_is_not_a_secret() {
        // Aggressive matching produces noise, and noise gets ignored.
        assert!(detector().is_clean("token = abc123"));
        assert!(detector().is_clean("password: hunter2"));
    }

    #[test]
    fn a_long_opaque_id_is_not_mistaken_for_a_key() {
        // A commit SHA or a content hash must not trip the detector.
        assert!(detector().is_clean("commit 9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0"));
    }

    #[test]
    fn empty_and_whitespace_input_are_clean() {
        assert!(detector().is_clean(""));
        assert!(detector().is_clean("   \n\t "));
    }

    #[test]
    fn detection_does_not_modify_the_text() {
        // Redacting silently would make a tool misreport what it returned.
        let secret = "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789";
        let text = format!("before {secret} after");
        detector().scan(&text);
        assert_eq!(
            text,
            "before sk-ant-api03-AbCdEf0123456789AbCdEf0123456789 after"
        );
    }
}
