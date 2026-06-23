//! Claude hook adapter for Brick explain context.

use anyhow::Result;
use brick_core::LocalStore;

pub fn run_explain_hook(store: &LocalStore) -> Result<()> {
    use std::io::Read;

    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return Ok(());
    }
    let Some(file_path) = parse_hook_file_path(&raw) else {
        return Ok(());
    };

    let context = match build_explain_hook_context(store, &file_path) {
        Ok(Some(context)) => context,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!("brick explain-hook: {error}");
            return Ok(());
        }
    };

    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": context,
        }
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn build_explain_hook_context(store: &LocalStore, file_path: &str) -> Result<Option<String>> {
    let events = store.read_all_events()?;
    let index = store.load_or_rebuild_index()?;

    let rel = file_path.trim_start_matches("./");
    let mut matches: Vec<&brick_protocol::TraceEvent> = events
        .iter()
        .filter(|event| event.event_type == brick_protocol::EventType::DiffCaptured)
        .filter(|event| diff_touches_path(event, rel))
        .collect();
    if matches.is_empty() {
        return Ok(None);
    }
    matches.sort_by_key(|event| std::cmp::Reverse(event.occurred_at));
    let anchor_event = matches[0].event_id.to_string();

    let anchor = brick_core::resolve_direct_anchor(&events, &anchor_event);
    let chain =
        brick_core::explain_from_events(&index, &events, anchor, brick_core::DEFAULT_EXPLAIN_DEPTH);

    let whys: Vec<String> = chain
        .steps
        .iter()
        .filter_map(|step| step.note.clone())
        .collect();
    if whys.is_empty() {
        return Ok(None);
    }

    let mut out = String::new();
    out.push_str(
        "Brick causal memory — WHY this file looks the way it does (prefer this over \
re-deriving from the code or git log):\n",
    );
    for why in whys.iter().take(4) {
        out.push_str(&format!("- {}\n", truncate(why, 200)));
    }
    out.push_str(&format!(
        "Run `brick explain {rel}:<line>` for the full causal chain on a specific line."
    ));
    Ok(Some(out))
}

fn diff_touches_path(event: &brick_protocol::TraceEvent, rel: &str) -> bool {
    event
        .payload
        .get("file_changes")
        .and_then(|value| value.as_array())
        .map(|changes| {
            changes.iter().any(|change| {
                change
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|path| path == rel || path.ends_with(rel) || rel.ends_with(path))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn parse_hook_file_path(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let tool_input = value.get("tool_input")?;
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(path) = tool_input.get(key).and_then(serde_json::Value::as_str) {
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_caps_long_text() {
        let long = "x".repeat(200);
        let out = truncate(&long, 10);
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }
}
