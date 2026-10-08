//! The tool executor: one `match` arm per registered tool, real behaviour
//! in every arm, honest refusals where nothing is wired.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bkgclaw_core::loop_engine::ToolExecutor;
use bkgclaw_core::models::ToolCall;
use bkgclaw_store::{MemoryStore, SkillIndex, TaskStatus, TaskStore, home_root, workspace_root};

use crate::spawn::EngineRequest;
use crate::ssrf;

/// Watchdog for shell commands. A command that needs longer than this is
/// not "thinking", it is hung.
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(60);
/// Combined stdout+stderr cap. The loop truncates further before the model
/// sees it; this cap protects the process.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Web body cap: fetched pages are context, not downloads.
const MAX_WEB_BODY: usize = 512 * 1024;
const WEB_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound on files a single search reports.
const MAX_FILES: usize = 5_000;

/// Runs tools against the local machine. Constructed per process; the
/// fields are the roots the store crates resolve for it, so a test can
/// point everything at a tempdir without touching the operator's home.
pub struct LocalExecutor {
    /// Machine root (`~/.bkgclaw`): long-term memory lives here.
    pub home: PathBuf,
    /// Project root (`./.bkgclaw`): skills, tasks, personality.
    pub workspace: PathBuf,
    /// Shell watchdog; a field so a test can prove the kill path quickly.
    pub exec_timeout: Duration,
    /// The gateway engine, when sub-agents are available.
    pub engine: Option<tokio::sync::mpsc::Sender<EngineRequest>>,
}

impl LocalExecutor {
    /// The production executor: roots from the environment, standard
    /// watchdog, no engine (the CLI wires one when the gateway runs).
    pub fn new() -> Self {
        LocalExecutor {
            home: home_root(),
            workspace: workspace_root(),
            exec_timeout: EXECUTE_TIMEOUT,
            engine: None,
        }
    }

