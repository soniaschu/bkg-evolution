//! The exit-code contract.
//!
//! Both templates get this wrong in the same way: `main.rs` returns `anyhow::Result<()>`
//! and every failure collapses to exit 1. A script cannot then tell "your config is
//! malformed" from "the provider is unreachable" from "that was a legitimate negative
//! answer". So it re-runs, or worse, it assumes success.
//!
//! Here the exit code is part of the type. Every command returns a `Verdict`, and
//! the mapping to a process exit code is the one place that decides it.

use std::fmt;

/// A command outcome, in the four classes a caller can act on.
///
/// This is deliberately not `Result<(), E>`. The error type says *what* went
/// wrong; this says *what kind of thing* went wrong, which is what a script
/// branches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The command succeeded.
    Ok,
    /// The command ran correctly and the answer was "no". Not a failure.
    /// Examples: `doctor` found problems, `check` found violations.
    /// The caller asked "is it ok?" and got a truthful "no".
    Negative(String),
    /// The caller got it wrong: bad flag, missing argument, malformed input.
    Usage(String),
    /// The environment is not ready: no binary, no credential, no network.
    Environment(String),
    /// Something we did not anticipate. Always carries a class for triage.
    Internal {
        class: &'static str,
        message: String,
    },
}

impl Verdict {
    pub fn negative(message: impl Into<String>) -> Self {
        Verdict::Negative(message.into())
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Verdict::Usage(message.into())
    }

    pub fn environment(message: impl Into<String>) -> Self {
        Verdict::Environment(message.into())
    }

    pub fn internal(class: &'static str, message: impl Into<String>) -> Self {
        Verdict::Internal {
            class,
            message: message.into(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Verdict::Ok)
    }

    /// The process exit code.
    ///
    /// 0 ok · 1 negative verdict · 2 usage · 3 environment · 4 internal
    pub fn exit_code(&self) -> i32 {
        match self {
            Verdict::Ok => 0,
            Verdict::Negative(_) => 1,
            Verdict::Usage(_) => 2,
            Verdict::Environment(_) => 3,
            Verdict::Internal { .. } => 4,
        }
    }

    /// Stable machine-readable code, for `--json` consumers.
    pub fn code(&self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Negative(_) => "negative",
            Verdict::Usage(_) => "usage",
            Verdict::Environment(_) => "environment",
            Verdict::Internal { class, .. } => class,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Verdict::Ok => String::new(),
            Verdict::Negative(m) | Verdict::Usage(m) | Verdict::Environment(m) => m.clone(),
            Verdict::Internal { class, message } => format!("{class}: {message}"),
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self {
            Verdict::Ok => return write!(f, "ok"),
            Verdict::Negative(_) => "[NEGATIVE]",
            Verdict::Usage(_) => "[USAGE]",
            Verdict::Environment(_) => "[ENVIRONMENT]",
            Verdict::Internal { class, .. } => return write!(f, "[{class}] {}", self.message()),
        };
        write!(f, "{prefix} {}", self.message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_classes_have_distinct_exit_codes() {
        assert_eq!(Verdict::Ok.exit_code(), 0);
        assert_eq!(Verdict::negative("nope").exit_code(), 1);
        assert_eq!(Verdict::usage("bad flag").exit_code(), 2);
        assert_eq!(Verdict::environment("no binary").exit_code(), 3);
        assert_eq!(Verdict::internal("io", "disk gone").exit_code(), 4);
    }

    #[test]
    fn a_negative_answer_is_not_a_failure() {
        // `doctor` finding a broken config is a correct, useful answer.
        let v = Verdict::negative("1 check failed");
        assert!(!v.is_ok());
        assert_eq!(v.code(), "negative");
        assert_ne!(v.exit_code(), 0, "the caller must be able to detect it");
    }

    #[test]
    fn each_class_carries_its_own_stable_code() {
        assert_eq!(Verdict::usage("x").code(), "usage");
        assert_eq!(Verdict::environment("x").code(), "environment");
        assert_eq!(Verdict::internal("tls", "x").code(), "tls");
    }

    #[test]
    fn ok_has_no_message_to_print() {
        assert_eq!(Verdict::Ok.message(), "");
        assert_eq!(Verdict::Ok.to_string(), "ok");
    }
}
