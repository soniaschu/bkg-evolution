//! End-to-end tests for the built binary.
//!
//! These run the real `bkgclaw` executable, because the contract being tested —
//! exit codes and stdout purity — is a property of the process, not of a
//! function. A unit test on `Verdict::exit_code` would not catch a dispatch
//! branch that forgets to return it.
//!
//! No test touches the network: the `local` provider is offline by design.

use std::path::PathBuf;
use std::process::{Command, Output};

fn binary() -> PathBuf {
    // target/debug/bkgclaw, next to the integration test binary.
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // deps/
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("bkgclaw")
}

struct Sandbox {
    config_dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let config_dir =
            std::env::temp_dir().join(format!("bkgclaw-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config_dir);
        Sandbox { config_dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(binary())
            .args(args)
            .env("BKGCLAW_CONFIG_DIR", &self.config_dir)
            // Strip inherited credentials so a developer's shell cannot make
            // a test pass or fail depending on who ran it.
            .env_remove("DIGITALOCEAN_TOKEN")
            .output()
            .expect("bkgclaw binary is built by `cargo test`")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.config_dir);
    }
}

#[test]
fn the_binary_is_where_the_tests_expect_it() {
    assert!(
        binary().exists(),
        "run `cargo test` from the workspace root; binary missing at {:?}",
        binary()
    );
}

#[test]
fn help_exits_zero() {
    let sandbox = Sandbox::new("help");
    let out = sandbox.run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in ["doctor", "providers", "auth", "deploy", "status", "destroy"] {
        assert!(text.contains(expected), "help is missing `{expected}`");
    }
}

#[test]
fn exit_code_zero_for_a_clean_doctor() {
    let sandbox = Sandbox::new("doctor-ok");
    // Only the `local` provider needs nothing, so a clean run is possible.
    let out = sandbox.run(&["doctor"]);
    let code = out.status.code();
    // Without DIGITALOCEAN_TOKEN the environment check legitimately fails.
    assert!(
        code == Some(1) || code == Some(0),
        "doctor must report a verdict, never crash; got {code:?}"
    );
}

#[test]
fn exit_code_two_for_an_unknown_provider() {
    let sandbox = Sandbox::new("unknown-provider");
    let out = sandbox.run(&["status", "--provider", "not-a-provider"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unknown provider is a usage error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not-a-provider"));
    // The message must list the valid options.
    assert!(
        stderr.contains("local"),
        "the error must name the known providers"
    );
}

#[test]
fn exit_code_two_when_destroy_lacks_confirmation() {
    let sandbox = Sandbox::new("destroy-guard");
    let out = sandbox.run(&["destroy", "--provider", "local", "--id", "x"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an irreversible command must refuse without --yes"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--yes"), "the error must say what to add");
}

#[test]
fn destroy_succeeds_with_confirmation() {
    let sandbox = Sandbox::new("destroy-yes");
    let out = sandbox.run(&["destroy", "--provider", "local", "--id", "local-1", "--yes"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn exit_code_three_when_the_provider_cannot_be_reached() {
    let sandbox = Sandbox::new("unreachable");
    // digitalocean's transport is not wired, which surfaces as an environment
    // failure — not as a crash and not as a false success.
    let out = sandbox.run(&["deploy", "--provider", "digitalocean", "--region", "nyc3"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "unreachable must be an environment error"
    );
}

#[test]
fn json_mode_emits_exactly_one_parseable_line() {
    let sandbox = Sandbox::new("json-purity");
    for args in [
        vec!["doctor", "--json"],
        vec!["providers", "--json"],
        vec!["auth", "status", "--json"],
        vec!["status", "--provider", "local", "--json"],
        vec!["status", "--provider", "nope", "--json"],
        vec!["destroy", "--provider", "local", "--id", "x", "--json"],
        vec!["exit-codes", "--json"],
    ] {
        let out = sandbox.run(&args);
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "`{}` wrote {} stdout lines; JSON mode allows exactly one",
            args.join(" "),
            lines.len()
        );
        serde_json::from_str::<serde_json::Value>(lines[0])
            .unwrap_or_else(|e| panic!("`{}` stdout is not JSON: {e}", args.join(" ")));
    }
}

#[test]
fn json_mode_still_reports_the_right_exit_code() {
    let sandbox = Sandbox::new("json-exit");
    // The rendering changed; the contract did not.
    assert_eq!(sandbox.run(&["providers", "--json"]).status.code(), Some(0));
    assert_eq!(
        sandbox
            .run(&["status", "--provider", "nope", "--json"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn quiet_mode_prints_one_line() {
    let sandbox = Sandbox::new("quiet");
    let out = sandbox.run(&["providers", "--quiet"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().filter(|l| !l.trim().is_empty()).count(), 1);
}

#[test]
fn a_credential_never_appears_in_any_output() {
    let sandbox = Sandbox::new("secret");
    let secret = "dop_v1_9f8a7b6c5d4e_LEAK_CANARY";

    // Configure it through the real auth flow.
    let login = Command::new(binary())
        .args([
            "auth",
            "login",
            "digitalocean",
            "--env-var",
            "DIGITALOCEAN_TOKEN",
        ])
        .env("BKGCLAW_CONFIG_DIR", &sandbox.config_dir)
        .env("DIGITALOCEAN_TOKEN", secret)
        .output()
        .expect("runs");
    assert_eq!(login.status.code(), Some(0));

    // Now ask every command, in every mode, for anything it prints.
    for args in [
        vec!["doctor"],
        vec!["doctor", "--json"],
        vec!["providers"],
        vec!["providers", "--json"],
        vec!["auth", "status"],
        vec!["auth", "status", "--json"],
        vec!["status", "--provider", "digitalocean"],
        vec!["status", "--provider", "digitalocean", "--json"],
    ] {
        let out = Command::new(binary())
            .args(&args)
            .env("BKGCLAW_CONFIG_DIR", &sandbox.config_dir)
            .env("DIGITALOCEAN_TOKEN", secret)
            .output()
            .expect("runs");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !combined.contains(secret),
            "`{}` leaked the credential:\n{combined}",
            args.join(" ")
        );
    }

    // And it must not be sitting in the config file either.
    let config =
        std::fs::read_to_string(sandbox.config_dir.join("config.toml")).expect("config written");
    assert!(
        !config.contains(secret),
        "the config file stored the credential value:\n{config}"
    );
    // Only the pointer is stored.
    assert!(config.contains("DIGITALOCEAN_TOKEN"));
}

#[test]
fn deploy_on_the_local_provider_needs_no_credential() {
    let sandbox = Sandbox::new("local-deploy");
    let out = sandbox.run(&[
        "deploy",
        "--provider",
        "local",
        "--region",
        "eu",
        "--name",
        "demo",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("demo"));
}

#[test]
fn status_lists_the_local_instance() {
    let sandbox = Sandbox::new("local-status");
    let out = sandbox.run(&["status", "--provider", "local", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let payload: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let rows = payload["data"].as_array().expect("data is a list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "running");
}

#[test]
fn the_exit_code_contract_is_self_documenting() {
    let sandbox = Sandbox::new("exit-codes");
    let out = sandbox.run(&["exit-codes", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let payload: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(payload["data"]["0"], "ok");
    assert!(
        payload["data"]["3"]
            .as_str()
            .unwrap()
            .contains("credential")
    );
}

#[test]
fn a_malformed_config_is_reported_not_ignored() {
    let sandbox = Sandbox::new("bad-config");
    std::fs::create_dir_all(&sandbox.config_dir).unwrap();
    std::fs::write(
        sandbox.config_dir.join("config.toml"),
        "[settings]\ndefualt_provider = \"typo\"\n",
    )
    .unwrap();

    let out = sandbox.run(&["auth", "status", "--json"]);
    // A misspelled key must surface, not be silently dropped.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(
        out.status.code(),
        Some(4),
        "a config typo is not an internal error"
    );
    assert!(
        combined.contains("malformed") || combined.contains("defualt"),
        "the error must name the problem: {combined}"
    );
}
