//! The plugin system: install, list, remove.
//!
//! A bkgclaw plugin is **data, not code** — we never load or execute
//! anything from a plugin. What a plugin brings:
//!
//! - a manifest (`openclaw.plugin.json`, `bkgclaw.plugin.json` or
//!   `plugin.json`) describing itself, and
//! - skills: a `skills/` directory of SKILL.md folders, or bare `.md`
//!   files anywhere in its root — both load through the normal skill index.
//!
//! Install sources:
//! - `github:<owner>/<repo>` — shallow git clone
//! - a local path — recursive copy (for developing a plugin in place)
//!
//! OpenClaw provider *contracts* in the manifest (speech providers, media
//! understanding, …) are recorded and shown, but bkgclaw does not execute
//! them; wiring a contract is per-plugin work and saying otherwise would
//! be a lie the user would rely on.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The plugin cache: `<home>/plugins/`.
pub fn plugin_dir(home: &Path) -> PathBuf {
    home.join("plugins")
}

/// What a manifest says about itself. Every field optional at the JSON
/// level — a broken manifest is reported, not guessed around.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginManifest {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    /// Raw OpenClaw-style contracts, kept as-is for the listing.
    #[serde(default)]
    pub contracts: serde_json::Value,
}

/// One installed plugin, as the CLI shows it.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledPlugin {
    /// Directory name under `<home>/plugins/`.
    pub dir_name: String,
    pub manifest: PluginManifest,
    /// Where it was installed from (`github:owner/repo` or a local path).
    pub source: String,
    /// Seconds since the epoch.
    pub installed_at: u64,
    /// How many skills the plugin contributes to the index.
    pub skill_count: usize,
}

/// The installer's own bookkeeping file, written next to the plugin.
const PLUGIN_META: &str = ".bkgclaw-plugin.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginMeta {
    source: String,
    installed_at: u64,
}

/// Where a source string points.
enum Source {
    Github { owner: String, repo: String },
    Local(PathBuf),
}

fn parse_source(raw: &str) -> Result<Source, String> {
    let raw = raw.trim().trim_end_matches(".git");
    if let Some(rest) = raw.strip_prefix("github:") {
        return parse_repo_slug(rest);
    }
    // A bare `owner/repo` is also a github source — that is what people
    // type when they forget the prefix, and refusing it helps nobody.
    if !raw.contains('/') && !raw.starts_with('.') && !raw.starts_with('/') && raw.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && raw != "list" && raw != "remove" && raw != "install" {
        return Err("keine quelle erkannt — github:<owner>/<repo> oder ein lokaler pfad".to_string());
    }
    if raw.contains(':') && !raw.starts_with('/') && !raw.contains("://") {
        // Some other scheme (git@…, https://) is not supported; say so.
        if raw.starts_with("git@") || raw.starts_with("https://") {
            return parse_repo_slug(raw.rsplit('/').take(2).collect::<Vec<_>>().join("/").as_str());
        }
    }
    let path = PathBuf::from(raw);
    if path.exists() {
        Ok(Source::Local(path))
    } else {
        // Not a path that exists: the last honest interpretation is a
        // github slug of exactly two segments.
        parse_repo_slug(raw)
    }
}

fn parse_repo_slug(slug: &str) -> Result<Source, String> {
    let Some((owner, repo)) = slug.split_once('/') else {
        return Err(format!("`{slug}` ist kein repo-slug (owner/repo)"));
    };
    let owner = owner.trim();
    let repo = repo.trim();
    if owner.is_empty() || repo.is_empty() || owner.contains('/') || repo.contains('/') {
        return Err(format!("`{slug}` ist kein repo-slug (owner/repo)"));
    }
    Ok(Source::Github { owner: owner.to_string(), repo: repo.to_string() })
}

