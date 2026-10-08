//! Output rendering: one payload, two renderers.
//!
//! Both templates interleave their output format with their command logic, so
//! `--json` is supported on the commands someone remembered to add it to. Here
//! every command returns a `Report`, and the renderer decides the shape. A
//! command cannot forget: it does not know how to print.

use serde::Serialize;

/// One check result. Every diagnostic in the CLI is one of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    /// Short stable code, e.g. `[ENV]`. Stable across releases so scripts
    /// can match on it.
    pub code: String,
    pub detail: String,
}

impl Check {
    pub fn ok(name: &str, code: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            ok: true,
            code: code.into(),
            detail: detail.into(),
        }
    }

    pub fn fail(name: &str, code: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            ok: false,
            code: code.into(),
            detail: detail.into(),
        }
    }
}

/// What a command produced. The renderer turns this into text or JSON.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    /// Machine-readable status, matching `Verdict::code`.
    pub status: String,
    /// Human headline. Present in both renderings.
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// Free-form structured payload, e.g. a list of instances.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Report {
    /// A report that is intentionally not ok. Used when a command ran fine and
    /// the answer is "no" — `doctor` found problems, `auth status` found none
    /// configured. The caller must still be able to detect it.
    pub fn negative(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Report {
            ok: false,
            status: "negative".into(),
            summary: summary.into(),
            checks: vec![Check::fail("result", "[STATE]", detail.into())],
            data: None,
        }
    }

    pub fn ok(summary: impl Into<String>) -> Self {
        Report {
            ok: true,
            status: "ok".into(),
            summary: summary.into(),
            checks: Vec::new(),
            data: None,
        }
    }

    pub fn with_checks(summary: impl Into<String>, checks: Vec<Check>) -> Self {
        let ok = checks.iter().all(|c| c.ok);
        Report {
            ok,
            status: if ok { "ok" } else { "negative" }.into(),
            summary: summary.into(),
            checks,
            data: None,
        }
    }

    /// Attach a payload. Takes `Vec` directly because a list is by far the
    /// common case and forcing callers to wrap it in `json!([...])` is noise.
    pub fn with_data(mut self, data: impl Into<serde_json::Value>) -> Self {
        self.data = Some(data.into());
        self
    }
}

/// How to render. Five modes, each answering for every command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// TTY: framed, aligned, colour-free by default.
    #[default]
    Human,
    /// JSON only, nothing before or after.
    Json,
    /// Essential line only.
    Quiet,
}

impl Mode {
    pub fn parse(json: bool, quiet: bool) -> Self {
        match (json, quiet) {
            (true, _) => Mode::Json,
            (false, true) => Mode::Quiet,
            _ => Mode::Human,
        }
    }

    pub fn is_json(self) -> bool {
        self == Mode::Json
    }
}

/// Render a report. In JSON mode this is the ONLY thing that reaches stdout.
pub fn render(report: &Report, mode: Mode) -> String {
    match mode {
        Mode::Json => format!(
            "{}\n",
            serde_json::to_string(report).expect("report is serialisable")
        ),
        Mode::Quiet => {
            if report.ok {
                format!("{}\n", report.summary)
            } else {
                format!("{}\n", failing_summary(report))
            }
        }
        Mode::Human => render_human(report),
    }
}

fn failing_summary(report: &Report) -> String {
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.code.as_str())
        .collect();
    if failed.is_empty() {
        report.summary.clone()
    } else {
        format!("{} ({})", report.summary, failed.join(" "))
    }
}

fn render_human(report: &Report) -> String {
    let mut out = String::new();
    let total = report.checks.len();
    let failed = report.checks.iter().filter(|c| !c.ok).count();

    if !report.checks.is_empty() {
        let width = report
            .checks
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or(0);
        for check in &report.checks {
            let mark = if check.ok { "pass" } else { "FAIL" };
            out.push_str(&format!(
                "{mark}  {:<width$}  {}\n",
                check.name,
                check.detail,
                width = width
            ));
        }
        out.push('\n');
        out.push_str(&format!("{}/{total} checks passed\n", total - failed));
        if failed > 0 {
            out.push('\n');
        }
    }

    out.push_str(if report.ok { "✓ " } else { "✗ " });
    out.push_str(&report.summary);
    out.push('\n');

    if let Some(data) = &report.data {
        if let Some(text) = render_table(data) {
            out.push('\n');
            out.push_str(&text);
        }
    }
    out
}

