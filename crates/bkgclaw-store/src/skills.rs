//! Skills: markdown procedures the agent loads on demand.
//!
//! A skill is a directory under `<workspace>/skills/<name>/` (or
//! `<home>/skills/<name>/` for machine-wide skills) with a `SKILL.md`. The
//! index parses only name and description from the frontmatter — the
//! Nerve/OpenClaw "progressive disclosure" rule: the system prompt carries
//! the one-line description, the full text is one `skill_read` call away.
//!
//! Frontmatter parsing is deliberately tiny: `name:` and `description:`
//! lines between `---` markers. A full YAML parser for two keys would be a
//! dependency for no behaviour.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One indexed skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Directory the SKILL.md lives in, for reading the full text.
    pub dir: PathBuf,
    /// Which root the skill came from: workspace skills shadow home skills
    /// of the same name, exactly like project files shadow global ones.
    pub source: SkillSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Workspace,
    Home,
}

/// Parsed frontmatter, the only part that enters a system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillIndex {
    pub skills: Vec<Skill>,
}

/// Extract `name:` and `description:` from a SKILL.md frontmatter block.
/// Missing frontmatter means the skill is listed with its directory name —
/// a skill that documents nothing still works, it just sells itself badly.
pub fn parse_frontmatter(text: &str, fallback_name: &str) -> (String, String) {
    let mut name = String::new();
    let mut description = String::new();
    let mut in_frontmatter = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if in_frontmatter {
                break; // end of frontmatter; body follows
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name:") {
            name = rest.trim().to_string();
        } else if let Some(rest) = trimmed.strip_prefix("description:") {
            description = rest.trim().to_string();
        }
    }
    let name = if name.is_empty() {
        fallback_name.to_string()
    } else {
        name
    };
    (name, description)
}

