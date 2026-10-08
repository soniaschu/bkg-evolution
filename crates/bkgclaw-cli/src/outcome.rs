//! A command's full outcome: the verdict (which decides the exit code) and the
//! report (which the renderer turns into text or JSON).
//!
//! Success carries both. Failure carries only the verdict, because a command
//! that failed before producing data has nothing to render beyond its message.
//! This type lives in the CLI crate so `core` never has to depend on `ui`.

use bkgclaw_core::Verdict;
use bkgclaw_ui::Report;

pub struct Outcome(pub Result<Verdict, Verdict>, pub Report, pub Option<i32>);

impl Outcome {
    /// Success with a payload. The verdict follows `report.ok`, so a command
    /// cannot report ok while its own checks say otherwise.
    pub fn from_report(report: Report) -> Self {
        let verdict = if report.ok {
            Verdict::Ok
        } else {
            Verdict::negative(report.summary.clone())
        };
        Outcome(Ok(verdict), report, None)
    }

    pub fn success(summary: impl Into<String>) -> Self {
        Outcome(Ok(Verdict::Ok), Report::ok(summary), None)
    }

    /// Attach an explicit exit code, for commands that manage their own
    /// process lifetime (an interactive REPL).
    pub fn with_exit(mut self, code: i32) -> Self {
        self.2 = Some(code);
        self
    }

    /// Split into the pieces `main` needs: status decides the exit code,
    /// report decides the rendering.
    pub fn into_parts(self) -> (Result<Verdict, Verdict>, Report, Option<i32>) {
        (self.0, self.1, self.2)
    }

    pub fn fail(verdict: Verdict) -> Self {
        let report = Report {
            ok: false,
            status: verdict.code().to_string(),
            summary: verdict.message(),
            checks: Vec::new(),
            data: None,
        };
        Outcome(Err(verdict), report, None)
    }
}

impl From<Verdict> for Outcome {
    fn from(verdict: Verdict) -> Self {
        Outcome::fail(verdict)
    }
}