/// A tiny fixed-width table for list payloads. Column widths come from the
/// data, so a long name never truncates silently — it pushes the row wider.
fn render_table(data: &serde_json::Value) -> Option<String> {
    let rows = data.as_array()?;
    if rows.is_empty() {
        return Some("(nothing to show)".to_string());
    }

    let headers: Vec<String> = rows[0]
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();
    if headers.is_empty() {
        return None;
    }

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            headers
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    let value = row.get(&headers[index]).map(scalar).unwrap_or_default();
                    if index < widths.len() {
                        widths[index] = widths[index].max(value.len());
                    }
                    value
                })
                .collect()
        })
        .collect();

    let mut out = String::new();
    let header: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad(h, widths[i]))
        .collect();
    out.push_str(header.join("  ").trim_end());
    out.push('\n');
    for cell in cells {
        let line: Vec<String> = cell
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    Some(out)
}

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "—".to_string(),
        other => other.to_string(),
    }
}

fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Report {
        Report::with_checks(
            "environment checked",
            vec![
                Check::ok("claude", "[BIN]", "2.1.285"),
                Check::fail("do-token", "[ENV]", "DO_TOKEN is unset"),
            ],
        )
    }

    #[test]
    fn json_mode_emits_one_object() {
        let out = render(&sample(), Mode::Json);
        assert_eq!(
            out.trim().lines().count(),
            1,
            "json output must be one line"
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["status"], "negative");
        assert_eq!(parsed["checks"][1]["code"], "[ENV]");
    }

    #[test]
    fn json_output_ends_with_a_newline() {
        assert!(render(&sample(), Mode::Json).ends_with('\n'));
    }

    #[test]
    fn a_negative_report_is_not_ok() {
        let report = sample();
        assert!(!report.ok);
        assert_eq!(report.status, "negative");
    }

    #[test]
    fn human_mode_shows_every_check_and_a_total() {
        let out = render(&sample(), Mode::Human);
        assert!(out.contains("pass  claude"));
        assert!(out.contains("FAIL  do-token"));
        assert!(out.contains("1/2 checks passed"));
    }

    #[test]
    fn quiet_mode_names_the_failing_codes() {
        let out = render(&sample(), Mode::Quiet);
        assert!(out.contains("[ENV]"), "quiet mode must surface what failed");
        assert!(!out.contains("pass  claude"), "quiet mode drops the noise");
    }

    #[test]
    fn quiet_mode_on_success_is_just_the_summary() {
        let report = Report::ok("all good");
        assert_eq!(render(&report, Mode::Quiet), "all good\n");
    }

    #[test]
    fn mode_parsing_prefers_json() {
        assert_eq!(Mode::parse(true, true), Mode::Json);
        assert_eq!(Mode::parse(false, true), Mode::Quiet);
        assert_eq!(Mode::parse(false, false), Mode::Human);
    }

    #[test]
    fn a_list_payload_renders_as_a_table() {
        let report = Report::ok("2 instances").with_data(serde_json::json!([
            { "name": "alpha", "state": "running" },
            { "name": "beta-with-a-longer-name", "state": "stopped" }
        ]));
        let out = render(&report, Mode::Human);
        assert!(out.contains("alpha"));
        assert!(out.contains("beta-with-a-longer-name"));

        // The longer name must widen the column, never truncate, so every
        // "state" value starts at the same column in every row.
        let state_column = out
            .lines()
            .find(|l| l.starts_with("name"))
            .and_then(|l| l.find("state"))
            .expect("header has a state column");
        for (name, value) in [("alpha", "running"), ("beta-with-a-longer-name", "stopped")] {
            let line = out
                .lines()
                .find(|l| l.starts_with(name))
                .expect("row exists");
            assert_eq!(
                line.find(value),
                Some(state_column),
                "row `{name}` misaligns the state column"
            );
        }
    }

    #[test]
    fn an_empty_list_says_so_instead_of_printing_nothing() {
        let report = Report::ok("none").with_data(serde_json::json!([]));
        assert!(render(&report, Mode::Human).contains("nothing to show"));
    }

    #[test]
    fn null_renders_as_a_dash_not_the_word_null() {
        let report = Report::ok("1").with_data(serde_json::json!([{ "address": null }]));
        assert!(render(&report, Mode::Human).contains('—'));
    }

    #[test]
    fn json_mode_carries_the_data_payload() {
        let report = Report::ok("ok").with_data(serde_json::json!({ "count": 1 }));
        let parsed: serde_json::Value = serde_json::from_str(&render(&report, Mode::Json)).unwrap();
        assert_eq!(parsed["data"]["count"], 1);
    }

    #[test]
    fn empty_checks_are_omitted_from_json() {
        let parsed: serde_json::Value =
            serde_json::from_str(&render(&Report::ok("ok"), Mode::Json)).unwrap();
        assert!(
            parsed.get("checks").is_none(),
            "an empty vector is noise in JSON"
        );
    }
}
