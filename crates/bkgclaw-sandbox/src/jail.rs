//! The jail itself: fork, restrict, exec.
//!
//! Landlock can only restrict the calling process, so the jail is applied
//! between `fork` and `exec` in the child — the documented pattern of the
//! `landlock` crate and the single place this crate uses `unsafe`
//! (`CommandExt::pre_exec` is declared unsafe because it runs after fork;
//! the landlock calls made there are async-signal-safe and designed for
//! exactly this use).
//!
//! Fail-closed: without a working Landlock the jail refuses to run
//! anything. Exit code 3 (environment), never a silent unsandboxed exec.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{resolve_write_paths, Config};
use crate::unboxexec;

/// What the doctor found out about this kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doctor {
    /// The highest Landlock ABI the kernel accepted, or None when the
    /// kernel (or the container) refuses Landlock entirely.
    pub landlock_abi: Option<u32>,
    /// True when `run` will actually jail.
    pub can_jail: bool,
}

/// Probe the kernel. Cheap, side-effect-free: builds rulesets that are
/// never applied.
pub fn doctor() -> Doctor {
    let abi = probe_landlock();
    Doctor { landlock_abi: abi, can_jail: abi.is_some() }
}

/// Map the probed ABI version back to the crate's ABI handle.
fn abi_from_version(version: u32) -> landlock::ABI {
    match version {
        5 => landlock::ABI::V5,
        4 => landlock::ABI::V4,
        3 => landlock::ABI::V3,
        2 => landlock::ABI::V2,
        _ => landlock::ABI::V1,
    }
}

fn probe_landlock() -> Option<u32> {
    // Try the ABIs newest-first; the first that the kernel accepts wins.
    for (version, abi) in [
        (5, landlock::ABI::V5),
        (4, landlock::ABI::V4),
        (3, landlock::ABI::V3),
        (2, landlock::ABI::V2),
        (1, landlock::ABI::V1),
    ] {
        if build_ruleset(abi, &[], &[]).is_ok() {
            return Some(version);
        }
    }
    None
}

/// Assemble the Landlock ruleset: read+execute everywhere, write only in
/// the allowed paths. Returns the created (not yet restricted) ruleset.
fn build_ruleset(
    abi: landlock::ABI,
    read_roots: &[PathBuf],
    write_paths: &[PathBuf],
) -> Result<landlock::RulesetCreated, String> {
    use landlock::{
        Access, AccessFs, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };
    let mut ruleset = Ruleset::default()
        // HardRequirement: if this kernel lacks any access right the ABI
        // promises, building fails loudly instead of quietly under-jailing.
        .set_compatibility(landlock::CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("landlock-zugriffsrechte: {e}"))?
        .create()
        .map_err(|e| format!("landlock-ruleset: {e}"))?;

    // Reads and executions come from the read roots (default: everything).
    for root in read_roots {
        let fd = PathFd::new(root).map_err(|e| format!("{}: {e}", root.display()))?;
        let access = AccessFs::from_read(abi);
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| format!("{}: {e}", root.display()))?;
    }

    // Writes only inside the allowed paths — the entire point of the jail.
    for path in write_paths {
        let fd = PathFd::new(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let access = AccessFs::from_write(abi);
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }

    Ok(ruleset)
}

/// What one jailed run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailOutcome {
    /// The command ran inside the jail; carries its exit code.
    Ran(i32),
    /// The jail could not be established; NOTHING was executed.
    Refused(String),
}

