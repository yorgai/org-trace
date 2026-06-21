//! Single source of truth for cross-surface default values.
//!
//! Brick exposes the same operations through two surfaces — the CLI (clap args)
//! and the MCP server (`tools/call` dispatch). Historically each surface declared
//! its own copy of "the default": a `100_000` literal in two modules, `"all"` /
//! `10` / `2000` defaults duplicated between `mcp.rs` consts and `args.rs` clap
//! attributes. Duplicated literals drift silently (e.g. the artifact-kind default
//! once disagreed: `Note` in MCP vs `Decision` in CLI). Centralizing them here
//! makes the two surfaces reference one definition, so they can never diverge.

use crate::args::ArtifactKindArg;

/// Ceiling on how many source sessions to index before answering a metadata
/// query/recall/export. Shared by `brick metadata` and `brick history`.
pub const SOURCE_REFRESH_LIMIT: usize = 100_000;

/// Default kind for a newly recorded artifact. `Note` is the neutral "I produced
/// something" default and matches the MCP path; the CLI previously defaulted to
/// `Decision`, which over-claimed. Both surfaces now use this single value.
pub const ARTIFACT_KIND: ArtifactKindArg = ArtifactKindArg::Note;
