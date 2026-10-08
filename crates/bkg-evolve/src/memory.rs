//! The attempt archive: evolution's memory, in files.
//!
//! Every attempt — improved, suspect or reverted — is a JSONL line in
//! `<home>/evolve/attempts.jsonl` with its goal, branch, fitness and a
//! one-line lesson. The next attempt's briefing carries the recent
//! lessons, so a failed approach is not retried blind (the loop guard
//! inside the run catches the exact repeat; the memory catches the
//! *idea* repeat).
//!
//! `journal.md` is the human-readable twin, regenerated after every
//! attempt — the yoyo "entire history in the journal" idea, except the
//! journal lives on this machine and in the repo, not on a website.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fitness::{FitnessReport, Verdict};

/// One evolution attempt, as recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: u64,
    /// Seconds since the epoch.
    pub ts: u64,
    pub goal: String,
    /// The `evolve/NNN-slug` branch the attempt ran on.
    pub branch: String,
    pub verdict: Verdict,
    pub fitness: FitnessReport,
    #[serde(default)]
    pub lesson: String,
    #[serde(default)]
    pub origin: String,
    /// Why the agent turn ended — "Answered" after one turn without tool
    /// calls is the honest signature of a model that talked instead of
    /// worked, and the journal must say so.
    #[serde(default)]
    pub stop_reason: String,
    /// How many model responses the attempt took.
    #[serde(default)]
    pub turns: u32,
    /// The agent's own final words — evidence for the operator, never the
    /// verdict.
    #[serde(default)]
    pub answer: String,
}

/// The archive for one home.
pub struct AttemptArchive {
    path: PathBuf,
}

impl AttemptArchive {
    pub fn new(home: &Path) -> Self {
        let dir = home.join("evolve");
        AttemptArchive { path: dir.join("attempts.jsonl") }
    }

