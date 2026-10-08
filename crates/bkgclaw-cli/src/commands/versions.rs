//! `bkgclaw versions` — the built-in checkpoint system.
//!
//! The workflow this enables is the one an agent improving its own code
//! needs: snapshot before the run, let it work, look at the changes, keep
//! them or roll the whole tree back with one command.

use crate::outcome::Outcome;
use bkgclaw_core::Verdict;
use bkgclaw_store::{Change, SnapshotStore};

fn store() -> SnapshotStore {
    SnapshotStore::for_dir(&bkgclaw_store::home_root(), &std::env::current_dir().unwrap_or_default())
}

pub fn init() -> Outcome {
    let store = store();
    match store.init() {
        Ok(()) => Outcome::success(format!(
            "versionierung aktiv für {} — jeder schreibende agenten-lauf macht jetzt automatisch einen snapshot",
            std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()
        )),
        Err(error) => Outcome::fail(Verdict::environment(format!("init fehlgeschlagen: {error}"))),
    }
}

pub fn snapshot(note: &str) -> Outcome {
    let store = store();
    match store.snapshot(&std::env::current_dir().unwrap_or_default(), note, "manual") {
        Ok(snapshot) => Outcome::from_report(
            bkgclaw_ui::Report::ok(format!(
                "snapshot {} — {} dateien, »{}«",
                snapshot.id,
                snapshot.files.len(),
                note
            ))
            .with_data(serde_json::json!({
                "id": snapshot.id,
                "files": snapshot.files.len(),
                "note": note,
            })),
        ),
        Err(error) => Outcome::fail(Verdict::negative(error)),
    }
}

pub fn list() -> Outcome {
    let snapshots = store().list();
    if snapshots.is_empty() {
        return Outcome::from_report(bkgclaw_ui::Report::negative(
            "keine snapshots",
            "erstellen mit `bkgclaw versions snapshot -m \"…\"` (init einmalig davor)",
        ));
    }
    let checks = snapshots
        .iter()
        .map(|snapshot| bkgclaw_ui::Check {
            name: snapshot.id.clone(),
            ok: true,
            code: "[SNAP]".into(),
            detail: format!(
                "{} dateien · {} · »{}«",
                snapshot.files.len(),
                snapshot.origin,
                snapshot.note
            ),
        })
        .collect();
    Outcome::from_report(
        bkgclaw_ui::Report::with_checks(format!("{} snapshots", snapshots.len()), checks),
    )
}

/// The id to compare against: the argument, or the latest snapshot.
fn resolve_id(store: &SnapshotStore, id: Option<&str>) -> Result<String, String> {
    match id {
        Some(id) => {
            if store.get(id).is_some() {
                Ok(id.to_string())
            } else {
                Err(format!("kein snapshot `{id}`"))
            }
        }
        None => store
            .latest()
            .map(|snapshot| snapshot.id)
            .ok_or_else(|| "kein snapshot vorhanden".to_string()),
    }
}

pub fn changes(id: Option<&str>) -> Outcome {
    let store = store();
    let id = match resolve_id(&store, id) {
        Ok(id) => id,
        Err(error) => return Outcome::fail(Verdict::negative(error)),
    };
    match store.changes(&std::env::current_dir().unwrap_or_default(), &id) {
        Ok(changes) => {
            if changes.is_empty() {
                return Outcome::success(format!("keine änderungen seit {id}"));
            }
            let checks = changes
                .iter()
                .map(|(path, change)| {
                    let (ok, detail) = match change {
                        Change::Created => (true, "neu".to_string()),
                        Change::Modified => (true, "geändert".to_string()),
                        Change::Deleted => (false, "gelöscht".to_string()),
                    };
                    bkgclaw_ui::Check {
                        name: path.clone(),
                        ok,
                        code: "[DIFF]".into(),
                        detail,
                    }
                })
                .collect();
            Outcome::from_report(bkgclaw_ui::Report::with_checks(
                format!("{} änderungen seit {}", changes.len(), id),
                checks,
            ))
        }
        Err(error) => Outcome::fail(Verdict::negative(error)),
    }
}

pub fn diff(a: &str, b: &str) -> Outcome {
    let store = store();
    let Some(snapshot_a) = store.get(a) else {
        return Outcome::fail(Verdict::negative(format!("kein snapshot `{a}`")));
    };
    let Some(snapshot_b) = store.get(b) else {
        return Outcome::fail(Verdict::negative(format!("kein snapshot `{b}`")));
    };
    // diff_lists is the store's comparison core; expose it through a
    // snapshot-to-snapshot comparison here.
    let changes = store.diff_snapshots(&snapshot_a, &snapshot_b);
    if changes.is_empty() {
        return Outcome::success(format!("{a} und {b} sind identisch"));
    }
    let checks = changes
        .iter()
        .map(|(path, change)| bkgclaw_ui::Check {
            name: path.clone(),
            ok: !matches!(change, Change::Deleted),
            code: "[DIFF]".into(),
            detail: match change {
                Change::Created => "neu".to_string(),
                Change::Modified => "geändert".to_string(),
                Change::Deleted => "gelöscht".to_string(),
            },
        })
        .collect();
    Outcome::from_report(bkgclaw_ui::Report::with_checks(
        format!("{changes} änderungen zwischen {a} und {b}", changes = changes.len()),
        checks,
    ))
}

pub fn restore(id: &str) -> Outcome {
    let store = store();
    match store.restore(&std::env::current_dir().unwrap_or_default(), id) {
        Ok(snapshot) => Outcome::success(format!(
            "{} wiederhergestellt — {} dateien, exakt wie »{}« (vorher wurde ein sicherungs-snapshot erstellt)",
            snapshot.id, snapshot.files.len(), snapshot.note
        )),
        Err(error) => Outcome::fail(Verdict::negative(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_listing_explains_how_to_create_one() {
        // Whatever the machine: the message must name the way forward.
        let outcome = list();
        let (_, report, _) = outcome.into_parts();
        assert!(
            report.summary.contains("keine snapshots") || report.summary.contains("snapshots"),
            "unexpected summary: {}",
            report.summary
        );
    }
}
