//! Local-first storage, indexing, identity, and repository helpers.
//!
//! This crate owns local behavior only: unified event/chunk storage, rebuildable
//! cache indexes, Git context capture, and identity resolution. Server sync and
//! auth should remain outside this crate until the local recorder is stable.

mod activity;
mod attachment_store;
mod blame;
mod diff_capture;
mod explain;
mod file_session_blame;
mod global_home;
mod identity;
mod index;
mod index_types;
mod local_event_store;
mod log_store;
mod metadata_db;
mod native_source;
mod plan_metadata;
mod repo;
mod repo_context;
mod session_query;
mod source_discovery;
mod source_profile;
mod source_session_event;
mod sources;
mod sqlite_index;
#[cfg(test)]
mod sqlite_index_tests;
mod sqlite_schema;
mod store;
mod store_options;

pub use activity::*;
pub use attachment_store::*;
pub use blame::*;
pub use diff_capture::*;
pub use explain::*;
pub use file_session_blame::*;
pub use global_home::*;
pub use identity::*;
pub use index_types::*;
pub use local_event_store::*;
pub use log_store::*;
pub use metadata_db::*;
pub use native_source::*;
pub use plan_metadata::*;
pub use repo::*;
pub use repo_context::*;
pub use session_query::*;
pub use source_discovery::*;
pub use source_profile::*;
pub use source_session_event::*;
pub use sources::*;
pub use sqlite_index::*;
pub use store::*;
pub use store_options::*;

pub const BRICK_DIR: &str = ".brick";
pub const PROVENANCE_DIR: &str = ".brick/provenance";
pub const QUEUE_DIR: &str = "queue";
pub const EVENTS_DIR: &str = "events";
pub const CACHE_DIR: &str = "cache";
pub const BLOBS_DIR: &str = "blobs";
pub const VIEWS_DIR: &str = "views";
pub const REPO_CONFIG_FILE: &str = "repo.json";
pub const BRICK_CONFIG_FILE: &str = "config.toml";
pub const CURRENT_CONTEXT_FILE: &str = "current.json";
pub const SOURCE_PROFILES_DIR: &str = "sources";
pub const CURRENT_SOURCE_FILE: &str = "source-current.toml";
