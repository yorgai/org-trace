use anyhow::Result;

use crate::{list_native_source_sessions, NativeSourceSession, SourceProfile};

use super::{claude_code, codex_app, cursor_ide};

const SOURCE_CLAUDE_CODE: &str = "claude_code";
const SOURCE_CODEX_APP: &str = "codex_app";
const SOURCE_CURSOR_IDE: &str = "cursor_ide";

/// Lists native sessions through the app-specific provider for a source profile.
pub fn list_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    match profile.name.as_str() {
        SOURCE_CLAUDE_CODE => claude_code::list_sessions(profile, limit),
        SOURCE_CODEX_APP => codex_app::list_sessions(profile, limit),
        SOURCE_CURSOR_IDE => cursor_ide::list_sessions(profile, limit),
        _ => list_native_source_sessions(profile, limit),
    }
}
