use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::NativeSessionMetadata;

pub(super) const TITLE_LIMIT: usize = 200;

pub(super) fn read_jsonl_values(path: &Path) -> Result<Vec<Value>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read JSONL source file {}", path.display()))?;
    let mut values = Vec::new();
    for (line_index, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str(trimmed).with_context(|| {
            format!(
                "failed to parse JSONL line {} in {}",
                line_index + 1,
                path.display()
            )
        })?;
        values.push(value);
    }
    Ok(values)
}

pub(super) fn update_session_times(
    metadata: &mut NativeSessionMetadata,
    timestamp: Option<&Value>,
) {
    let Some(time) = timestamp
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_system_time)
    else {
        return;
    };
    metadata.session_created_at = Some(match metadata.session_created_at {
        Some(existing) => existing.min(time),
        None => time,
    });
    metadata.session_updated_at = Some(match metadata.session_updated_at {
        Some(existing) => existing.max(time),
        None => time,
    });
}

pub(super) fn set_first_path(target: &mut Option<PathBuf>, value: Option<&Value>) {
    if target.is_none() {
        *target = value.and_then(Value::as_str).map(PathBuf::from);
    }
}

pub(super) fn set_first_string(target: &mut Option<String>, value: Option<&Value>) {
    if target.is_none() {
        *target = value.and_then(Value::as_str).map(ToOwned::to_owned);
    }
}

pub(super) fn set_first_string_value(target: &mut Option<String>, value: &str) {
    if target.is_none() && !value.is_empty() {
        *target = Some(value.to_string());
    }
}

pub(super) fn text_from_value(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

pub(super) fn truncate_title(value: String) -> String {
    value.chars().take(TITLE_LIMIT).collect()
}

pub(super) fn token_value(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn parse_rfc3339_system_time(value: &str) -> Option<SystemTime> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc).into())
}