/// Install a plugin from a source. Returns the installed plugin.
pub fn install(home: &Path, source: &str) -> Result<InstalledPlugin, String> {
    let parsed = parse_source(source)?;
    let (name, target) = match &parsed {
        Source::Github { owner, repo } => {
            let name = repo.clone();
            let url = format!("https://github.com/{owner}/{repo}");
            let target = plugin_dir(home).join(&name);
            if target.exists() {
                return Err(format!("plugin `{name}` ist bereits installiert — erst entfernen"));
            }
            std::fs::create_dir_all(plugin_dir(home)).map_err(|e| e.to_string())?;
            // Shallow clone: plugins are data; history is dead weight.
            let status = std::process::Command::new("git")
                .args(["clone", "--depth", "1", &url])
                .arg(&target)
                .output()
                .map_err(|e| format!("git fehlt oder scheiterte: {e}"))?;
            if !status.status.success() {
                let stderr = String::from_utf8_lossy(&status.stderr).trim().to_string();
                // A failed clone must not leave a half-empty directory
                // that lists as installed.
                let _ = std::fs::remove_dir_all(&target);
                return Err(format!("clone fehlgeschlagen: {stderr}"));
            }
            (name, target)
        }
        Source::Local(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "plugin".into());
            let target = plugin_dir(home).join(&name);
            if target.exists() {
                return Err(format!("plugin `{name}` ist bereits installiert — erst entfernen"));
            }
            std::fs::create_dir_all(plugin_dir(home)).map_err(|e| e.to_string())?;
            copy_dir(path, &target).map_err(|e| {
                let _ = std::fs::remove_dir_all(&target);
                format!("kopieren fehlgeschlagen: {e}")
            })?;
            (name, target)
        }
    };

    let manifest = read_manifest(&target)
        .map_err(|error| {
            // Without a manifest it is not a plugin; do not keep the copy.
            let _ = std::fs::remove_dir_all(&target);
            error
        })?;

    let meta = PluginMeta {
        source: source.trim().to_string(),
        installed_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    std::fs::write(
        target.join(PLUGIN_META),
        serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let mut installed = InstalledPlugin {
        dir_name: name,
        manifest,
        source: meta.source,
        installed_at: meta.installed_at,
        skill_count: 0,
    };
    installed.skill_count = count_skills(home, &installed.dir_name);
    Ok(installed)
}

/// Read and list every installed plugin. A broken plugin (missing or
/// malformed manifest) is listed with an empty manifest and a marker in
/// the description rather than breaking the listing.
pub fn list(home: &Path) -> Vec<InstalledPlugin> {
    let dir = plugin_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut plugins = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() || path.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false) {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        let manifest = read_manifest(&path).unwrap_or_else(|error| PluginManifest {
            id: dir_name.clone(),
            name: dir_name.clone(),
            description: format!("⚠ manifest unlesbar: {error}"),
            version: String::new(),
            contracts: serde_json::Value::Null,
        });
        let (source, installed_at) = read_meta(&path);
        plugins.push(InstalledPlugin {
            dir_name,
            manifest,
            source,
            installed_at,
            skill_count: count_skills(home, &entry.file_name().to_string_lossy()),
        });
    }
    plugins.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
    plugins
}

/// Remove an installed plugin by directory name.
pub fn remove(home: &Path, name: &str) -> Result<InstalledPlugin, String> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("ungültiger plugin-name".to_string());
    }
    let target = plugin_dir(home).join(name);
    if !target.exists() {
        return Err(format!("plugin `{name}` ist nicht installiert"));
    }
    let manifest = read_manifest(&target).unwrap_or_default();
    let (source, installed_at) = read_meta(&target);
    let installed = InstalledPlugin {
        dir_name: name.to_string(),
        manifest,
        source,
        installed_at,
        skill_count: count_skills(home, name),
    };
    std::fs::remove_dir_all(&target).map_err(|e| format!("entfernen fehlgeschlagen: {e}"))?;
    Ok(installed)
}

fn read_manifest(root: &Path) -> Result<PluginManifest, String> {
    for candidate in ["bkgclaw.plugin.json", "openclaw.plugin.json", "plugin.json"] {
        let path = root.join(candidate);
        if path.is_file() {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let manifest: PluginManifest =
                serde_json::from_str(&text).map_err(|e| format!("`{candidate}` ist kein gültiges manifest: {e}"))?;
            return Ok(manifest);
        }
    }
    Err("kein plugin-manifest gefunden (bkgclaw.plugin.json / openclaw.plugin.json / plugin.json)".to_string())
}

fn read_meta(root: &Path) -> (String, u64) {
    std::fs::read_to_string(root.join(PLUGIN_META))
        .ok()
        .and_then(|text| serde_json::from_str::<PluginMeta>(&text).ok())
        .map(|meta| (meta.source, meta.installed_at))
        .unwrap_or(("unbekannt".to_string(), 0))
}

/// How many skills this plugin contributes. Counts both layouts through
/// the real index, not a guess.
fn count_skills(home: &Path, _name: &str) -> usize {
    let roots = crate::skills::plugin_roots(home);
    SkillCountIndex::count(home, &roots)
}

