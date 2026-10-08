//! The system prompt, assembled from the workspace the operator curates.
//!
//! Personality files follow the Nerve/OpenClaw pattern: whatever exists in
//! the workspace is injected, whatever is missing is simply not. There is
//! no schema to satisfy — a workspace with only a `MEMORY.md` is a valid
//! workspace.
//!
//! Sections, in prompt order:
//!
//! 1. The core instructions (added by the caller, this crate stays neutral)
//! 2. `IDENTITY.md` — who the agent is
//! 3. `SOUL.md` — how it behaves
//! 4. `USER.md` — who it works for
//! 5. `MEMORY.md` — hot facts, verbatim
//! 6. The skills index — one line per skill (progressive disclosure)
//! 7. Open tasks — pending and in-progress only

use std::path::Path;

use crate::skills::SkillIndex;
use crate::tasks::{TaskStatus, TaskStore};

/// One section of the assembled prompt: a heading and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    pub heading: String,
    pub body: String,
}

fn read_if_present(workspace: &Path, file: &str, heading: &str) -> Option<PromptSection> {
    let text = std::fs::read_to_string(workspace.join(file)).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(PromptSection {
        heading: heading.to_string(),
        body: text.to_string(),
    })
}

/// Collect the workspace sections for the system prompt. Missing files are
/// not errors; a fresh workspace contributes nothing, which is correct.
pub fn system_prompt_sections(workspace: &Path, home: &Path) -> Vec<PromptSection> {
    let mut sections = Vec::new();

    for (file, heading) in [
        ("IDENTITY.md", "Deine Identität"),
        ("SOUL.md", "Deine Art zu arbeiten"),
        ("USER.md", "Über den Menschen, für den du arbeitest"),
        ("MEMORY.md", "Heiße Fakten (kurzfristiges Gedächtnis)"),
    ] {
        if let Some(section) = read_if_present(workspace, file, heading) {
            sections.push(section);
        }
    }

    let skills = SkillIndex::load(workspace, home);
    if !skills.skills.is_empty() {
        let body = skills
            .skills
            .iter()
            .map(|s| {
                if s.description.is_empty() {
                    format!("- {}", s.name)
                } else {
                    format!("- {}: {}", s.name, s.description)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(PromptSection {
            heading: "Verfügbare Skills (vollständigen Text mit skill_read laden)".to_string(),
            body,
        });
    }

    let tasks = TaskStore::new(workspace);
    let open: Vec<_> = [TaskStatus::Pending, TaskStatus::InProgress]
        .iter()
        .flat_map(|status| tasks.list_by_status(*status))
        .collect();
    if !open.is_empty() {
        let body = open
            .iter()
            .map(|t| format!("- [{}] {} ({})", t.status.as_str(), t.title, t.slug))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(PromptSection {
            heading: "Offene Aufgaben".to_string(),
            body,
        });
    }

    sections
}

/// Render the sections into one prompt block, with the headings the model
/// sees. Empty sections are skipped by the assembler, so the rendering has
/// no gaps to explain.
pub fn render_sections(sections: &[PromptSection]) -> String {
    let mut out = String::new();
    for section in sections {
        out.push_str(&format!("## {}\n\n{}\n\n", section.heading, section.body));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bkgclaw-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_workspace_contributes_nothing() {
        let workspace = temp_root("empty");
        let home = temp_root("empty-home");
        assert!(system_prompt_sections(&workspace, &home).is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn present_files_become_sections_in_prompt_order() {
        let workspace = temp_root("full");
        std::fs::write(
            workspace.join("SOUL.md"),
            "Sag die Wahrheit, auch unangenehme.",
        )
        .unwrap();
        std::fs::write(
            workspace.join("IDENTITY.md"),
            "Du bist bkgclaw, ein Coding-Agent.",
        )
        .unwrap();
        std::fs::write(
            workspace.join("MEMORY.md"),
            "Projekt nutzt Rust 2024 Edition.",
        )
        .unwrap();

        let sections = system_prompt_sections(&workspace, Path::new("/nonexistent"));
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(
            headings,
            [
                "Deine Identität",
                "Deine Art zu arbeiten",
                "Heiße Fakten (kurzfristiges Gedächtnis)"
            ]
        );
        let rendered = render_sections(&sections);
        assert!(rendered.contains("Rust 2024"));
        assert!(rendered.contains("## Deine Identität"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn skills_and_open_tasks_get_index_lines() {
        let workspace = temp_root("mixed");
        let skill_dir = workspace.join("skills").join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: Deployment auf den BKG-Server\n---\nsteps",
        )
        .unwrap();
        let tasks = TaskStore::new(&workspace);
        tasks.add("ship-it", "Release bauen", "details").unwrap();

        let sections = system_prompt_sections(&workspace, Path::new("/nonexistent"));
        let rendered = render_sections(&sections);
        assert!(rendered.contains("deploy: Deployment auf den BKG-Server"));
        assert!(rendered.contains("[pending] Release bauen"));

        // A done task does not appear: the prompt lists work, not history.
        tasks.set_status("ship-it", TaskStatus::Done).unwrap();
        let sections = system_prompt_sections(&workspace, Path::new("/nonexistent"));
        assert!(!render_sections(&sections).contains("Release bauen"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn empty_files_are_skipped_not_injected_as_noise() {
        let workspace = temp_root("blank");
        std::fs::write(workspace.join("SOUL.md"), "   \n").unwrap();
        let sections = system_prompt_sections(&workspace, Path::new("/nonexistent"));
        assert!(sections.is_empty(), "a blank file has nothing to say");
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
