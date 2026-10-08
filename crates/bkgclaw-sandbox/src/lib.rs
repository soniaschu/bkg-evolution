//! `bkgclaw-sandbox` — the write-jail.
//!
//! Our own Rust version of the enclave idea (kohkimakimoto/enclave, Go,
//! macOS `sandbox-exec`), rebuilt for the platform we actually run on:
//!
//! > run any command with **file writes restricted to chosen paths**,
//! > reads allowed everywhere, plus one audited escape hatch.
//!
//! Why Landlock: it is the Linux kernel's unprivileged sandbox — enforced
//! by the kernel, no root, no user namespaces, no helper daemon. The
//! model is deliberately minimal, like the original: no network rules, no
//! syscall filtering. An agent that can only write inside its project
//! cannot destroy the machine; everything else stays simple.
//!
//! `unboxexec` is the escape hatch from the original, kept 1:1: the
//! jailed command may ask a daemon *outside* the jail to run
//! allow-listed programs (git, package managers, …) and returns their
//! output. Every request is audited.
//!
//! **Fail-closed is the contract**: if the kernel cannot enforce the jail,
//! `run` refuses with exit code 3 and starts nothing. A sandbox that
//! silently does not sandbox is worse than no sandbox.
//!
//! One honest note on `unsafe`: exactly one bridge uses it — `pre_exec`,
//! applying the Landlock ruleset in the forked child before `exec`. That
//! is the documented pattern of the `landlock` crate; the calls are
//! async-signal-safe and the surface is a dozen lines, isolated in
//! `jail::exec`.

pub mod config;
pub mod jail;
pub mod unboxexec;

pub use config::{merged_config, Config, DEFAULT_CONFIG_NAME, LOCAL_CONFIG_NAME};
pub use jail::{doctor, run, Doctor, JailOutcome};
pub use unboxexec::{spawn_daemon, request, DaemonHandle, Response};