    fn entries(&self) -> Vec<Attempt> {
        std::fs::read_to_string(&self.path)
            .ok()
            .map(|text| {
                text.lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The newest attempt id — attempts are numbered globally, like the
    /// snapshots, so a branch name and an archive id always match up.
    pub fn next_id(&self) -> u64 {
        self.entries().last().map(|a| a.id + 1).unwrap_or(1)
    }

    /// Record one attempt. The JSONL line is the truth; the journal is
    /// derived, never edited by hand.
    pub fn record(&self, attempt: Attempt) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let line = serde_json::to_string(&attempt).map_err(|e| e.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        use std::io::Write;
        file.write_all(format!("{line}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        self.regenerate_journal()
    }

    pub fn all(&self) -> Vec<Attempt> {
        self.entries()
    }

    /// The most recent attempts, oldest first — the order the briefing
    /// wants them in.
    pub fn recent(&self, count: usize) -> Vec<Attempt> {
        let mut entries = self.entries();
        if entries.len() > count {
            entries.drain(..entries.len() - count);
        }
        entries
    }

    /// The lessons as prompt lines. Failures carry their lesson verbatim;
    /// successes just say they succeeded — no need to re-teach what worked.
    pub fn briefing_lines(&self, count: usize) -> Vec<String> {
        self.recent(count)
            .into_iter()
            .map(|attempt| {
                let verdict = attempt.verdict.as_str();
                match attempt.verdict {
                    Verdict::Reverted | Verdict::Suspect => format!(
                        "- versuch {} ({verdict}, {}/{} tests): {}",
                        attempt.id, attempt.fitness.tests_passed, attempt.fitness.tests_failed, attempt.lesson
                    ),
                    _ => format!(
                        "- versuch {} ({verdict}): {}",
                        attempt.id, attempt.lesson
                    ),
                }
            })
            .collect()
    }

    /// Regenerate `evolve/journal.md` from the archive.
    pub fn regenerate_journal(&self) -> Result<(), String> {
        let dir = self.path.parent().ok_or("kein journal-verzeichnis")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let attempts = self.entries();
        let mut journal = String::from(
            "# Evolutionstagebuch\n\n\
             Jeder Versuch, bkgclaw zu verbessern — Ziel, Urteil, Fitness, Lehre. \
             Diese Datei wird nach jedem Versuch neu erzeugt; die Quelle ist \
             `attempts.jsonl`.\n\n",
        );
        for attempt in attempts.iter().rev() {
            journal.push_str(&format!(
                "## Versuch {id:03} — {verdict}\n\n\
                 - **ziel**: {goal}\n\
                 - **branch**: `{branch}`\n\
                 - **fitness**: build {build}, {passed} tests ({failed} failed), \
                 {clippy} clippy-warnungen, {secs}s\n\
                 - **lehre**: {lesson}\n\
                 - **agent**: {turns} runde(n), stopp {stop_reason}\n\n",
                id = attempt.id,
                verdict = attempt.verdict.as_str(),
                goal = attempt.goal,
                branch = attempt.branch,
                build = if attempt.fitness.build_ok { "ok" } else { "kaputt" },
                passed = attempt.fitness.tests_passed,
                failed = attempt.fitness.tests_failed,
                clippy = attempt.fitness.clippy_warnings,
                secs = attempt.fitness.duration_secs,
                lesson = attempt.lesson,
                turns = attempt.turns,
                stop_reason = attempt.stop_reason,
            ));
        }
        std::fs::write(dir.join("journal.md"), journal).map_err(|e| e.to_string())
    }
}

/// The one-line lesson an attempt leaves behind. Derived from the
/// verdict + fitness — the model's own summary is deliberately NOT the
/// lesson; the numbers are.
pub fn lesson_for(goal: &str, verdict: Verdict, fitness: &FitnessReport) -> String {
    match verdict {
        Verdict::Improved => format!(
            "{goal}: verbessert — {} tests, {} clippy-warnungen.",
            fitness.tests_passed, fitness.clippy_warnings
        ),
        Verdict::Neutral => format!(
            "{goal}: grün, aber ohne messbaren gewinn ({} tests, {} warnungen).",
            fitness.tests_passed, fitness.clippy_warnings
        ),
        Verdict::Suspect => format!(
            "{goal}: grün, aber verdächtig — tests oder warnungen schlechter als die basis.",
        ),
        Verdict::Reverted => format!(
            "{goal}: zurückgerollt — {}.",
            if !fitness.build_ok {
                "bauen kaputt".to_string()
            } else if fitness.tests_failed > 0 {
                format!("{} test(s) rot", fitness.tests_failed)
            } else {
                // cargo test exited non-zero without counting a single
                // test: the test build itself failed. "0 rot" would read
                // as "nothing was wrong".
                "test-bau schlug fehl (kein test lief)".to_string()
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        dir
    }

    fn attempt(id: u64, goal: &str, verdict: Verdict, lesson: &str) -> Attempt {
        Attempt {
            id,
            ts: 1,
            goal: goal.to_string(),
            branch: format!("evolve/{id:04}-x"),
            verdict,
            fitness: FitnessReport {
                build_ok: true,
                tests_ok: true,
                tests_passed: 10,
                tests_failed: if verdict == Verdict::Reverted { 2 } else { 0 },
                clippy_warnings: 3,
                duration_secs: 9,
                failure_excerpt: String::new(),
            },
            lesson: lesson.to_string(),
            origin: String::new(),
            stop_reason: "Answered".to_string(),
            turns: 1,
            answer: String::new(),
        }
    }

    #[test]
    fn ids_are_global_and_the_briefing_repeats_failures() {
        let home = home();
        let archive = AttemptArchive::new(&home);
        assert_eq!(archive.next_id(), 1);
        archive.record(attempt(1, "schneller bauen", Verdict::Reverted, "cargo build hängt an linkzeit")).unwrap();
        assert_eq!(archive.next_id(), 2);
        archive.record(attempt(2, "schneller bauen", Verdict::Improved, "inkrementell")).unwrap();

        let briefing = archive.briefing_lines(5);
        assert!(briefing[0].contains("versuch 1 (reverted, 10/2 tests)"), "erster eintrag: {briefing:#?}");
        assert!(briefing[0].contains("linkzeit"), "fehlversuche lehren");
        assert!(briefing[1].contains("versuch 2 (improved)"));

        // The journal exists and tells both stories.
        let journal = std::fs::read_to_string(home.join("evolve").join("journal.md")).unwrap();
        assert!(journal.contains("Versuch 002 — improved"));
        assert!(journal.contains("Versuch 001 — reverted"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lessons_state_the_numbers_not_the_mood() {
        let fitness = FitnessReport {
            build_ok: false,
            ..Default::default()
        };
        let lesson = lesson_for("refaktor", Verdict::Reverted, &fitness);
        assert!(lesson.contains("bauen kaputt"), "{lesson}");
        assert!(!lesson.contains("tut mir leid"));
    }
}