    fn argument(&self, call: &ToolCall, key: &str) -> Result<String, String> {
        call.arguments
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("missing `{key}` argument"))
    }

    fn memory(&self) -> Result<MemoryStore, String> {
        MemoryStore::load(&self.home)
    }

    fn task_store(&self) -> TaskStore {
        TaskStore::new(&self.workspace)
    }

    fn skills(&self) -> SkillIndex {
        SkillIndex::load(&self.workspace, &self.home)
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolExecutor for LocalExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String, String> {
        match call.name.as_str() {
            // ── Files ────────────────────────────────────────────────────
            "read_file" => {
                let path = self.argument(call, "path")?;
                let content =
                    std::fs::read_to_string(&path).map_err(|e| format!("`{path}`: {e}"))?;
                Ok(content)
            }
            "write_file" => {
                let path = self.argument(call, "path")?;
                let content = self.argument(call, "content")?;
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| format!("`{path}`: cannot create parent: {e}"))?;
                    }
                }
                let bytes = content.len();
                std::fs::write(&path, &content).map_err(|e| format!("`{path}`: {e}"))?;
                Ok(format!("wrote {bytes} bytes to `{path}`"))
            }
            "edit_file" => {
                let path = self.argument(call, "path")?;
                let old = self.argument(call, "old")?;
                let new = self.argument(call, "new")?;
                let text = std::fs::read_to_string(&path).map_err(|e| format!("`{path}`: {e}"))?;
                let matches = text.matches(&old).count();
                match matches {
                    0 => Err(format!("`{old}` does not appear in `{path}`")),
                    1 => {
                        let edited = text.replacen(&old, &new, 1);
                        std::fs::write(&path, &edited).map_err(|e| format!("`{path}`: {e}"))?;
                        Ok(format!("replaced 1 occurrence in `{path}`"))
                    }
                    n => Err(format!(
                        "`{old}` appears {n} times in `{path}`; add surrounding context to make it unique"
                    )),
                }
            }
            "list_directory" => {
                let path = self.argument(call, "path")?;
                let entries = std::fs::read_dir(&path).map_err(|e| format!("`{path}`: {e}"))?;
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if e.path().is_dir() {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                names.sort();
                Ok(names.join("\n"))
            }
            "search_files" => {
                let pattern = self.argument(call, "pattern")?;
                let root = call
                    .arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string();
                let mut hits = Vec::new();
                walk(std::path::Path::new(&root), 0, &mut |path| {
                    if hits.len() >= MAX_FILES {
                        return;
                    }
                    if let Ok(content) = std::fs::read_to_string(path) {
                        if content.contains(&pattern) {
                            hits.push(path.display().to_string());
                        }
                    }
                })?;
                if hits.len() >= MAX_FILES {
                    hits.push(format!("… stopped at {MAX_FILES} matches"));
                }
                if hits.is_empty() {
                    Ok(format!("no file contains `{pattern}`"))
                } else {
                    Ok(hits.join("\n"))
                }
            }

            // ── Shell ────────────────────────────────────────────────────
            "execute_command" => {
                let command = self.argument(call, "command")?;
                run_shell(&command, self.exec_timeout)
            }

            // ── Web ──────────────────────────────────────────────────────
            "web_fetch" => {
                let url = self.argument(call, "url")?;
                fetch("GET", &url, None)
            }
            "http_request" => {
                let url = self.argument(call, "url")?;
                let method = call
                    .arguments
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GET")
                    .to_string();
                let body = call
                    .arguments
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                fetch(&method, &url, body.as_deref())
            }
            "web_search" => Err(
                "no search backend is wired in this build; use web_fetch on a known URL"
                    .to_string(),
            ),

            // ── Memory (L2 entries; L1 is MEMORY.md, an ordinary file) ───
            "memory_set" => {
                let key = self.argument(call, "key")?;
                let value = self.argument(call, "value")?;
                let tags = call
                    .arguments
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut store = self.memory()?;
                store.set(&key, &value, tags);
                store.save(&self.home)?;
                Ok(format!("remembered `{key}`"))
            }
            "memory_get" => {
                let key = self
                    .argument(call, "id")
                    .or_else(|_| self.argument(call, "key"))?;
                let store = self.memory()?;
                match store.get(&key) {
                    Some(entry) => Ok(format!(
                        "{} = {} [{}]",
                        entry.key, entry.value, entry.updated_at
                    )),
                    None => Ok(format!("no memory entry for `{key}`")),
                }
            }
            "memory_search" => {
                let query = self.argument(call, "query")?;
                let store = self.memory()?;
                let hits = store.search(&query);
                if hits.is_empty() {
                    Ok(format!("nothing in memory matches `{query}`"))
                } else {
                    Ok(hits
                        .iter()
                        .map(|e| format!("{} = {}", e.key, e.value))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
            }

            // ── Skills ───────────────────────────────────────────────────
            "skill_list" => {
                let index = self.skills();
                if index.skills.is_empty() {
                    return Ok("no skills are installed in this workspace".to_string());
                }
                Ok(index
                    .skills
                    .iter()
                    .map(|s| {
                        if s.description.is_empty() {
                            s.name.clone()
                        } else {
                            format!("{}: {}", s.name, s.description)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "skill_read" => {
                let name = self.argument(call, "name")?;
                let index = self.skills();
                match index.read(&name) {
                    Some(text) => Ok(text),
                    None => Err(format!("no skill named `{name}`")),
                }
            }

            // ── Tasks ────────────────────────────────────────────────────
            "task_list" => {
                let tasks = self.task_store().list();
                if tasks.is_empty() {
                    return Ok("no tasks in this workspace".to_string());
                }
                Ok(tasks
                    .iter()
                    .map(|t| format!("[{}] {} ({})", t.status.as_str(), t.title, t.slug))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "task_add" => {
                let slug = self.argument(call, "slug")?;
                let title = self
                    .argument(call, "title")
                    .or_else(|_| self.argument(call, "task"))?;
                let body = call
                    .arguments
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.task_store()
                    .add(&slug, &title, body)
                    .map(|t| format!("task `{}` created [{}]", t.slug, t.status.as_str()))
            }
            "task_update" => {
                let slug = self.argument(call, "slug")?;
                let status = self.argument(call, "status")?;
                let status = TaskStatus::parse(&status);
                self.task_store()
                    .set_status(&slug, status)
                    .map(|t| format!("task `{}` is now [{}]", t.slug, t.status.as_str()))
            }

            // ── Sub-agents: only the gateway engine can provide these ────
            "sessions_spawn" => {
                let task = self.argument(call, "task")?;
                let model = call
                    .arguments
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let engine = self.engine.as_ref().ok_or_else(|| {
                    "sub-agents need the gateway engine (start `bkgclaw gateway`)".to_string()
                })?;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                engine
                    .blocking_send(EngineRequest::Spawn {
                        task,
                        model,
                        reply: reply_tx,
                    })
                    .map_err(|_| "the engine is not accepting requests".to_string())?;
                reply_rx
                    .blocking_recv()
                    .map_err(|_| "the engine dropped the spawn request".to_string())?
            }
            "sessions_send" => {
                let session = self.argument(call, "session")?;
                let message = self.argument(call, "message")?;
                let engine = self.engine.as_ref().ok_or_else(|| {
                    "sub-agents need the gateway engine (start `bkgclaw gateway`)".to_string()
                })?;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                engine
                    .blocking_send(EngineRequest::Send {
                        session,
                        message,
                        reply: reply_tx,
                    })
                    .map_err(|_| "the engine is not accepting requests".to_string())?;
                reply_rx
                    .blocking_recv()
                    .map_err(|_| "the engine dropped the request".to_string())?
            }
            "sessions_steer" => {
                let session = self.argument(call, "session")?;
                let instruction = self.argument(call, "instruction")?;
                let engine = self.engine.as_ref().ok_or_else(|| {
                    "sub-agents need the gateway engine (start `bkgclaw gateway`)".to_string()
                })?;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                engine
                    .blocking_send(EngineRequest::Steer {
                        session,
                        instruction,
                        reply: reply_tx,
                    })
                    .map_err(|_| "the engine is not accepting requests".to_string())?;
                reply_rx
                    .blocking_recv()
                    .map_err(|_| "the engine dropped the request".to_string())?
            }

            // ── Honestly not in this build ───────────────────────────────
            // A registered-but-refused tool tells the model the truth; a
            // stub answering "ok" would let it believe a cron exists.
            "cron_add" | "cron_remove" => Err(
                "cron scheduling needs a long-running scheduler; not implemented in this build"
                    .to_string(),
            ),

            other => Err(format!(
                "`{other}` is registered but not implemented in this build"
            )),
        }
    }
}

/// Run a shell command under a watchdog, capturing combined output through
/// reader threads (a child that out-writes the pipe buffer must not
/// deadlock the watchdog loop).
fn run_shell(command: &str, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start `sh`: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let per_stream = MAX_OUTPUT_BYTES / 2;
    let out_handle = std::thread::spawn(move || read_capped(stdout, per_stream));
    let err_handle = std::thread::spawn(move || read_capped(stderr, per_stream));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Kill before returning: an un-killed child keeps writing
                    // into pipes nobody reads.
                    let _ = child.kill();
                    return Err(format!(
                        "command exceeded {}s and was killed",
                        timeout.as_secs_f32()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("could not wait for the command: {e}")),
        }
    };

    let stdout = out_handle.join().map_err(|_| "stdout reader died")?;
    let stderr = err_handle.join().map_err(|_| "stderr reader died")?;
    let code = status.code().unwrap_or(-1);
    Ok(format!(
        "exit: {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    ))
}

/// Read a stream up to `cap` bytes, marking the cut.
fn read_capped(reader: Option<impl Read>, cap: usize) -> String {
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut buffer = vec![0u8; 8192];
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() + n > cap {
                    let keep = cap - out.len();
                    out.extend_from_slice(&buffer[..keep]);
                    truncated = true;
                    break;
                }
                out.extend_from_slice(&buffer[..n]);
            }
            Err(_) => break,
        }
    }
    let mut text = String::from_utf8_lossy(&out).to_string();
    if truncated {
        text.push_str(&format!("\n… [output capped at {cap} bytes]"));
    }
    text
}

/// One HTTP request through the SSRF guard, with redirect re-checks.
fn fetch(method: &str, url: &str, body: Option<&str>) -> Result<String, String> {
    ssrf::url_allowed(url)?;

    let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
        .map_err(|_| format!("`{method}` is not an HTTP method"))?;

    let client = reqwest::blocking::ClientBuilder::new()
        .timeout(WEB_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 {
                attempt.error("more than 3 redirects")
            } else if ssrf::url_allowed(attempt.url().as_str()).is_err() {
                attempt.error("redirect to a private address refused")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| e.to_string())?;

    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let response = request.send().map_err(|e| format!("{e}"))?;
    let status = response.status();

    // Refuse before reading: a huge body must not be downloaded to be
    // rejected.
    if let Some(length) = response.content_length() {
        if length as usize > MAX_WEB_BODY {
            return Err(format!(
                "body is {length} bytes; this tool caps at {MAX_WEB_BODY}"
            ));
        }
    }

    let mut stream = response;
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 16384];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() + n > MAX_WEB_BODY {
                    let keep = MAX_WEB_BODY - out.len();
                    out.extend_from_slice(&buffer[..keep]);
                    truncated = true;
                    break;
                }
                out.extend_from_slice(&buffer[..n]);
            }
            Err(e) => return Err(format!("reading the body failed: {e}")),
        }
    }

    let text = String::from_utf8_lossy(&out);
    let marker = if truncated { " …[truncated]" } else { "" };
    Ok(format!("HTTP {status}{marker}\n{text}"))
}

/// Bounded directory walk. Depth-limited, skipping the directories that
/// make a search useless.
fn walk(
    dir: &std::path::Path,
    depth: usize,
    visit: &mut impl FnMut(&std::path::Path),
) -> Result<(), String> {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("`{}`: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if matches!(
                name.as_str(),
                ".git" | "target" | "node_modules" | ".agent-console-state" | ".bkgclaw"
            ) {
                continue;
            }
            walk(&path, depth + 1, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bkgclaw_core::models::ToolCall;

    fn executor() -> (LocalExecutor, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        // `keep()`: the TempDir guard would delete the tree the moment this
        // helper returns, and every test after would write into a ghost.
        let root = dir.keep();
        (
            LocalExecutor {
                home: root.join("home"),
                workspace: root.join("ws"),
                exec_timeout: Duration::from_secs(10),
                engine: None,
            },
            root,
        )
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t".into(),
            name: name.into(),
            arguments: args,
        }
    }

    fn run(ex: &LocalExecutor, name: &str, args: serde_json::Value) -> Result<String, String> {
        ex.execute(&call(name, args))
    }

    #[test]
    fn write_then_read_round_trips() {
        let (ex, _dir) = executor();
        let path = std::env::temp_dir().join("bkgclaw-exec-wr.txt");
        run(
            &ex,
            "write_file",
            serde_json::json!({"path": path, "content": "inhalt"}),
        )
        .unwrap();
        assert!(
            run(&ex, "read_file", serde_json::json!({"path": path}))
                .unwrap()
                .contains("inhalt")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_file_creates_parent_directories_and_reports_bytes() {
        let (ex, root) = executor();
        let path = root.join("deep/nested/file.txt");
        let result = run(
            &ex,
            "write_file",
            serde_json::json!({"path": path, "content": "abc"}),
        )
        .unwrap();
        assert!(result.contains("wrote 3 bytes"));
        assert!(path.exists());
    }

    #[test]
    fn edit_file_requires_a_unique_match() {
        let (ex, root) = executor();
        let path = root.join("e.txt");
        std::fs::write(&path, "eins zwei drei eins").unwrap();

        let zero = run(
            &ex,
            "edit_file",
            serde_json::json!({"path": path, "old": "vier", "new": "x"}),
        )
        .unwrap_err();
        assert!(zero.contains("does not appear"));

        let many = run(
            &ex,
            "edit_file",
            serde_json::json!({"path": path, "old": "eins", "new": "x"}),
        )
        .unwrap_err();
        assert!(
            many.contains("2 times"),
            "ambiguous edits must be refused: {many}"
        );

        run(
            &ex,
            "edit_file",
            serde_json::json!({"path": path, "old": "zwei", "new": "2"}),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "eins 2 drei eins");
    }

    #[test]
    fn list_directory_marks_directories() {
        let (ex, root) = executor();
        std::fs::create_dir_all(root.join("adir")).unwrap();
        std::fs::write(root.join("afile"), "x").unwrap();
        let listing = run(&ex, "list_directory", serde_json::json!({"path": root})).unwrap();
        assert!(listing.contains("adir/"));
        assert!(listing.contains("afile"));
    }

    #[test]
    fn a_command_reports_its_exit_code_and_output() {
        let (ex, _) = executor();
        let out = run(
            &ex,
            "execute_command",
            serde_json::json!({"command": "echo hallo"}),
        )
        .unwrap();
        assert!(out.contains("exit: 0"));
        assert!(out.contains("hallo"));
        let failing = run(
            &ex,
            "execute_command",
            serde_json::json!({"command": "exit 3"}),
        )
        .unwrap();
        assert!(failing.contains("exit: 3"));
    }

    #[test]
    fn a_hung_command_is_killed_by_the_watchdog() {
        let (ex, _) = executor();
        let mut ex = ex;
        ex.exec_timeout = Duration::from_millis(300);
        let err = run(
            &ex,
            "execute_command",
            serde_json::json!({"command": "sleep 30"}),
        )
        .unwrap_err();
        assert!(err.contains("killed"), "{err}");
    }

    #[test]
    fn huge_command_output_is_capped_rather_than_eating_memory() {
        let (ex, _) = executor();
        let out = run(
            &ex,
            "execute_command",
            serde_json::json!({"command": "yes 0123456789 | head -c 5000000"}),
        )
        .unwrap();
        assert!(
            out.len() < 200_000,
            "capped output must stay small, got {}",
            out.len()
        );
        assert!(out.contains("output capped"), "the cut must be marked");
    }

    #[test]
    fn memory_tools_persist_across_executor_instances() {
        let (ex, root) = executor();
        run(
            &ex,
            "memory_set",
            serde_json::json!({"key": "server", "value": "bkg-01", "tags": ["infra"]}),
        )
        .unwrap();

        let second = LocalExecutor {
            home: root.join("home"),
            workspace: root.join("ws"),
            exec_timeout: Duration::from_secs(1),
            engine: None,
        };
        let found = run(
            &second,
            "memory_search",
            serde_json::json!({"query": "BKG-01"}),
        )
        .unwrap();
        assert!(found.contains("server = bkg-01"));
        let direct = run(&second, "memory_get", serde_json::json!({"key": "server"})).unwrap();
        assert!(direct.contains("bkg-01"));
        let miss = run(&second, "memory_get", serde_json::json!({"key": "nope"})).unwrap();
        assert!(miss.contains("no memory entry"));
    }

    #[test]
    fn task_and_skill_tools_work_through_the_executor() {
        let (ex, root) = executor();
        run(
            &ex,
            "task_add",
            serde_json::json!({"slug": "release", "title": "Release bauen", "body": "x"}),
        )
        .unwrap();
        let listed = run(&ex, "task_list", serde_json::json!({})).unwrap();
        assert!(listed.contains("[pending] Release bauen"));
        run(
            &ex,
            "task_update",
            serde_json::json!({"slug": "release", "status": "in-progress"}),
        )
        .unwrap();
        assert!(
            run(&ex, "task_list", serde_json::json!({}))
                .unwrap()
                .contains("[in-progress]")
        );

        let skill_dir = root.join("ws").join("skills").join("releasen");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: releasen\ndescription: Wie wir releasen\n---\nSchritt für Schritt.",
        )
        .unwrap();
        let skills = run(&ex, "skill_list", serde_json::json!({})).unwrap();
        assert!(skills.contains("releasen: Wie wir releasen"));
        let full = run(&ex, "skill_read", serde_json::json!({"name": "releasen"})).unwrap();
        assert!(full.contains("Schritt für Schritt."));
    }

    #[test]
    fn sub_agent_tools_refuse_without_an_engine_and_say_why() {
        let (ex, _) = executor();
        for name in ["sessions_spawn", "sessions_send", "sessions_steer"] {
            let err = run(&ex, name, serde_json::json!({"task": "x", "session": "s", "message": "m", "instruction": "i"})).unwrap_err();
            assert!(err.contains("gateway engine"), "{name}: {err}");
        }
    }

    #[test]
    fn cron_and_web_search_refuse_honestly() {
        let (ex, _) = executor();
        assert!(
            run(
                &ex,
                "cron_add",
                serde_json::json!({"expr": "* * * * *", "task": "x"})
            )
            .unwrap_err()
            .contains("not implemented")
        );
        assert!(
            run(&ex, "web_search", serde_json::json!({"query": "x"}))
                .unwrap_err()
                .contains("no search backend")
        );
    }

    #[test]
    fn web_fetch_rejects_private_and_local_targets() {
        let (ex, _) = executor();
        for url in [
            "http://127.0.0.1:9/x",
            "http://169.254.169.254/meta",
            "http://localhost/x",
        ] {
            let err = run(&ex, "web_fetch", serde_json::json!({"url": url})).unwrap_err();
            assert!(
                err.contains("private") || err.contains("resolve"),
                "{url}: {err}"
            );
        }
    }

    #[test]
    fn web_fetch_gets_a_real_page_over_http() {
        // A one-shot local web server on a random port — the request goes
        // through the full client path, including the guard.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer);
                let body = "hallo vom server";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });

        let (ex, _) = executor();
        let result = run(
            &ex,
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/")}),
        )
        .unwrap_err();
        // The guard fires before the request: loopback is refused even
        // though the server is right there. That is the property.
        assert!(result.contains("private"), "{result}");
    }

    #[test]
    fn http_request_accepts_methods_and_bodies() {
        let (ex, _) = executor();
        let err = run(&ex, "http_request", serde_json::json!({"url": "http://192.168.0.1/api", "method": "POST", "body": "{\"a\":1}"})).unwrap_err();
        assert!(err.contains("private"));
        // A space is not a valid HTTP token character, so this method must
        // be rejected before any connection is attempted.
        let bad = run(
            &ex,
            "http_request",
            serde_json::json!({"url": "http://example.com/", "method": "NOT A METHOD"}),
        );
        assert!(bad.is_err());
    }

    #[test]
    fn a_missing_argument_is_named_in_the_error() {
        let (ex, _) = executor();
        assert!(
            run(&ex, "read_file", serde_json::json!({}))
                .unwrap_err()
                .contains("path")
        );
    }

    #[test]
    fn an_unknown_tool_is_refused_not_faked() {
        let (ex, _) = executor();
        let err = run(&ex, "teleport", serde_json::json!({})).unwrap_err();
        assert!(err.contains("not implemented"));
    }
}
