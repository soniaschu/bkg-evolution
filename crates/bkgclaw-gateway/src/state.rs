//! Gateway state: sessions in memory, events on one bus, approvals pending.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use bkgclaw_core::models::{ModelRegistry, Vendor};
use bkgclaw_core::router::Candidate;
use bkgclaw_core::tools::{Decision, Policy};
use tokio::sync::{broadcast, mpsc};

use bkgclaw_exec::EngineRequest;
use bkgclaw_store::Session;

use crate::events::Event;

/// Everything the handlers share. `Arc` on the outside.
pub struct Inner {
    /// Active sessions by id. Loaded from disk on demand, kept until the
    /// process exits — a session that is active in the UI must not fall
    /// out of memory mid-turn.
    pub sessions: Mutex<HashMap<String, Arc<SessionSlot>>>,
    /// One bus for every client; events carry their session id.
    pub events: broadcast::Sender<Event>,
    /// Per-session event log for REST polling (`GET …/events?since=N`).
    /// Grows with the session's lifetime; the log is capped so a
    /// long-running gateway cannot eat memory with transcript deltas.
    pub logs: Mutex<HashMap<String, Vec<Event>>>,
    /// Approval answers waiting for the loop to ask. Tokio oneshots: the
    /// loop awaits them without parking a thread.
    pub approvals: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Decision>>>,
    /// Context for a pending approval: call id → (session id, tool name).
    /// Needed so an "always" answer can grant the tool by name on the
    /// right session, because the core `Decision` carries no tool identity.
    pub approval_context: Mutex<HashMap<String, (String, String)>>,
    /// The default policy for new sessions; `interactive_overrides` turns
    /// it into per-tool Ask decisions.
    pub default_policy: Mutex<Policy>,
    /// Channel for the sub-agent engine loop. The receiver is taken out
    /// once, by the loop, which then owns it exclusively — a std mutex
    /// guard may never be held across an await.
    pub engine_tx: mpsc::Sender<EngineRequest>,
    pub engine_rx: std::sync::Mutex<Option<mpsc::Receiver<EngineRequest>>>,
}

/// One session as the gateway holds it.
pub struct SessionSlot {
    /// The persisted session.
    pub session: Mutex<Session>,
    /// A turn is running. One at a time per session: two concurrent turns
    /// on one transcript would interleave tool calls into nonsense.
    pub busy: AtomicBool,
    /// The running turn's task handle. Aborting it cancels the turn; the
    /// commit happens only at the end of the task, so an aborted turn
    /// leaves the persisted session untouched.
    pub task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Tools the operator answered "always" to, this session. Shared with
    /// the turn's observer as the same `Arc`, so a grant lands live.
    pub allowed: Arc<Mutex<HashSet<String>>>,
}