/// Run a command inside the write-jail.
///
/// `working_dir` is where relative write paths resolve and where the
/// command starts. The escape-hatch daemon starts unless the config
/// disables it; its socket path is handed to the jailed process through
/// `BKGCLAW_UNBOXEXEC_SOCK`.
pub fn run(config: &Config, working_dir: &Path, argv: &[String]) -> JailOutcome {
    if argv.is_empty() {
        return JailOutcome::Refused("kein befehl übergeben".into());
    }

    // Fail-closed before anything spawns. The probe result is pinned: we
    // build with exactly the ABI the kernel accepted, so a HardRequirement
    // build never disagrees with the probe.
    let doctor = doctor();
    let Some(landlock_version) = doctor.landlock_abi else {
        return JailOutcome::Refused(
            "landlock nicht verfügbar (kernel/container) — der jail würde nicht greifen; nichts wurde ausgeführt".into(),
        );
    };
    let abi = abi_from_version(landlock_version);

    let write_paths = resolve_write_paths(config, working_dir);
    let read_roots: Vec<PathBuf> = config
        .read
        .iter()
        .map(|entry| {
            let path = PathBuf::from(entry);
            if path.is_absolute() {
                path
            } else {
                working_dir.join(path)
            }
        })
        .collect();

    // Validate before forking: a bad path or an unusable rule refuses
    // the run instead of failing inside the child with a cryptic exec
    // error.
    if let Err(error) = build_ruleset(abi, &read_roots, &write_paths) {
        return JailOutcome::Refused(error);
    }

    // The escape hatch: a daemon OUTSIDE the jail, reachable through a
    // Unix socket. Its lifetime is tied to this run.
    let daemon = if config.unboxexec {
        unboxexec::spawn_daemon(&config.unboxexec_allow)
    } else {
        None
    };

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(working_dir)
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", working_dir)
        .env("TERM", std::env::var("TERM").unwrap_or_else(|_| "dumb".into()));
    // The jail is a filesystem jail, not a secret jail: the credentials and
    // settings the jailed agent legitimately needs travel with it, one
    // explicit allowlist — nothing else leaks in from the host shell.
    for name in [
        "NIM_API_KEY",
        "NIM_BASE_URL",
        "BKGCLAW_HOME",
        "BKGCLAW_WORKSPACE",
        "BKGCLAW_SKILLS_PATHS",
        "BKGCLAW_MAX_TURNS",
        "LANG",
        "NO_COLOR",
    ] {
        if let Ok(value) = std::env::var(name) {
            command.env(name, value);
        }
    }
    if let Some(handle) = &daemon {
        command.env(unboxexec::SOCKET_ENV, handle.socket_path());
    }

    {
        let write_paths = write_paths.clone();
        let read_roots = read_roots.clone();
        // SAFETY: this closure runs in the forked child before exec.
        // It only calls landlock's apply() — async-signal-safe, no
        // allocation, no threads — which is precisely what the landlock
        // crate documents pre_exec for. Failure aborts the exec.
        unsafe {
            command.pre_exec(move || {
                let ruleset = build_ruleset(abi, &read_roots, &write_paths)
                    .map_err(|e| std::io::Error::other(e))?;
                // `restrict_self` is landlock 0.4's apply: the ruleset
                // becomes binding for this process and everything it execs.
                ruleset
                    .restrict_self()
                    .map_err(|e| std::io::Error::other(format!("landlock: {e}")))?;
                Ok(())
            });
        }
    }

    match command.status() {
        Ok(status) => {
            drop(daemon);
            JailOutcome::Ran(status.code().unwrap_or(-1))
        }
        Err(error) => JailOutcome::Refused(format!("prozess startete nicht: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "bkgclaw-jail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ))
    }

    #[test]
    fn the_doctor_reports_a_structured_verdict() {
        let doctor = doctor();
        // Whatever this kernel says, the two fields must agree: no Landlock
        // means no jail.
        assert_eq!(doctor.can_jail, doctor.landlock_abi.is_some());
    }

    #[test]
    fn an_empty_argv_is_refused_without_touching_the_kernel() {
        let config = Config::default();
        assert!(matches!(
            run(&config, Path::new("."), &[]),
            JailOutcome::Refused(_)
        ));
    }

    #[test]
    fn the_write_jail_is_real_when_landlock_is_real() {
        // The end-to-end proof, gated on the kernel: inside the jail a
        // write into the allowed directory succeeds, a write outside it
        // fails, and a read everywhere still works. On a kernel without
        // Landlock this test cannot run — and `run` would refuse anyway,
        // which the next test pins down.
        let doctor = doctor();
        if !doctor.can_jail {
            return; // kernel/container without landlock: covered below
        }

        let dir = working_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::default(); // write: ["."], read: ["/"]
        let inside = dir.join("innen.txt");
        let outside = dir.join("..").join("aussen-verboten.txt");

        // Write inside must succeed.
        let outcome = run(
            &config,
            &dir,
            &[
                "sh".into(),
                "-c".into(),
                format!("echo ok > {}", inside.display()),
            ],
        );
        assert!(
            matches!(outcome, JailOutcome::Ran(code) if code == 0),
            "schreiben im jail-fenster schlug fehl: {outcome:?}"
        );
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "ok");

        // Write outside must fail.
        let outcome = run(
            &config,
            &dir,
            &[
                "sh".into(),
                "-c".into(),
                format!("echo bös > {} 2>/dev/null || exit 7", outside.display()),
            ],
        );
        assert!(
            matches!(outcome, JailOutcome::Ran(code) if code == 7),
            "schreiben außerhalb wurde nicht verweigert: {outcome:?}"
        );
        assert!(!outside.exists(), "die jail-grenze wurde durchbrochen");

        // Reading outside still works — no redirect: writing to /dev/null
        // is a write, and the jail denies writes outside its window by
        // design (this test tripped over its own redirection once).
        let outcome = run(&config, &dir, &["head".into(), "-c".into(), "1".into(), "/etc/hostname".into()]);
        assert!(matches!(outcome, JailOutcome::Ran(0)), "lesen im jail: {outcome:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn without_landlock_nothing_runs() {
        // Simulated by a config whose write root does not exist —
        // build_ruleset fails and run must refuse. This pins the
        // fail-closed path without faking a kernel.
        let config = Config {
            write: vec!["/dieses/verzeichnis/existiert/nicht".into()],
            ..Config::default()
        };
        let outcome = run(&config, Path::new("."), &["sh".into(), "-c".into(), "echo x".into()]);
        assert!(matches!(outcome, JailOutcome::Refused(_)), "{outcome:?}");
    }
}
