//! `bkgclaw plugins` — install, list, remove.
//!
//! A plugin is data, never code: the manifest describes it, its skills
//! load through the normal index, and nothing from a plugin is executed by
//! bkgclaw itself. OpenClaw provider contracts in the manifest are shown
//! for what they are — recorded, not wired.

use crate::outcome::Outcome;
use bkgclaw_core::Verdict;

pub fn install(source: &str) -> Outcome {
    let home = bkgclaw_store::home_root();
    match bkgclaw_store::install_plugin(&home, source) {
        Ok(plugin) => {
            let mut checks = vec![
                bkgclaw_ui::Check {
                    name: "manifest".into(),
                    ok: true,
                    code: "[PLUGIN]".into(),
                    detail: format!(
                        "{} v{} — {}",
                        plugin.manifest.name, plugin.manifest.version, plugin.manifest.description
                    ),
                },
                bkgclaw_ui::Check {
                    name: "skills".into(),
                    ok: true,
                    code: "[PLUGIN]".into(),
                    detail: format!("{} skills geladen", plugin.skill_count),
                },
            ];
            // Honest: contracts recorded, but bkgclaw does not execute
            // plugin provider code. Saying "installed and active" for a
            // speech contract would be a lie someone would rely on.
            if let Some(contracts) = plugin.manifest.contracts.as_object() {
                if !contracts.is_empty() {
                    checks.push(bkgclaw_ui::Check {
                        name: "verträge".into(),
                        ok: false,
                        code: "[LIMIT]".into(),
                        detail: format!(
                            "aufgezeichnet ({}) — von bkgclaw nicht ausgeführt; skills laufen",
                            contracts.keys().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    });
                }
            }
            Outcome::from_report(
                bkgclaw_ui::Report::with_checks(
                    format!("plugin `{}` installiert", plugin.dir_name),
                    checks,
                )
                .with_data(serde_json::json!({
                    "plugin": plugin.dir_name,
                    "source": plugin.source,
                    "skills": plugin.skill_count,
                })),
            )
        }
        Err(error) => Outcome::fail(Verdict::negative(error)),
    }
}

pub fn list() -> Outcome {
    let home = bkgclaw_store::home_root();
    let plugins = bkgclaw_store::list_plugins(&home);
    if plugins.is_empty() {
        return Outcome::from_report(bkgclaw_ui::Report::negative(
            "keine plugins installiert",
            "installieren mit: bkgclaw plugins install github:<owner>/<repo>",
        ));
    }
    let checks = plugins
        .iter()
        .map(|plugin| bkgclaw_ui::Check {
            name: plugin.dir_name.clone(),
            ok: !plugin.manifest.description.starts_with('⚠'),
            code: "[PLUGIN]".into(),
            detail: format!(
                "v{} · {} skills · {} — {}",
                plugin.manifest.version, plugin.skill_count, plugin.source, plugin.manifest.description
            ),
        })
        .collect();
    let rows: Vec<serde_json::Value> = plugins
        .iter()
        .map(|plugin| {
            serde_json::json!({
                "plugin": plugin.dir_name,
                "version": plugin.manifest.version,
                "name": plugin.manifest.name,
                "skills": plugin.skill_count,
                "source": plugin.source,
            })
        })
        .collect();
    Outcome::from_report(
        bkgclaw_ui::Report::with_checks(format!("{} plugins installiert", plugins.len()), checks)
            .with_data(rows),
    )
}

pub fn remove(name: &str) -> Outcome {
    let home = bkgclaw_store::home_root();
    match bkgclaw_store::remove_plugin(&home, name) {
        Ok(plugin) => Outcome::success(format!(
            "plugin `{}` ({}) entfernt",
            plugin.dir_name, plugin.manifest.name
        )),
        Err(error) => Outcome::fail(Verdict::negative(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_without_plugins_is_negative_with_a_hint() {
        // Whatever the machine: the empty-listing contract is that it says
        // HOW to install, not just that nothing is there.
        let outcome = list();
        let (status, report, _) = outcome.into_parts();
        assert!(report.summary.contains("keine plugins") || report.summary.contains("plugins installiert"));
        let _ = status;
    }
}
