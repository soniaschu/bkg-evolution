//! The model wiring, re-exported from core. This module exists so every
//! existing `bkgclaw_gateway::wiring::…` call keeps compiling; new code
//! should use `bkgclaw_core::wiring` directly.

pub use bkgclaw_core::wiring::*;
