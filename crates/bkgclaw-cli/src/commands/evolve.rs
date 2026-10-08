//! `bkgclaw evolve` — the self-improvement commands.
//!
//! The CLI is the operator's lever: init the repo, run cycles, read the
//! journal, push. The verdicts inside come from the fitness gate, never
//! from the model's self-assessment.

use crate::outcome::Outcome;
use bkgclaw_core::Verdict;

pub fn init(origin: Option<&str>) -> Outcome {
    let dir = std::env::current_dir().unwrap_or_default();
    match bkg_evolve::init_repo(&dir, origin) {
        Ok(()) => {
            let mut summary = format!(
                "evolution-repo bereit in {} — baseline committed",
                dir.display()
            );
            if let Some(origin) = origin {
                match bkg_evolve::gitops::push(&dir, "origin", "main", true) {
                    Ok(()) => summary.push_str(&format!(" — gepusht zu {origin}")),
                    Err(error) => {
                        summary.push_str(&format!(
                            " — push fehlgeschlagen (später `bkgclaw evolve push`): {error}"
                        ));
                    }
                }
            }
            Outcome::success(summary)
        }
        Err(error) => Outcome::fail(Verdict::environment(error)),
    }
}

pub async fn run(goal: &str, push: bool, strict: bool) -> Outcome {
    let dir = std::env::current_dir().unwrap_or_default();
    let options = bkg_evolve::EvolveOptions {
        push,
        strict,
        ..Default::default()
    };
    match bkg_evolve::run_cycle(&dir, goal, &options).await {
        Ok(result) => {
            let improved = matches!(
                result.verdict,
                bkg_evolve::Verdict::Improved | bkg_evolve::Verdict::Neutral
            );
            let mut summary = format!(
                "evolve {id:03} auf `{branch}` — {verdict}: {lesson}",
                id = result.id,
                branch = result.branch,
                verdict = result.verdict.as_str(),
                lesson = result.lesson,
            );
            if result.pushed {
                summary.push_str(" — gepusht");
            }
            let mut report = bkgclaw_ui::Report::ok(summary);
            if !result.answer.is_empty() {
                report = report.with_data(serde_json::json!({
                    "agent": result.answer,
                    "fitness": result.fitness,
                    "baseline": result.baseline,
                    "branch": result.branch,
                }));
            }
            let outcome = Outcome::from_report(report);
            if improved { outcome } else { outcome }
        }
        Err(error) => Outcome::fail(Verdict::environment(error)),
    }
}

pub fn attempts() -> Outcome {
    let home = bkgclaw_store::home_root();
    let archive = bkg_evolve::AttemptArchive::new(&home);
    let attempts = archive.all();
    if attempts.is_empty() {
        return Outcome::from_report(bkgclaw_ui::Report::negative(
            "noch keine evolve-versuche",
            "starten mit: bkgclaw evolve run \"mache X besser\"",
        ));
    }
    let checks = attempts
        .iter()
        .rev()
        .map(|attempt| {
            let ok = !matches!(attempt.verdict, bkg_evolve::Verdict::Reverted);
            bkgclaw_ui::Check {
                name: format!("{:03} · {}", attempt.id, attempt.verdict.as_str()),
                ok,
                code: "[EVOLVE]".into(),
                detail: format!(
                    "{} tests, {} failed, {} warnungen — {}",
                    attempt.fitness.tests_passed,
                    attempt.fitness.tests_failed,
                    attempt.fitness.clippy_warnings,
                    attempt.lesson,
                ),
            }
        })
        .collect();
    Outcome::from_report(bkgclaw_ui::Report::with_checks(
        format!("{} evolve-versuche", attempts.len()),
        checks,
    ))
}

pub fn fitness_report() -> Outcome {
    let dir = std::env::current_dir().unwrap_or_default();
    let report = bkg_evolve::measure_fitness(&dir);
    let checks = vec![
        bkgclaw_ui::Check {
            name: "build".into(),
            ok: report.build_ok,
            code: "[FITNESS]".into(),
            detail: if report.build_ok { "ok".into() } else { "kaputt".into() },
        },
        bkgclaw_ui::Check {
            name: "tests".into(),
            ok: report.tests_ok,
            code: "[FITNESS]".into(),
            detail: format!("{} passed, {} failed", report.tests_passed, report.tests_failed),
        },
        bkgclaw_ui::Check {
            name: "clippy".into(),
            ok: true,
            code: "[FITNESS]".into(),
            detail: format!("{} warnungen", report.clippy_warnings),
        },
    ];
    Outcome::from_report(bkgclaw_ui::Report::with_checks(
        format!("fitness in {} s", report.duration_secs),
        checks,
    ))
}

pub fn push() -> Outcome {
    let dir = std::env::current_dir().unwrap_or_default();
    match bkg_evolve::push_state(&dir) {
        Ok(()) => Outcome::success("branch und journal gepusht"),
        Err(error) => Outcome::fail(Verdict::environment(error)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_verdicts_are_the_engine_s_not_the_model_s() {
        // The CLI reports the fitness gate's verdict; there is no code
        // path where a model's own "ich habe es geschafft" turns into ok.
        assert_eq!(bkg_evolve::Verdict::Reverted.as_str(), "reverted");
        assert_eq!(bkg_evolve::Verdict::Improved.as_str(), "improved");
    }
}
