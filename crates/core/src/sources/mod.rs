//! App-specific native history source providers.

mod claude_code;
mod codex_app;
mod cursor_agent;
mod cursor_family;
mod cursor_ide;
mod gemini;
mod jsonl;
pub mod liveness;
mod opencode;
mod orgii;
pub(crate) mod shell_edits;
mod traits;
mod windsurf;

pub use traits::*;
