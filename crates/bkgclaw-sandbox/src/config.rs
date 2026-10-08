//! Configuration: three tiers of TOML, merged field by field.
//!
//! Order (each level overrides the previous for every field it sets):
//!
//! 1. User: `~/.config/bkgclaw/enclave.toml`
//! 2. Project: `./.enclave.toml`
//! 3. Local: `./.enclave.local.toml` (gitignore-friendly overrides)
//!
//! A missing file is not an error — the built-in defaults apply.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_NAME: &str = ".enclave.toml";
pub const LOCAL_CONFIG_NAME: &str = ".enclave.local.toml";

/// The effective sandbox configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Directories the jailed command may write to. Relative entries are
    /// resolved against the working directory at start time.
    #[serde(default = "default_write")]
    pub write: Vec<String>,
    /// Roots the jailed command may read (and execute from).
    #[serde(default = "default_read")]
    pub read: Vec<String>,
    /// Programs the jailed command may ask the outside daemon to run,
    /// matched by executable path prefix.
    #[serde(default)]
    pub unboxexec_allow: Vec<String>,
    /// Whether to start the escape-hatch daemon for jailed runs. Default
    /// on: an agent without any way out is a blocked agent.
    #[serde(default = "default_true")]
    pub unboxexec: bool,
}

fn default_write() -> Vec<String> {
    vec![".".to_string()]
}

fn default_read() -> Vec<String> {
    vec!["/".to_string()]
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            write: default_write(),
            read: default_read(),
            unboxexec_allow: Vec::new(),
            unboxexec: true,
        }
    }
}

/// Load and merge all three tiers for a working directory. Pure given the
/// paths — the tiering itself is what gets tested.
///
/// Each tier overrides only the fields it explicitly sets: a local file
/// granting an escape hatch must not reset the project's write roots to
/// defaults. Fail-closed: a tier that exists but does not parse aborts
/// the whole start — a config the operator cannot trust is worse than
/// one that refuses to load.
pub fn merged_config(user_config: Option<&Path>, working_dir: &Path) -> Result<Config, String> {
    /// `Option` mirrors the parsed tier: `Some` means the file said so.
    #[derive(Deserialize)]
    struct RawTier {
        write: Option<Vec<String>>,
        read: Option<Vec<String>>,
        unboxexec_allow: Option<Vec<String>>,
        unboxexec: Option<bool>,
    }

    let mut config = Config::default();

    let mut tiers: Vec<PathBuf> = Vec::new();
    if let Some(path) = user_config {
        tiers.push(path.to_path_buf());
    }
    for name in [DEFAULT_CONFIG_NAME, LOCAL_CONFIG_NAME] {
        tiers.push(working_dir.join(name));
    }

    for path in tiers {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // a missing tier is normal; only content decides
        };
        let tier: RawTier = toml::from_str(&text)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(write) = tier.write {
            config.write = write;
        }
        if let Some(read) = tier.read {
            config.read = read;
        }
        if let Some(unboxexec_allow) = tier.unboxexec_allow {
            config.unboxexec_allow = unboxexec_allow;
        }
        if let Some(unboxexec) = tier.unboxexec {
            config.unboxexec = unboxexec;
        }
    }
    Ok(config)
}

/// The user-level config path for this machine.
pub fn user_config_path() -> PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        });
    config_home.join("bkgclaw").join("enclave.toml")
}

/// Resolve the write roots to absolute paths against the working dir.
pub fn resolve_write_paths(config: &Config, working_dir: &Path) -> Vec<PathBuf> {
    config
        .write
        .iter()
        .map(|entry| {
            let path = PathBuf::from(entry);
            if path.is_absolute() {
                path
            } else {
                working_dir.join(path)
            }
        })
        .collect()
}

/// A ready-to-edit project config for `sandbox init`. Every commented
/// example is a top-level key, so uncommenting one line is enough —
/// section headers that silently swallow a field are a trap this template
/// does not set.
pub const TEMPLATE: &str = r#"# bkgclaw sandbox — schreibrechte und escape-hatch
# merge-reihenfolge: ~/.config/bkgclaw/enclave.toml → ./.enclave.toml → ./.enclave.local.toml

# verzeichnisse, in denen der gefangene prozess schreiben darf (relativ oder absolut)
# default: ["."]
# write = [".", "/tmp/geteilt"]

# lese-wurzeln (default: das ganze dateisystem)
# read = ["/"]

# programme, die der gefangene prozess über den daemon AUSSEN ausführen darf
# unboxexec_allow = ["/usr/bin/git", "/usr/bin/cargo", "/usr/local/bin/docker"]

# escape-hatch-daemon für gefangene läufe starten (default: true)
# unboxexec = true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_write_cwd_read_everything() {
        let config = Config::default();
        assert_eq!(config.write, [".".to_string()]);
        assert_eq!(config.read, ["/".to_string()]);
        assert!(config.unboxexec);
        assert!(config.unboxexec_allow.is_empty());
    }

    #[test]
    fn project_tiers_override_user_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        std::fs::write(&user, "write = [\"/nur-user\"]\n").unwrap();
        std::fs::write(dir.path().join(DEFAULT_CONFIG_NAME), "write = [\"/projekt\"]\n").unwrap();
        std::fs::write(
            dir.path().join(LOCAL_CONFIG_NAME),
            "unboxexec_allow = [\"/usr/bin/git\"]\n",
        )
        .unwrap();

        let config = merged_config(Some(&user), dir.path()).unwrap();
        // Project beats user, local adds its fields.
        assert_eq!(config.write, ["/projekt".to_string()]);
        assert_eq!(config.unboxexec_allow, ["/usr/bin/git".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_config_tier_is_silently_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let config = merged_config(None, dir.path()).unwrap();
        assert_eq!(config, Config::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_broken_config_tier_aborts_the_start() {
        // Fail-closed: a tier that exists but does not parse is an error,
        // never a silent fallback to defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_CONFIG_NAME), "write = ").unwrap();
        let error = merged_config(None, dir.path()).unwrap_err();
        assert!(error.contains(".enclave.toml"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn relative_write_paths_resolve_against_the_working_dir() {
        let config = Config { write: vec!["src".into(), "/abs".into()], ..Config::default() };
        let paths = resolve_write_paths(&config, Path::new("/work"));
        assert_eq!(paths, vec![PathBuf::from("/work/src"), PathBuf::from("/abs")]);
    }

    #[test]
    fn the_template_parses_as_a_config() {
        // A template that does not parse is a trap for every user who
        // uncomments a block.
        let parsed: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(parsed, Config::default());
    }
}