impl SessionSlot {
    pub fn new(session: Session) -> Self {
        SessionSlot {
            session: Mutex::new(session),
            busy: AtomicBool::new(false),
            task: std::sync::Mutex::new(None),
            allowed: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

/// The gateway's static configuration and shared state.
pub struct Gateway {
    pub registry: ModelRegistry,
    pub chain: Vec<Candidate>,
    pub home: PathBuf,
    pub workspace: PathBuf,
    /// Bearer token; `None` = open (localhost binding is the boundary).
    pub token: Option<String>,
    /// Directory of built web assets, served at `/`.
    pub web_dir: Option<PathBuf>,
    /// Dollar ceiling per turn.
    pub turn_budget_usd: f64,
    pub inner: Arc<Inner>,
}

impl Gateway {
    pub fn new(
        registry: ModelRegistry,
        chain: Vec<Candidate>,
        home: PathBuf,
        workspace: PathBuf,
        token: Option<String>,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        let (engine_tx, engine_rx) = mpsc::channel(64);
        let inner = Inner {
            sessions: Mutex::new(HashMap::new()),
            events,
            logs: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            approval_context: Mutex::new(HashMap::new()),
            default_policy: Mutex::new(Policy::AllowReadOnly),
            engine_tx,
            engine_rx: std::sync::Mutex::new(Some(engine_rx)),
        };
        Gateway {
            registry,
            chain,
            home,
            workspace,
            token,
            web_dir: None,
            turn_budget_usd: 1.0,
            inner: Arc::new(inner),
        }
    }

    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// A session by id, loading from disk on first mention. Unknown ids
    /// are unknown — there is no session guessing.
    pub fn slot(&self, id: &str) -> Option<Arc<SessionSlot>> {
        let mut sessions = self.inner.sessions.lock().expect("sessions");
        if let Some(slot) = sessions.get(id) {
            return Some(slot.clone());
        }
        let session = Session::load(&self.home, id)?;
        let slot = Arc::new(SessionSlot::new(session));
        sessions.insert(id.to_string(), slot.clone());
        Some(slot)
    }

    /// Register a session created in this process.
    pub fn insert_session(&self, session: Session) -> Arc<SessionSlot> {
        let slot = Arc::new(SessionSlot::new(session));
        self.inner.sessions.lock().expect("sessions").insert(
            slot.session.lock().expect("session").id.clone(),
            slot.clone(),
        );
        slot
    }

    /// Publish an event: to the bus, and to the session's log.
    pub fn publish(&self, event: Event) {
        let session = event.session();
        // The log is capped: recent events are what polling needs; ancient
        // deltas are not.
        let mut logs = self.inner.logs.lock().expect("logs");
        let log = logs.entry(session.to_string()).or_default();
        if log.len() >= 2048 {
            let cut = log.len() / 4;
            log.drain(..cut);
        }
        log.push(event.clone());
        drop(logs);
        // A client gone is not an error; send() fails only with no
        // receivers.
        let _ = self.inner.events.send(event);
    }

    /// The events of one session after `since`.
    pub fn events_since(&self, session: &str, since: usize) -> Vec<Event> {
        self.inner
            .logs
            .lock()
            .expect("logs")
            .get(session)
            .map(|log| log.iter().skip(since).cloned().collect())
            .unwrap_or_default()
    }

    /// The interactive mapping from a policy to per-tool overrides. In a
    /// gateway there is always a human reachable, so what a policy would
    /// silently deny becomes a question instead: `allow-read-only` lets
    /// reads run and asks before writes; `allow-mutating` lets writes run
    /// and asks before the destructive class; `allow-all` asks nothing.
    pub fn interactive_overrides(policy: Policy) -> bkgclaw_core::tools::Overrides {
        let mut pairs: Vec<String> = Vec::new();
        let registry = bkgclaw_core::tools::builtin_tools();
        for tool in registry.all() {
            let ask = match policy {
                Policy::DenyAll => true,
                Policy::AllowReadOnly => !policy.permits(tool.risk),
                Policy::AllowMutating => !policy.permits(tool.risk),
                Policy::AllowAll => false,
            };
            if ask {
                pairs.push(format!("{}=ask", tool.name));
            }
        }
        bkgclaw_core::tools::Overrides::parse(&pairs.join(","))
    }

    /// The usable model ids for the models endpoint.
    pub fn usable_models(&self) -> Vec<(String, String, bool)> {
        let installed = self.registry.catalog();
        installed
            .into_iter()
            .map(|(model, usable)| (model.to_string(), model.vendor.as_str().to_string(), usable))
            .collect()
    }

    /// Whether this vendor is present at all (used by health).
    pub fn has_nim(&self) -> bool {
        self.registry.vendors().contains(&Vendor::Nim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bkgclaw_core::tools::{Decision, Risk};

    fn gateway() -> Gateway {
        Gateway::new(
            ModelRegistry::new(),
            Vec::new(),
            std::env::temp_dir(),
            std::env::temp_dir(),
            None,
        )
    }

    #[test]
    fn read_only_policy_asks_before_every_write_and_never_before_a_read() {
        let overrides = Gateway::interactive_overrides(Policy::AllowReadOnly);
        let registry = bkgclaw_core::tools::builtin_tools();
        assert_eq!(
            registry
                .gate("read_file", Policy::AllowAll, &overrides)
                .decision,
            Decision::Allow
        );
        for tool in ["write_file", "edit_file", "execute_command"] {
            assert_eq!(
                registry.gate(tool, Policy::AllowAll, &overrides).decision,
                Decision::Ask,
                "{tool} must surface for a human, not run silently"
            );
        }
    }

    #[test]
    fn mutating_policy_lets_writes_run_and_asks_before_destructive() {
        let overrides = Gateway::interactive_overrides(Policy::AllowMutating);
        let registry = bkgclaw_core::tools::builtin_tools();
        assert_eq!(
            registry
                .gate("write_file", Policy::AllowAll, &overrides)
                .decision,
            Decision::Allow
        );
        assert_eq!(
            registry
                .gate("execute_command", Policy::AllowAll, &overrides)
                .decision,
            Decision::Ask
        );
    }

    #[test]
    fn allow_all_asks_nothing_and_deny_all_asks_everything() {
        let registry = bkgclaw_core::tools::builtin_tools();
        let open = Gateway::interactive_overrides(Policy::AllowAll);
        for tool in registry.all() {
            assert_eq!(
                registry.gate(&tool.name, Policy::AllowAll, &open).decision,
                Decision::Allow
            );
        }
        let closed = Gateway::interactive_overrides(Policy::DenyAll);
        for tool in registry.all() {
            assert_eq!(
                registry
                    .gate(&tool.name, Policy::AllowAll, &closed)
                    .decision,
                Decision::Ask
            );
        }
    }

    #[test]
    fn every_risk_class_is_covered_by_the_override_map() {
        // If a new tool ships without landing in a mapping arm, this test
        // notices before a user does.
        let overrides = Gateway::interactive_overrides(Policy::AllowReadOnly);
        let registry = bkgclaw_core::tools::builtin_tools();
        for tool in registry.all() {
            let decision = registry
                .gate(&tool.name, Policy::AllowAll, &overrides)
                .decision;
            let expected = if tool.risk == Risk::ReadOnly {
                Decision::Allow
            } else {
                Decision::Ask
            };
            assert_eq!(decision, expected, "{}", tool.name);
        }
    }

    #[test]
    fn the_event_log_is_capped_and_keeps_the_recent_quarter() {
        let gw = gateway();
        for i in 0..2100 {
            gw.publish(Event::Stop {
                session: "s".into(),
                reason: format!("{i}"),
            });
        }
        let count = gw.events_since("s", 0).len();
        assert!(count <= 2048, "the log must be bounded, got {count}");
        let recent = gw.events_since("s", 0);
        assert!(
            recent.last().unwrap().session() == "s",
            "the newest events survive the cut"
        );
    }
}
