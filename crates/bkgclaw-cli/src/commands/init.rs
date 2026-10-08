//! `bkgclaw init` — scaffold the workspace the system prompt reads.
//!
//! Two modes, two philosophies (the Nerve pattern): a *personal* assistant
//! that accumulates identity and user context over time, and a *worker*
//! with a mission, no personality. Both create the same files; only the
//! content differs, because the agent itself is the same engine.

use crate::outcome::Outcome;
use bkgclaw_core::Verdict;

pub fn run(mode: &str) -> Outcome {
    let (personal, soul, identity, user) = match mode {
        "personal" => templates_personal(),
        "worker" => templates_worker(),
        other => {
            return Outcome::fail(Verdict::usage(format!(
                "`{other}` is not a mode — use personal or worker"
            )));
        }
    };

    let workspace = bkgclaw_store::workspace_root();
    let created = std::fs::create_dir_all(&workspace);
    if let Err(error) = created {
        return Outcome::fail(Verdict::environment(format!(
            "cannot create {}: {error}",
            workspace.display()
        )));
    }

    // Never overwrite: an operator's curated files are worth more than a
    // fresh template. Missing files get created; existing ones are listed
    // as skipped.
    let mut checks = Vec::new();
    let mut rows = Vec::new();
    for (name, body) in [
        ("SOUL.md", Some(soul)),
        ("IDENTITY.md", Some(identity)),
        ("USER.md", Some(user)),
        ("MEMORY.md", Some(personal)),
    ] {
        let path = workspace.join(name);
        let detail = if path.exists() {
            "vorhanden — unangetastet".to_string()
        } else {
            match std::fs::write(&path, body.expect("template present")) {
                Ok(()) => "erstellt".to_string(),
                Err(error) => format!("FEHLER: {error}"),
            }
        };
        checks.push(bkgclaw_ui::Check {
            name: name.to_string(),
            ok: !detail.starts_with("FEHLER"),
            code: "[FILE]".into(),
            detail: detail.clone(),
        });
        rows.push(serde_json::json!({ "file": name, "result": detail }));
    }

    // The skill and task directories exist from now on, empty.
    for dir in ["skills", "tasks"] {
        let path = workspace.join(dir);
        if std::fs::create_dir_all(&path).is_ok() {
            checks.push(bkgclaw_ui::Check {
                name: dir.to_string(),
                ok: true,
                code: "[DIR]".into(),
                detail: "bereit".into(),
            });
        }
    }

    let summary = format!("workspace {} im Modus {mode}", workspace.display());
    Outcome::from_report(bkgclaw_ui::Report::with_checks(summary, checks).with_data(rows))
}

fn templates_personal() -> (String, String, String, String) {
    let memory = "# Heiße Fakten\n\nAlles hier steht in jedem System-Prompt. Kurz halten, Datum dran, Altes räumen.\n\n- (noch nichts)\n".to_string();
    let soul = "# Seele\n\nDu arbeitest für einen Menschen, nicht für ein Ticket.\n\n- Erinnere dich an Vorlieben und wiedersprich ihnen nicht lautlos.\n- Bringe frühere Gespräche von selbst zur Sprache, wenn sie relevant sind.\n- Sag, wenn du etwas nicht weißt. Eine fundierte Vermutung ist nur als solche einen Satz wert.\n- Entwickle Meinungen, aber begründe sie aus Fakten im Gedächtnis, nicht aus Laune.\n".to_string();
    let identity = "# Identität\n\nDu bist bkgclaw, der persönliche Agent dieses Rechners. Du begleitest Projekte über Monate, nicht über einen Chat.\n".to_string();
    let user =
        "# Über den Menschen\n\n- Name: (eintragen)\n- Sprache: Deutsch\n- Wichtig: (eintragen)\n"
            .to_string();
    (memory, soul, identity, user)
}

fn templates_worker() -> (String, String, String, String) {
    // Same four slots; the worker gets no identity and no user file — a
    // mission, not a relationship.
    let memory = "# Operatives Gedächtnis\n\nMuster, Prozeduren, Entscheidungen, Freigaben. Keine Privates.\n\n- (noch nichts)\n".to_string();
    let soul = "# Seele\n\nDu bist ein Spezialist, kein Skript.\n\n- Plane, bevor du handelst; schreibe den Plan als Aufgabe, wenn er mehr als drei Schritte hat.\n- Prüfe nach jedem Schritt; ein Schritt ohne Prüfung ist nicht fertig.\n- Logge jede Entscheidung in memory, damit ein anderer Arbeiter weitermachen kann.\n- Stelle keine Fragen, die du aus dem Code beantworten kannst.\n".to_string();
    let identity = "# Mission\n\n(beschreibe die Aufgabe in einem Satz — der Arbeiter lädt sich bei Bedarf selbst auf)\n".to_string();
    let user =
        "# Nutzer\n\nArbeiter-Modus: kein persönlicher Kontext, nur Aufträge und Freigaben.\n"
            .to_string();
    (memory, soul, identity, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_mode_is_a_usage_error() {
        let outcome = run("chef");
        let (status, _, _) = outcome.into_parts();
        assert!(status.is_err_and(|v| v.exit_code() == 2));
    }

    #[test]
    fn the_templates_carry_their_modes_philosophy() {
        let (memory, soul, identity, user) = templates_personal();
        assert!(soul.contains("Mensch"));
        assert!(user.contains("Name"));
        assert!(memory.contains("Heiße Fakten"));
        assert!(identity.contains("persönliche"));

        let (memory, soul, identity, user) = templates_worker();
        assert!(soul.contains("Spezialist"));
        assert!(memory.contains("Operatives"));
        assert!(identity.contains("Mission"));
        assert!(user.contains("kein persönlicher"));
    }
}