impl SkillIndex {
    /// Build the index by scanning workspace, home, installed plugins and
    /// the extra roots from `BKGCLAW_SKILLS_PATHS` (colon-separated).
    /// Precedence: workspace shadows home, home shadows plugins, plugins
    /// shadow raw extra roots. Unreadable directories are skipped, not
    /// fatal: one broken skill must not blind the agent to the rest.
    pub fn load(workspace: &Path, home: &Path) -> Self {
        let mut skills: Vec<Skill> = Vec::new();
        // Bare markdown (styles, personas) is a plugin/extras concept:
        // workspace and home roots hold personality and memory files that
        // must never double-load as skills.
        for root in extra_roots() {
            scan_root(&root, SkillSource::Home, &mut skills, true);
        }
        for root in plugin_roots(home) {
            scan_root(&root, SkillSource::Home, &mut skills, true);
        }
        scan_root(home, SkillSource::Home, &mut skills, false);
        scan_root(workspace, SkillSource::Workspace, &mut skills, false);

        // Last writer wins per name.
        let mut unique: Vec<Skill> = Vec::new();
        for skill in skills {
            if let Some(existing) = unique.iter_mut().find(|s| s.name == skill.name) {
                *existing = skill;
            } else {
                unique.push(skill);
            }
        }
        unique.sort_by(|a, b| a.name.cmp(&b.name));
        SkillIndex { skills: unique }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// The full text of one skill, for `skill_read`.
    pub fn read(&self, name: &str) -> Option<String> {
        let skill = self.get(name)?;
        if skill.dir.is_file() {
            // A bare-markdown skill: the file itself is the whole text.
            return std::fs::read_to_string(&skill.dir).ok();
        }
        std::fs::read_to_string(skill.dir.join("SKILL.md")).ok()
    }

    /// Every SKILL.md may also carry references: files the agent can read
    /// when the skill's own text points to them.
    pub fn read_reference(&self, name: &str, relative: &str) -> Option<String> {
        let skill = self.get(name)?;
        // No traversal: a reference path may not escape the skill directory.
        if relative.contains("..") || relative.starts_with('/') {
            return None;
        }
        std::fs::read_to_string(skill.dir.join(relative)).ok()
    }
}

/// Extra skill roots from the environment (`BKGCLAW_SKILLS_PATHS`,
/// colon-separated): BKG packages with their own `skills/` directory — or
/// any directory of bare `.md` files — load without copying anything.
pub fn extra_roots() -> Vec<PathBuf> {
    std::env::var("BKGCLAW_SKILLS_PATHS")
        .ok()
        .map(|value| {
            value
                .split(':')
                .filter(|p| !p.trim().is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Skill roots of installed plugins: `<home>/plugins/<name>/`. The
/// plugin installer populates the directory; this makes its skills (and
/// bare markdown files) loadable with no configuration.
pub fn plugin_roots(home: &Path) -> Vec<PathBuf> {
    let dir = home.join("plugins");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect()
}

fn scan_root(root: &Path, source: SkillSource, skills: &mut Vec<Skill>, allow_bare: bool) {
    let skills_dir = root.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        // A root without skills/ can still hold bare markdown skills.
        if allow_bare {
            scan_bare_markdown(root, source, skills);
        }
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let file = dir.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let fallback = entry.file_name().to_string_lossy().to_string();
        let (name, description) = parse_frontmatter(&text, &fallback);
        skills.push(Skill {
            name,
            description,
            dir,
            source,
        });
    }
    // A plugin may ship both layouts: skills/ folders AND loose style or
    // persona files.
    if allow_bare {
        scan_bare_markdown(root, source, skills);
    }
}

/// Bare markdown files as skills: `<root>/<name>.md`, the whole file being
/// the instruction text. A directory of writing styles or personas looks
/// exactly like this, and wrapping each into a SKILL.md shell would be
/// busywork the agent does not need.
fn scan_bare_markdown(root: &Path, source: SkillSource, skills: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        // Repository meta files are documentation, not instructions.
        if matches!(
            file_name.to_ascii_uppercase().as_str(),
            "README.MD" | "CHANGELOG.MD" | "LICENSE.MD" | "CONTRIBUTING.MD" | "PLAN.MD"
        ) || file_name.starts_with('.')
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let fallback = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let (name, mut description) = parse_frontmatter(&text, &fallback);
        if description.is_empty() {
            // No frontmatter: the first content line sells the skill in
            // the index.
            description = text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
        }
        skills.push(Skill { name, description, dir: path, source });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bkgclaw-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, dir: &str, frontmatter: &str, body: &str) {
        let dir_path = root.join("skills").join(dir);
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(
            dir_path.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn frontmatter_keys_are_extracted() {
        let (name, description) = parse_frontmatter(
            "---\nname: git-flow\ndescription: How we branch\n---\nbody",
            "fallback",
        );
        assert_eq!(name, "git-flow");
        assert_eq!(description, "How we branch");
    }

    #[test]
    fn missing_frontmatter_falls_back_to_the_directory_name() {
        let (name, description) = parse_frontmatter("just a body", "my-dir");
        assert_eq!(name, "my-dir");
        assert_eq!(description, "");
    }

    #[test]
    fn the_index_lists_skills_and_workspace_shadows_home() {
        let home = temp_root("home");
        let workspace = temp_root("work");
        write_skill(
            &home,
            "deploy",
            "name: deploy\ndescription: old way",
            "home body",
        );
        write_skill(
            &workspace,
            "deploy",
            "name: deploy\ndescription: new way",
            "workspace body",
        );
        write_skill(
            &workspace,
            "review",
            "name: review\ndescription: code review",
            "review body",
        );

        let index = SkillIndex::load(&workspace, &home);
        assert_eq!(index.skills.len(), 2);
        assert_eq!(index.get("deploy").unwrap().description, "new way");
        assert_eq!(index.get("review").unwrap().description, "code review");
        assert_eq!(
            index.read("deploy").unwrap(),
            "---\nname: deploy\ndescription: new way\n---\nworkspace body"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn references_cannot_escape_the_skill_directory() {
        let workspace = temp_root("refs");
        let dir = workspace.join("skills").join("s");
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: s\n---\nbody").unwrap();
        std::fs::write(dir.join("references").join("a.md"), "detail").unwrap();
        std::fs::write(workspace.join("secret.txt"), "secret").unwrap();

        let index = SkillIndex::load(&workspace, Path::new("/nonexistent-home"));
        assert_eq!(
            index.read_reference("s", "references/a.md").unwrap(),
            "detail"
        );
        assert!(
            index.read_reference("s", "../secret.txt").is_none(),
            "no traversal"
        );
        assert!(
            index.read_reference("s", "/etc/passwd").is_none(),
            "no absolute paths"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
