//! Local tool execution — the hands of the agent.
//!
//! Everything the model can *do* happens here, behind the gate: read, write
//! and edit files, run a shell command with a watchdog, fetch the web
//! through an SSRF guard, remember, manage tasks and skills, and — when a
//! gateway engine is wired — spawn sub-agents.
//!
//! Three invariants:
//!
//! 1. **Every mutating tool reports exactly what it did.** `write_file`
//!    returns the path and byte count, `execute_command` the exit code.
//!    No tool ever answers "ok" without evidence.
//! 2. **Unbounded things are bounded.** Shell output is capped, web bodies
//!    are capped, commands are killed at a deadline.
//! 3. **What is not wired is refused honestly.** Sub-agents without a
//!    gateway say so; nothing fakes success.
#![forbid(unsafe_code)]

pub mod executor;
pub mod spawn;
pub mod ssrf;

pub use executor::{EXECUTE_TIMEOUT, LocalExecutor};
pub use spawn::EngineRequest;
pub use ssrf::url_allowed;
