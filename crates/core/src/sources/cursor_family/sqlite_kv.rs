use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

const CURSOR_DISK_KV_TABLE: &str = "cursorDiskKV";
const CURSOR_CONTENT_KEY_PREFIX: &str = "composer.content.";

pub(in crate::sources) fn open_state_db(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| {
        format!(
            "failed to open Cursor-family state DB at {}",
            path.display()
        )
    })
}

pub(in crate::sources) fn read_kv_value(
    connection: &Connection,
    key: &str,
) -> Result<Option<String>> {
    let sql = format!(
        "SELECT value FROM {} WHERE key = ?1 LIMIT 1",
        quote_identifier(CURSOR_DISK_KV_TABLE)
    );
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare Cursor-family KV lookup")?;
    let mut rows = statement
        .query([key])
        .context("failed to query Cursor-family KV value")?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

pub(in crate::sources) fn read_kv_entries_with_prefix(
    connection: &Connection,
    prefix: &str,
) -> Result<Vec<(String, String)>> {
    let sql = format!(
        "SELECT key, value FROM {} WHERE key LIKE ?1",
        quote_identifier(CURSOR_DISK_KV_TABLE)
    );
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare Cursor-family KV prefix lookup")?;
    let rows = statement
        .query_map([format!("{prefix}%")], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("failed to query Cursor-family KV prefix values")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read Cursor-family KV prefix rows")
}

#[derive(Debug)]
pub(in crate::sources) struct CursorContentResolver<'a> {
    connection: &'a Connection,
    seen_keys: HashSet<String>,
}

impl<'a> CursorContentResolver<'a> {
    pub(in crate::sources) fn new(connection: &'a Connection) -> Self {
        Self {
            connection,
            seen_keys: HashSet::new(),
        }
    }

    pub(in crate::sources) fn resolve_value(&mut self, value: &Value) -> Result<Value> {
        match value {
            Value::String(text) => self.resolve_string(text),
            Value::Array(items) => items
                .iter()
                .map(|item| self.resolve_value(item))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array),
            Value::Object(map) => {
                if let Some(content_key) = cursor_content_key_from_value(value) {
                    if let Some(content) = self.read_content_key(&content_key)? {
                        return Ok(content);
                    }
                }
                let mut resolved = serde_json::Map::with_capacity(map.len());
                for (key, child) in map {
                    if should_resolve_cursor_content_field(key) {
                        if let Some(content_key) = cursor_content_key_from_value(child) {
                            if let Some(content) = self.read_content_key(&content_key)? {
                                resolved.insert(key.clone(), content);
                                continue;
                            }
                        }
                    }
                    resolved.insert(key.clone(), self.resolve_value(child)?);
                }
                Ok(Value::Object(resolved))
            }
            _ => Ok(value.clone()),
        }
    }

    fn resolve_string(&mut self, text: &str) -> Result<Value> {
        let Some(content_key) = cursor_content_key_from_str(text) else {
            return Ok(Value::String(text.to_string()));
        };
        self.read_content_key(&content_key)
            .map(|content| content.unwrap_or_else(|| Value::String(text.to_string())))
    }

    fn read_content_key(&mut self, key: &str) -> Result<Option<Value>> {
        if !self.seen_keys.insert(key.to_string()) {
            return Ok(None);
        }
        let value = read_kv_value(self.connection, key)?;
        self.seen_keys.remove(key);
        value
            .map(|raw| self.resolve_value(&parse_cursor_blob_value(&raw)))
            .transpose()
    }
}

pub(in crate::sources) fn cursor_value_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        if text.trim().is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    } else if let Some(array) = value.as_array() {
        let text = array
            .iter()
            .filter_map(cursor_value_text)
            .collect::<Vec<_>>()
            .join("");
        (!text.trim().is_empty()).then_some(text)
    } else {
        value
            .get("text")
            .or_else(|| value.get("content"))
            .or_else(|| value.get("value"))
            .or_else(|| value.get("data"))
            .and_then(cursor_value_text)
    }
}

fn parse_cursor_blob_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn cursor_content_key_from_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .and_then(cursor_content_key_from_str)
        .or_else(|| {
            value
                .get("key")
                .or_else(|| value.get("contentKey"))
                .or_else(|| value.get("contentId"))
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .and_then(cursor_content_key_from_str)
        })
}

fn cursor_content_key_from_str(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let content_key = trimmed
        .strip_prefix("cursorDiskKV://")
        .unwrap_or(trimmed)
        .strip_prefix("cursor://")
        .unwrap_or(trimmed);
    if content_key.starts_with(CURSOR_CONTENT_KEY_PREFIX) {
        Some(content_key.to_string())
    } else if is_embedded_cursor_content_identifier(content_key) {
        Some(format!("{CURSOR_CONTENT_KEY_PREFIX}{content_key}"))
    } else {
        None
    }
}

fn should_resolve_cursor_content_field(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    normalized.contains("content")
        || normalized.ends_with("id")
        || normalized.ends_with("ids")
        || normalized.ends_with("key")
        || normalized.ends_with("keys")
        || normalized.contains("hash")
}

fn is_embedded_cursor_content_identifier(value: &str) -> bool {
    if let Some(hash) = value.strip_prefix("content-") {
        return is_probable_content_hash(hash);
    }
    is_probable_content_hash(value)
}

fn is_probable_content_hash(value: &str) -> bool {
    (16..=128).contains(&value.len()) && value.chars().all(|char| char.is_ascii_hexdigit())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
