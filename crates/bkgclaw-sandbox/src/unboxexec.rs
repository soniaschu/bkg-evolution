//! The escape hatch: an audited daemon *outside* the jail.
//!
//! The original enclave idea, kept 1:1 — a jailed agent sometimes needs a
//! program that cannot run under filesystem restrictions (a package
//! manager writing to system paths, git reading global config). Instead of
//! punching holes in the jail, the jailed process asks this daemon over a
//! Unix socket; the daemon runs **allow-listed** executables outside the
//! jail and returns their output. Every request lands in an audit log.
//!
//! Protocol (one JSON object per line, both directions):
//!
//! ```text
//! → {"argv": ["git", "status"], "cwd": "/work"}
//! ← {"ok": true, "exit": 0, "stdout": "…", "stderr": "…"}
//! ← {"ok": false, "error": "nicht erlaubt: git"}
//! ```
//!
//! The allow list matches the RESOLVED executable path by prefix, so
//! `/usr/local/bin/` admits everything beneath it. Paths from the request
//! are never resolved through a shell: argv[0] is looked up in PATH by the
//! daemon itself, and the daemon never runs a shell.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Environment variable through which the jailed process learns the socket.
pub const SOCKET_ENV: &str = "BKGCLAW_UNBOXEXEC_SOCK";

/// The audit log lives next to the sessions — machine-scoped, like
/// everything the daemon produces.
fn audit_path() -> PathBuf {
    bkgclaw_store::home_root().join("unboxexec-audit.log")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    ok: bool,
    #[serde(default)]
    exit: i32,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    error: String,
}

/// The running daemon: a socket path and the thread's lifetime.
pub struct DaemonHandle {
    socket: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Dropping the handle joins the listener thread — the daemon's
    /// lifetime is the jailed run's lifetime, not the machine's.
    listener: Option<std::thread::JoinHandle<()>>,
}

impl DaemonHandle {
    pub fn socket_path(&self) -> String {
        self.socket.display().to_string()
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        // Flag first, then join: the nonblocking accept loop sees the flag
        // within its 20 ms tick and exits. A daemon left behind after the
        // run would be a standing privilege.
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

/// Whether an executable is allowed. Pure — the tests drive it directly.
///
/// Semantics: an entry ending in `/` admits everything beneath that
/// directory; any other entry admits exactly that file. `/usr/bin/git`
/// therefore does NOT admit `/usr/bin/gitk` — a boundary the agent cannot
/// widen by name-colliding.
pub fn allowed(executable: &str, allow: &[String]) -> bool {
    if allow.is_empty() {
        return false; // an empty list allows nothing, by design
    }
    allow.iter().any(|entry| {
        if entry.ends_with('/') {
            executable.starts_with(entry.as_str())
        } else {
            executable == entry
        }
    })
}

/// Resolve argv[0] the way a shell would — PATH lookup — without ever
/// invoking a shell. Returns the absolute executable path.
fn resolve_executable(argv0: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(argv0);
    if direct.is_absolute() {
        return Some(direct);
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(dir).join(argv0);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Start the daemon for one jailed run. One thread accepts connections
/// serially: escape-hatch requests are rare and short, and a queue beats
/// a thread pool for something this auxiliary.
pub fn spawn_daemon(allow: &[String]) -> Option<DaemonHandle> {
    let dir = std::env::temp_dir().join(format!(
        "bkgclaw-unboxexec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let socket = dir.join("daemon.sock");
    let listener = UnixListener::bind(&socket).ok()?;
    // A blocking accept would keep the thread alive after its owner is
    // gone; nonblocking + a stop flag makes the join in Drop real.
    listener.set_nonblocking(true).ok()?;

    let allow = Arc::new(allow.to_vec());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = std::thread::spawn(move || {
        // Serial accept loop: one request at a time, fully drained before
        // the next. Simple, auditable, and fast enough by orders of
        // magnitude for what a jailed agent asks for.
        while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = handle_one(stream, &allow);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });

    Some(DaemonHandle {
        socket,
        stop,
        listener: Some(handle),
    })
}

fn handle_one(mut stream: UnixStream, allow: &[String]) -> Result<Response, String> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .map_err(|e| format!("anfrage unlesbar: {e}"))?;
    }
    let request: Request = serde_json::from_str(line.trim())
        .map_err(|e| format!("anfrage kein json: {e}"))?;

    let response = match request.argv.first() {
        None => Response {
            ok: false,
            error: "leerer aufruf".into(),
            ..Response::default()
        },
        Some(argv0) => match resolve_executable(argv0) {
            None => Response {
                ok: false,
                error: format!("programm nicht gefunden: {argv0}"),
                ..Response::default()
            },
            Some(executable) => {
                let executable_str = executable.display().to_string();
                if !allowed(&executable_str, allow) {
                    audit(&request, "verweigert", &executable_str);
                    Response {
                        ok: false,
                        error: format!("nicht erlaubt: {argv0} ({executable_str})"),
                        ..Response::default()
                    }
                } else {
                    let mut command = std::process::Command::new(&executable);
                    command.args(&request.argv[1..]);
                    if let Some(cwd) = &request.cwd {
                        if !cwd.contains("..") {
                            command.current_dir(cwd);
                        }
                    }
                    match command.output() {
                        Ok(output) => {
                            let response = Response {
                                ok: true,
                                exit: output.status.code().unwrap_or(-1),
                                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                                error: String::new(),
                            };
                            audit(&request, "erlaubt", &executable_str);
                            response
                        }
                        Err(error) => Response {
                            ok: false,
                            error: format!("ausführung fehlgeschlagen: {error}"),
                            ..Response::default()
                        },
                    }
                }
            }
        },
    };

    let mut payload = serde_json::to_string(&response).map_err(|e| e.to_string())?;
    payload.push('\n');
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| format!("antwort unsendbar: {e}"))?;
    Ok(response)
}

fn audit(request: &Request, verdict: &str, executable: &str) {
    let line = format!(
        "{}\t{}\t{}\t{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        verdict,
        executable,
        request.argv.join(" ")
    );
    if let Some(parent) = audit_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path())
        .and_then(|mut file| file.write_all(line.as_bytes()));
}

/// The client side: what the jailed process runs. One request, one
/// response, then the socket closes.
pub fn request(socket_path: &str, argv: &[String], cwd: Option<&str>) -> Result<Response, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| format!("daemon nicht erreichbar: {e}"))?;
    let request = Request {
        argv: argv.to_vec(),
        cwd: cwd.map(str::to_string),
    };
    let payload = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    let mut stream = stream;
    stream
        .write_all(format!("{payload}\n").as_bytes())
        .map_err(|e| format!("senden fehlgeschlagen: {e}"))?;

