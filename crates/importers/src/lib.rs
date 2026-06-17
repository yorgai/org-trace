use anyhow::Result;
use org_trace_protocol::TraceEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    ClaudeCode,
    Codex,
    Cursor,
}

pub fn import_traces(_source: ImportSource) -> Result<Vec<TraceEvent>> {
    Ok(Vec::new())
}
