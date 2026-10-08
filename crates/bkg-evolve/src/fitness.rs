//! The fitness judge: real exit codes, real counts, never prose.
//!
//! The lazar principle, sharpened: a model cannot talk its way past this
//! gate, because this module does not read a single word the model wrote
//! about its own work. It runs the same three commands a human reviewer
//! would run and parses their output:
//!
//! - `cargo build` — does it compile at all
//! - `cargo test`  — do the tests pass, and how many are there
//! - `cargo clippy` — how many lints did the attempt add
//!
//! `verdict()` compares against the baseline so "still green" is not the
//! same as "better": clippy warnings must not grow, tests must not shrink.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long one fitness command may run. A stuck build must not hold an
/// evolution cycle hostage.
const FITNESS_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FitnessReport {
    pub build_ok: bool,
    pub tests_ok: bool,
    #[serde(default)]
    pub tests_passed: u64,
    #[serde(default)]
    pub tests_failed: u64,
    #[serde(default)]
    pub clippy_warnings: u64,
    /// Wall time of all three commands, in seconds.
    #[serde(default)]
    pub duration_secs: u64,
    /// The tail of failing output — evidence, not vibes, for the attempt
    /// memory and the journal.
    #[serde(default)]
    pub failure_excerpt: String,
}

fn cargo(dir: &Path, args: &[&str]) -> (bool, String) {
    let start = Instant::now();
    let output = Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .env("CARGO_TERM_COLOR", "never")
        .output();
    let duration = start.elapsed();
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            // A command that outlives the budget is a failure with a name.
            if duration > FITNESS_TIMEOUT {
                return (false, format!("zeitlimit überschritten: cargo {}", args.join(" ")));
            }
            (output.status.success(), text)
        }
        Err(error) => (false, format!("cargo nicht ausführbar: {error}")),
    }
}

/// Count `test result:` lines by outcome. The honest parser: it sums what
/// the harness printed, and nothing else.
fn count_test_results(output: &str) -> (u64, u64) {
    let mut passed = 0;
    let mut failed = 0;
    for line in output.lines().filter(|l| l.trim().starts_with("test result:")) {
        let extract = |key: &str| -> u64 {
            // The harness prints `test result: ok. 10 passed; 0 failed;` —
            // find the token pair "<number> <key>", tolerating the `;` and
            // `.` the harness glues onto the key.
            let tokens: Vec<&str> = line.split_whitespace().collect();
            tokens
                .windows(2)
                .find(|window| {
                    let name = window[1].trim_end_matches([';', '.']);
                    name == key
                })
                .and_then(|window| window[0].parse::<u64>().ok())
                .unwrap_or(0)
        };
        passed += extract("passed");
        failed += extract("failed");
    }
    (passed, failed)
}

fn count_clippy_warnings(output: &str) -> u64 {
    output
        .lines()
        .filter(|l| l.starts_with("warning") && !l.starts_with("warning: unused manifest"))
        .count() as u64
}

/// Measure the tree as it stands right now.
pub fn measure(dir: &Path) -> FitnessReport {
    let start = Instant::now();
    let (build_ok, build_out) = cargo(dir, &["build", "--workspace", "--release"]);
    let mut report = FitnessReport {
        build_ok,
        ..Default::default()
    };
    if !build_ok {
        // Without a build, tests and clippy say nothing about the tree.
        report.failure_excerpt = tail(&build_out, 12);
        report.duration_secs = start.elapsed().as_secs();
        return report;
    }

    let (tests_ok, test_out) = cargo(dir, &["test", "--workspace", "--release"]);
    let (passed, failed) = count_test_results(&test_out);
    report.tests_ok = tests_ok;
    report.tests_passed = passed;
    report.tests_failed = failed;
    if !tests_ok {
        report.failure_excerpt = tail(&test_out, 12);
    }

    // Clippy is evidence, not a gate on its own: existing warnings must
    // not block an otherwise sound improvement — but growth is recorded.
    let (_, clippy_out) = cargo(dir, &["clippy", "--workspace", "--release", "--all-targets"]);
    report.clippy_warnings = count_clippy_warnings(&clippy_out);

    report.duration_secs = start.elapsed().as_secs();
    report
}

/// The gate: is this attempt an improvement, a regression, or a wash?
/// An attempt that breaks the build or any test is reverted, full stop.
/// Shrinking tests or growing lint are called out in the lesson even when
/// the gate passes — the journal keeps the operator honest.
pub fn verdict(report: &FitnessReport, baseline: &FitnessReport) -> Verdict {
    if !report.build_ok || !report.tests_ok {
        return Verdict::Reverted;
    }
    if report.tests_passed < baseline.tests_passed {
        return Verdict::Suspect;
    }
    if report.clippy_warnings > baseline.clippy_warnings {
        return Verdict::Suspect;
    }
    if report.tests_passed > baseline.tests_passed {
        Verdict::Improved
    } else if report.clippy_warnings < baseline.clippy_warnings {
        Verdict::Improved
    } else {
        Verdict::Neutral
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Fitness improved measurably: more tests or fewer warnings, nothing broken.
    Improved,
    /// Nothing broken, nothing better. Kept on the branch, not pushed as a win.
    Neutral,
    /// Green, but tests shrank or warnings grew — kept, but the journal names it.
    Suspect,
    /// Build or tests broke: reverted.
    Reverted,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Improved => "improved",
            Verdict::Neutral => "neutral",
            Verdict::Suspect => "suspect",
            Verdict::Reverted => "reverted",
        }
    }
}

fn tail(text: &str, lines: usize) -> String {
    text.lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_lines_are_summed_across_suites() {
        let output = "\
running 10 tests
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured

running 2 tests
test result: FAILED. 1 passed; 1 failed; 0 ignored";
        let (passed, failed) = count_test_results(output);
        assert_eq!((passed, failed), (11, 1));
    }

    #[test]
    fn a_broken_build_short_circuits_with_evidence() {
        let report = FitnessReport {
            build_ok: false,
            failure_excerpt: "error[E0308]: mismatched types".into(),
            ..Default::default()
        };
        assert_eq!(
            verdict(&report, &FitnessReport::default()),
            Verdict::Reverted
        );
        assert!(report.failure_excerpt.contains("E0308"));
    }

    #[test]
    fn the_verdict_distinguishes_all_four_outcomes() {
        let base = FitnessReport {
            build_ok: true,
            tests_ok: true,
            tests_passed: 100,
            clippy_warnings: 10,
            ..Default::default()
        };
        // More tests: improved.
        let better = FitnessReport { tests_passed: 102, ..base.clone() };
        assert_eq!(verdict(&better, &base), Verdict::Improved);
        // Same tests, fewer warnings: improved.
        let cleaner = FitnessReport { clippy_warnings: 8, ..base.clone() };
        assert_eq!(verdict(&cleaner, &base), Verdict::Improved);
        // Identical: neutral.
        assert_eq!(verdict(&base, &base), Verdict::Neutral);
        // Fewer tests while green: suspect, and the gate says so.
        let shrunken = FitnessReport { tests_passed: 98, ..base.clone() };
        assert_eq!(verdict(&shrunken, &base), Verdict::Suspect);
        // More warnings: suspect.
        let noisy = FitnessReport { clippy_warnings: 12, ..base.clone() };
        assert_eq!(verdict(&noisy, &base), Verdict::Suspect);
    }

    #[test]
    fn clippy_warning_lines_are_counted_not_guessed() {
        let output = "\
warning: unused variable: `x`
warning: function is never used
   Compiling foo
warning: unused manifest key";
        assert_eq!(count_clippy_warnings(output), 2);
    }
}