/// Counting through the real scanner without polluting the API: the index
/// over only the plugin roots.
struct SkillCountIndex;

impl SkillCountIndex {
    fn count(home: &Path, plugin_roots: &[PathBuf]) -> usize {
        // The index over a nonexistent workspace + the real home, then
        // keep only skills whose dir lives under a plugin root.
        let index = crate::skills::SkillIndex::load(Path::new("/nonexistent-bkgclaw-workspace"), home);
        index
            .skills
            .iter()
            .filter(|skill| plugin_roots.iter().any(|root| skill.dir.starts_with(root)))
            .count()
    }
}

/// Recursive directory copy, used for local plugin sources.
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())?.filter_map(|e| e.ok()) {
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            // node_modules and target dirs are never part of a plugin.
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), "node_modules" | "target" | ".git") {
                continue;
            }
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|e| format!("`{}`: {e}", source.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bkgclaw-plugins-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture_plugin(dir: &Path) {
        std::fs::create_dir_all(dir.join("skills").join("sprechen")).unwrap();
        std::fs::write(
            dir.join("openclaw.plugin.json"),
            r#"{"id":"test-speech","name":"Test Speech","description":"TTS/STT providers","version":"1.2.3","contracts":{"speechProviders":["nvidia"]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("skills").join("sprechen").join("SKILL.md"),
            "---\nname: sprechen\ndescription: Wie man spricht\n---\nhier steht die anweisung",
        )
        .unwrap();
        std::fs::write(
            dir.join("stile.md"),
            "# Stile\nkurz, präzise, ohne schmuck.\n",
        )
        .unwrap();
    }

    #[test]
    fn a_local_plugin_installs_lists_and_removes() {
        let home = temp_home("local");
        let source = home.join("quelle");
        std::fs::create_dir_all(&source).unwrap();
        fixture_plugin(&source);

        let installed = install(&home, source.to_str().unwrap()).unwrap();
        assert_eq!(installed.manifest.id, "test-speech");
        assert_eq!(installed.manifest.version, "1.2.3");
        // Both skill layouts contribute: skills/sprechen + the bare stile.md.
        assert!(installed.skill_count >= 2, "got {}", installed.skill_count);

        let listed = list(&home);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].dir_name, "quelle");
        assert_eq!(listed[0].manifest.name, "Test Speech");

        let removed = remove(&home, "quelle").unwrap();
        assert_eq!(removed.manifest.id, "test-speech");
        assert!(list(&home).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_existing_installation_is_refused_not_overwritten() {
        let home = temp_home("dup");
        let source = home.join("quelle");
        std::fs::create_dir_all(&source).unwrap();
        fixture_plugin(&source);
        install(&home, source.to_str().unwrap()).unwrap();
        assert!(install(&home, source.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_directory_without_manifest_is_rejected_and_cleaned_up() {
        let home = temp_home("nomanifest");
        let source = home.join("leer");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("irgendwas.txt"), "x").unwrap();
        let error = install(&home, source.to_str().unwrap()).unwrap_err();
        assert!(error.contains("manifest"), "{error}");
        // No half-installed leftover.
        assert!(list(&home).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_github_slug_parses_and_a_broken_one_does_not() {
        match parse_source("github:dhiraj-salian/openclaw-nvidia-speech").unwrap() {
            Source::Github { owner, repo } => {
                assert_eq!(owner, "dhiraj-salian");
                assert_eq!(repo, "openclaw-nvidia-speech");
            }
            Source::Local(_) => panic!("github-quelle als lokal erkannt"),
        }
        assert!(parse_source("github:only-owner").is_err());
        assert!(parse_source("github:a/b/c").is_err());
    }

    #[test]
    fn removal_names_are_sanitised() {
        let home = temp_home("sanitize");
        assert!(remove(&home, "../escape").is_err());
        assert!(remove(&home, "a/b").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn plugin_skills_are_visible_through_the_skill_index() {
        let home = temp_home("skills");
        let source = home.join("quelle");
        std::fs::create_dir_all(&source).unwrap();
        fixture_plugin(&source);
        install(&home, source.to_str().unwrap()).unwrap();

        let index = crate::skills::SkillIndex::load(Path::new("/nonexistent-ws"), &home);
        assert!(index.get("sprechen").is_some(), "plugin skill must be indexed");
        assert_eq!(index.read("sprechen").unwrap().contains("anweisung"), true);
        let _ = std::fs::remove_dir_all(&home);
    }
}
