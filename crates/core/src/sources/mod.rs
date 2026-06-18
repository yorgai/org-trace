//! App-specific native history source providers.

mod claude_code;
mod codex_app;
mod cursor_family;
mod cursor_ide;
mod jsonl;
mod opencode;
mod traits;
mod windsurf;

pub use traits::*;