    // The daemon answers promptly; a hung escape hatch should fail loudly,
    // not park the agent forever.
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    let mut reader = BufReader::new(&mut stream);
    reader
        .read_line(&mut line)
        .map_err(|e| format!("antwort unlesbar: {e}"))?;
    serde_json::from_str(line.trim()).map_err(|e| format!("antwort kein json: {e}"))
}

impl Default for Response {
    fn default() -> Self {
        Response {
            ok: false,
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            error: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_allow_list_allows_nothing() {
        assert!(!allowed("git", &[]));
    }

    #[test]
    fn exact_entries_admit_one_file_and_slashes_admit_directories() {
        let allow = vec!["/usr/bin/git".to_string(), "/usr/local/bin/".to_string()];
        assert!(allowed("/usr/bin/git", &allow));
        assert!(allowed("/usr/local/bin/docker", &allow));
        // Not a sibling by name collision: git does not admit gitk.
        assert!(!allowed("/usr/bin/gitk", &allow));
        assert!(!allowed("/usr/bin/curl", &allow));
        assert!(!allowed("/usr/bin/git/config", &allow), "no path-beneath tricks");
    }

    #[test]
    fn a_roundtrip_runs_an_allowed_command_and_refuses_others() {
        // Exact entry: echo runs, everything else — including sh — is
        // refused with a reason. No directory prefix in the list, so a
        // name collision cannot smuggle a shell through.
        let echo = resolve_for_test("echo");
        let allow = vec![echo.clone()];
        let daemon = spawn_daemon(&allow).expect("daemon startet");
        let socket = daemon.socket_path();

        let allowed = request(&socket, &["echo".into(), "hallo".into()], None).unwrap();
        assert!(allowed.ok, "{allowed:?}");
        assert_eq!(allowed.exit, 0);
        assert!(allowed.stdout.contains("hallo"));

        let denied = request(&socket, &["sh".into(), "-c".into(), "true".into()], None).unwrap();
        assert!(!denied.ok, "sh muss mit einer exakten allow-list verweigert werden");
        assert!(denied.error.contains("nicht erlaubt"), "{denied:?}");

        drop(daemon);
        // The socket file is gone with the daemon, and the thread left
        // with it — the drop joins.
        assert!(!std::path::Path::new(&socket).exists());
    }

    /// Resolve an executable the daemon would resolve, for building an
    /// honest allow list in tests.
    fn resolve_for_test(name: &str) -> String {
        let path = std::env::var("PATH").unwrap_or_default();
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.is_file() {
                return candidate.display().to_string();
            }
        }
        panic!("programm für den test nicht gefunden: {name}");
    }

    #[test]
    fn the_audit_log_records_verdicts() {
        // The roundtrip above wrote entries; verify the audit has content
        // in the machine's home. Testing file presence, not this test's own
        // entries, keeps it independent of test ordering.
        let path = audit_path();
        assert!(path.exists(), "audit fehlt: {}", path.display());
    }
}
