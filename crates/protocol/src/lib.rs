//! Shared protocol types for Brick provenance events.
//!
//! This crate is the wire-format boundary between the CLI, local store,
//! importers, and the future self-hosted server. Keep these exports stable and
//! typed so callers do not pass raw strings for domain identifiers or event
//! names.

mod actor;
mod events;
mod ids;
mod payloads;
mod sync;
mod trace_event;

pub use actor::*;
pub use events::*;
pub use ids::*;
pub use payloads::*;
pub use sync::*;
pub use trace_event::*;
