use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::{json, Value};

use crate::{
    assistant_message_chunk, parse_inner_json, thinking_chunk, tool_call_chunk, user_message_chunk,
    ActivityChunk, ImportedToolCall, NativeSourceSession, SourceProfile, FUNCTION_EDIT_FILE,
    FUNCTION_RUN_COMMAND_LINE,
};

use super::jsonl::normalize_title;

const OPENCODE_SOURCE_ID: &str = "opencode";
const OPENCODE_SQLITE_PARSER_VERSION: &str = "opencode-sqlite-v1";
const OPENCODE_PROVIDER_SLUG: &str = "opencode";
const OPENCODE_DB_FILE: &str = "opencode.db";
const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Clone)]
struct TableSchema {
    columns: Vec<String>,
}

impl TableSchema {
    fn has(&self, column: &str) -> bool {
        self.columns.iter().any(|candidate| candidate == column)
    }

    fn first<'a>(&self, candidates: &[&'a str]) -> Option<&'a str> {
        candidates.iter().copied().find(|column| self.has(column))
    }
}

#[derive(Debug, Clone, Default)]
struct TokenTotals {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    let db_path = opencode_db_path(profile)?;
    let connection = open_opencode_db(&db_path)?;
    let session_schema = table_schema(&connection, "session")?;
    if !session_schema.has("id") {
        return Err(anyhow!(
            "OpenCode session table is missing required id column"
        ));
    }
    let part_schema = table_schema(&connection, "part").unwrap_or(TableSchema {
        columns: Vec::new(),
    });
    let message_schema = table_schema(&connection, "message").unwrap_or(TableSchema {
        columns: Vec::new(),
    });
    let part_token_totals = aggregate_part_tokens(&connection, &part_schema, &message_schema)?;
    let db_metadata = fs::metadata(&db_path).with_context(|| {
        format!(
            "failed to read OpenCode DB metadata at {}",
            db_path.display()
        )
    })?;
    let app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| OPENCODE_SOURCE_ID.to_string());
    let sql = session_query_sql(&session_schema, limit.unwrap_or(DEFAULT_LIMIT));
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare OpenCode session metadata query")?;
    let rows = statement
        .query_map([], |row| {
            let session_id = string_cell(row, 0)?;
            let part_tokens = part_token_totals
                .get(&session_id)
                .cloned()
                .unwrap_or_default();
            session_from_row(row, &db_path, &db_metadata, &app_id, &part_tokens)
        })
        .context("failed to query OpenCode sessions")?;
    // Incremental: `time_updated` is an epoch number (ms/seconds), not RFC3339,
    // so rather than bind a converted param we filter the cheap metadata rows in
    // Rust on the already-parsed `session_updated_at`. The DB read is small
    // (session table only); the real per-session cost is in `format_chunks`,
    // which incremental runs avoid by never re-indexing skipped sessions.
    let since = crate::since_to_system_time(since);
    let mut sessions = Vec::new();
    for row in rows {
        let session = row?;
        if let (Some(since), Some(updated)) = (since, session.session_updated_at) {
            if updated <= since {
                continue;
            }
        }
        sessions.push(session);
    }
    Ok(sessions)
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    let db_path = source_path.ok_or_else(|| {
        anyhow!("OpenCode source path missing for session: {external_session_id}")
    })?;
    let connection = open_opencode_db(db_path)?;
    let part_schema = table_schema(&connection, "part")?;
    let message_schema = table_schema(&connection, "message")?;
    if !part_schema.has("message_id") || !message_schema.has("id") {
        return Ok(Vec::new());
    }
    let Some(session_filter) = session_filter_expression(&part_schema, &message_schema) else {
        return Ok(Vec::new());
    };

    let role_expression = role_expression(&message_schema);
    let part_type_expression = part_type_expression(&part_schema);
    let part_time_expression = optional_column_expression(&part_schema, "p", "time_created");
    let part_data_expression = optional_column_expression(&part_schema, "p", "data");
    let message_data_expression = optional_column_expression(&message_schema, "m", "data");
    let sql = format!(
        "SELECT p.{part_id}, p.{message_id}, {role_expression} AS role, \
         {part_type_expression} AS part_type, {part_data_expression} AS part_data, \
         {part_time_expression} AS part_time_created, {message_data_expression} AS message_data \
         FROM {part_table} p JOIN {message_table} m ON m.{message_pk} = p.{part_message_id} \
         WHERE {session_filter} = ?1 \
         ORDER BY {part_order}, p.{part_id}",
        part_id = quote_identifier("id"),
        message_id = quote_identifier("message_id"),
        message_pk = quote_identifier("id"),
        part_message_id = quote_identifier("message_id"),
        part_table = quote_identifier("part"),
        message_table = quote_identifier("message"),
        part_order = if part_schema.has("time_created") {
            format!("p.{}", quote_identifier("time_created"))
        } else {
            format!("p.{}", quote_identifier("id"))
        },
    );
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare OpenCode chunk query")?;
    let rows = statement
        .query_map([external_session_id], opencode_chunk_row)
        .context("failed to query OpenCode chunks")?;
    let mut chunks = Vec::new();
    let mut sequence = 0_usize;
    for row in rows {
        if let Some(mut chunk) = chunk_from_part(row?, external_session_id, sequence) {
            let message_id = chunk.source_message_id.clone();
            let part_id = chunk.source_part_id.clone();
            chunk.set_source_pointer(
                OPENCODE_SOURCE_ID,
                db_path,
                None,
                None,
                message_id.as_deref(),
                part_id.as_deref(),
            );
            chunks.push(chunk);
            sequence += 1;
        }
    }
    Ok(chunks)
}

fn opencode_db_path(profile: &SourceProfile) -> Result<PathBuf> {
    if let Some(path) = &profile.session_db_path {
        return Ok(path.clone());
    }
    for path in [&profile.session_log_path, &profile.evidence_root]
        .into_iter()
        .flatten()
    {
        if path.is_file() {
            return Ok(path.clone());
        }
        let candidate = path.join(OPENCODE_DB_FILE);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "opencode source requires session_db_path or a profile path containing opencode.db"
    ))
}

fn open_opencode_db(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open OpenCode DB at {}", path.display()))
}

fn table_schema(connection: &Connection, table: &str) -> Result<TableSchema> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut statement = connection
        .prepare(&sql)
        .with_context(|| format!("failed to inspect OpenCode {table} table"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(TableSchema { columns })
}

fn session_query_sql(schema: &TableSchema, limit: usize) -> String {
    let title = optional_column_expression(schema, "s", "title");
    let directory = optional_column_expression(schema, "s", "directory");
    let model = optional_column_expression(schema, "s", "model");
    let time_created = optional_column_expression(schema, "s", "time_created");
    let time_updated = optional_column_expression(schema, "s", "time_updated");
    let tokens_input = optional_column_expression(schema, "s", "tokens_input");
    let tokens_cache_read = optional_column_expression(schema, "s", "tokens_cache_read");
    let tokens_cache_write = optional_column_expression(schema, "s", "tokens_cache_write");
    let tokens_output = optional_column_expression(schema, "s", "tokens_output");
    let tokens_reasoning = optional_column_expression(schema, "s", "tokens_reasoning");
    let archive_filter = archive_filter(schema);
    let order_by = if schema.has("time_updated") {
        format!("s.{} DESC", quote_identifier("time_updated"))
    } else if schema.has("time_created") {
        format!("s.{} DESC", quote_identifier("time_created"))
    } else {
        format!("s.{} DESC", quote_identifier("id"))
    };
    format!(
        "SELECT s.{id}, {title}, {directory}, {model}, {time_created}, {time_updated}, \
         {tokens_input}, {tokens_cache_read}, {tokens_cache_write}, {tokens_output}, {tokens_reasoning} \
         FROM {session_table} s {archive_filter} ORDER BY {order_by} LIMIT {limit}",
        id = quote_identifier("id"),
        session_table = quote_identifier("session"),
    )
}

fn archive_filter(schema: &TableSchema) -> String {
    let mut filters = Vec::new();
    if schema.has("time_archived") {
        filters.push(format!("s.{} IS NULL", quote_identifier("time_archived")));
    }
    for column in ["archived", "is_archived", "isArchived"] {
        if schema.has(column) {
            filters.push(format!("COALESCE(s.{}, 0) = 0", quote_identifier(column)));
        }
    }
    if filters.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", filters.join(" AND "))
    }
}

fn optional_column_expression(schema: &TableSchema, alias: &str, column: &str) -> String {
    if schema.has(column) {
        format!("{alias}.{}", quote_identifier(column))
    } else {
        "NULL".to_string()
    }
}

fn session_from_row(
    row: &Row<'_>,
    db_path: &Path,
    db_metadata: &fs::Metadata,
    source_app_id: &str,
    part_tokens: &TokenTotals,
) -> rusqlite::Result<NativeSourceSession> {
    let external_session_id = string_cell(row, 0)?;
    let title = optional_string_cell(row, 1)?.map(normalize_title);
    let repo_path = optional_string_cell(row, 2)?.map(PathBuf::from);
    let model = optional_string_cell(row, 3)?.and_then(|raw| model_name(&raw));
    let session_created_at = system_time_cell(row, 4)?;
    let session_updated_at = system_time_cell(row, 5)?.or(session_created_at);
    let session_input_tokens = token_sum_cell(row, &[6, 7, 8])?;
    let session_output_tokens = token_sum_cell(row, &[9, 10])?;
    Ok(NativeSourceSession {
        title: title.or_else(|| Some(external_session_id.clone())),
        external_session_id,
        source_app_id: source_app_id.to_string(),
        path: db_path.to_path_buf(),
        size_bytes: db_metadata.len(),
        modified_at: db_metadata.modified().ok(),
        parser_version: OPENCODE_SQLITE_PARSER_VERSION.to_string(),
        session_created_at,
        session_updated_at,
        model,
        input_tokens: session_input_tokens.or(part_tokens.input_tokens),
        output_tokens: session_output_tokens.or(part_tokens.output_tokens),
        repo_path,
        branch: None,
        files_changed: None,
        lines_added: None,
        lines_removed: None,
        touched_files: Vec::new(),
        listable: true,
        metadata_json: None,
        cwd: None,
        liveness: crate::Liveness::Unknown,
        last_activity: None,
    })
}

fn aggregate_part_tokens(
    connection: &Connection,
    part_schema: &TableSchema,
    message_schema: &TableSchema,
) -> Result<std::collections::HashMap<String, TokenTotals>> {
    let token_columns = [
        "tokens_input",
        "tokens_cache_read",
        "tokens_cache_write",
        "tokens_output",
        "tokens_reasoning",
    ];
    if !token_columns.iter().any(|column| part_schema.has(column)) {
        return Ok(std::collections::HashMap::new());
    }
    if !part_schema.has("message_id") || !message_schema.has("id") {
        return Ok(std::collections::HashMap::new());
    }
    let Some(session_expression) = session_filter_expression(part_schema, message_schema) else {
        return Ok(std::collections::HashMap::new());
    };
    let input_expression = token_sum_expression(
        part_schema,
        "p",
        &["tokens_input", "tokens_cache_read", "tokens_cache_write"],
    );
    let output_expression =
        token_sum_expression(part_schema, "p", &["tokens_output", "tokens_reasoning"]);
    let sql = format!(
        "SELECT {session_expression} AS session_id, SUM({input_expression}) AS input_tokens, \
         SUM({output_expression}) AS output_tokens FROM {part_table} p \
         JOIN {message_table} m ON m.{message_pk} = p.{part_message_id} \
         GROUP BY {session_expression}",
        part_table = quote_identifier("part"),
        message_table = quote_identifier("message"),
        message_pk = quote_identifier("id"),
        part_message_id = quote_identifier("message_id"),
    );
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare OpenCode part token aggregation")?;
    let rows = statement.query_map([], |row| {
        Ok((
            string_cell(row, 0)?,
            TokenTotals {
                input_tokens: u64_cell(row, 1)?,
                output_tokens: u64_cell(row, 2)?,
            },
        ))
    })?;
    let mut totals = std::collections::HashMap::new();
    for row in rows {
        let (session_id, token_totals) = row?;
        totals.insert(session_id, token_totals);
    }
    Ok(totals)
}

fn session_filter_expression(
    part_schema: &TableSchema,
    message_schema: &TableSchema,
) -> Option<String> {
    if part_schema.has("session_id") {
        Some(format!("p.{}", quote_identifier("session_id")))
    } else if message_schema.has("session_id") {
        Some(format!("m.{}", quote_identifier("session_id")))
    } else {
        None
    }
}

fn token_sum_expression(schema: &TableSchema, alias: &str, columns: &[&str]) -> String {
    let parts = columns
        .iter()
        .filter(|column| schema.has(column))
        .map(|column| format!("COALESCE({alias}.{}, 0)", quote_identifier(column)))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(" + ")
    }
}

fn role_expression(message_schema: &TableSchema) -> String {
    if let Some(column) = message_schema.first(&["role", "author", "sender"]) {
        format!("m.{}", quote_identifier(column))
    } else if message_schema.has("data") {
        "CASE WHEN json_valid(m.data) THEN json_extract(m.data, '$.role') END".to_string()
    } else {
        "NULL".to_string()
    }
}

fn part_type_expression(part_schema: &TableSchema) -> String {
    if part_schema.has("type") {
        format!("p.{}", quote_identifier("type"))
    } else if part_schema.has("data") {
        "CASE WHEN json_valid(p.data) THEN json_extract(p.data, '$.type') END".to_string()
    } else {
        "NULL".to_string()
    }
}

#[derive(Debug)]
struct OpenCodePartRow {
    part_id: String,
    message_id: String,
    role: Option<String>,
    part_type: Option<String>,
    part_data: Option<String>,
    part_time_created: String,
    message_data: Option<String>,
}

fn opencode_chunk_row(row: &Row<'_>) -> rusqlite::Result<OpenCodePartRow> {
    Ok(OpenCodePartRow {
        part_id: string_cell(row, 0)?,
        message_id: string_cell(row, 1)?,
        role: optional_string_cell(row, 2)?,
        part_type: optional_string_cell(row, 3)?,
        part_data: optional_string_cell(row, 4)?,
        part_time_created: rfc3339_cell(row, 5)?.unwrap_or_default(),
        message_data: optional_string_cell(row, 6)?,
    })
}

fn chunk_from_part(
    row: OpenCodePartRow,
    external_session_id: &str,
    sequence: usize,
) -> Option<ActivityChunk> {
    let part_data = row
        .part_data
        .as_deref()
        .map(parse_json_or_string)
        .unwrap_or_else(|| json!({}));
    let message_data = row
        .message_data
        .as_deref()
        .map(parse_json_or_string)
        .unwrap_or_else(|| json!({}));
    let role = row
        .role
        .clone()
        .or_else(|| string_field(&message_data, &["role", "author", "sender"]));
    let part_type = row
        .part_type
        .clone()
        .or_else(|| string_field(&part_data, &["type", "kind"]))
        .unwrap_or_else(|| infer_part_type(&part_data));
    let mut chunk = match part_type.as_str() {
        "text" | "message" => text_from_part(&part_data).map(|text| {
            if role.as_deref() == Some("user") {
                user_message_chunk(
                    external_session_id,
                    OPENCODE_PROVIDER_SLUG,
                    sequence,
                    &row.part_time_created,
                    &text,
                )
            } else {
                assistant_message_chunk(
                    external_session_id,
                    OPENCODE_PROVIDER_SLUG,
                    sequence,
                    &row.part_time_created,
                    &text,
                )
            }
        }),
        "reasoning" | "thinking" => text_from_part(&part_data).map(|text| {
            thinking_chunk(
                external_session_id,
                OPENCODE_PROVIDER_SLUG,
                sequence,
                &row.part_time_created,
                &text,
            )
        }),
        "tool" | "tool_call" => tool_call_from_part(&row, &part_data).map(|(call, output)| {
            tool_call_chunk(
                external_session_id,
                OPENCODE_PROVIDER_SLUG,
                sequence,
                &call,
                &output,
            )
        }),
        _ => text_from_part(&part_data).map(|text| {
            if role.as_deref() == Some("user") {
                user_message_chunk(
                    external_session_id,
                    OPENCODE_PROVIDER_SLUG,
                    sequence,
                    &row.part_time_created,
                    &text,
                )
            } else {
                assistant_message_chunk(
                    external_session_id,
                    OPENCODE_PROVIDER_SLUG,
                    sequence,
                    &row.part_time_created,
                    &text,
                )
            }
        }),
    }?;
    chunk.source_message_id = Some(row.message_id);
    chunk.source_part_id = Some(row.part_id);
    Some(chunk)
}

fn tool_call_from_part(row: &OpenCodePartRow, value: &Value) -> Option<(ImportedToolCall, String)> {
    let raw_name = string_field(
        value,
        &["name", "tool", "tool_name", "toolName", "function"],
    )
    .unwrap_or_else(|| "tool".to_string());
    let call_id = string_field(value, &["call_id", "tool_call_id", "toolCallId", "id"])
        .unwrap_or_else(|| row.part_id.clone());
    let args = value
        .get("arguments")
        .or_else(|| value.get("args"))
        .or_else(|| value.get("input"))
        .map(tool_payload_json)
        .unwrap_or_else(|| json!({}));
    let output = value
        .get("output")
        .or_else(|| value.get("result"))
        .or_else(|| value.get("content"))
        .and_then(text_value)
        .unwrap_or_default();
    Some((
        ImportedToolCall {
            call_id,
            raw_name: raw_name.clone(),
            canonical_name: canonical_tool_name(&raw_name),
            args,
            created_at: row.part_time_created.clone(),
        },
        output,
    ))
}

fn canonical_tool_name(raw_name: &str) -> String {
    let normalized = raw_name.to_ascii_lowercase();
    if normalized.contains("bash")
        || normalized.contains("shell")
        || normalized.contains("execute")
        || normalized.contains("command")
    {
        FUNCTION_RUN_COMMAND_LINE.to_string()
    } else if normalized.contains("write")
        || normalized.contains("edit")
        || normalized.contains("patch")
    {
        FUNCTION_EDIT_FILE.to_string()
    } else {
        raw_name.to_string()
    }
}

fn tool_payload_json(value: &Value) -> Value {
    value
        .as_str()
        .map(parse_inner_json)
        .unwrap_or_else(|| value.clone())
}

fn text_from_part(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(text_value)
        .or_else(|| value.get("content").and_then(text_value))
        .or_else(|| value.get("message").and_then(text_value))
        .or_else(|| value.as_str().map(ToOwned::to_owned))
}

fn text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(text_value)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(_) => value
            .get("text")
            .or_else(|| value.get("content"))
            .and_then(text_value),
        _ => None,
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(|value| value.as_str().map(ToOwned::to_owned))
}

fn infer_part_type(value: &Value) -> String {
    if string_field(value, &["name", "tool", "tool_name", "toolName"]).is_some() {
        "tool".to_string()
    } else if value.get("reasoning").is_some() || value.get("thinking").is_some() {
        "reasoning".to_string()
    } else {
        "text".to_string()
    }
}

fn parse_json_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn model_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return ["id", "modelId", "model_id", "providerId", "provider_id"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .or_else(|| value.as_str().map(ToOwned::to_owned));
    }
    Some(trimmed.to_string())
}

fn token_sum_cell(row: &Row<'_>, columns: &[usize]) -> rusqlite::Result<Option<u64>> {
    let mut total = 0_u64;
    let mut saw_value = false;
    for column in columns {
        if let Some(value) = u64_cell(row, *column)? {
            total = total.saturating_add(value);
            saw_value = true;
        }
    }
    Ok(saw_value.then_some(total))
}

fn string_cell(row: &Row<'_>, index: usize) -> rusqlite::Result<String> {
    Ok(optional_string_cell(row, index)?.unwrap_or_default())
}

fn optional_string_cell(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) => Ok(Some(String::from_utf8_lossy(bytes).into_owned())),
        ValueRef::Integer(value) => Ok(Some(value.to_string())),
        ValueRef::Real(value) => Ok(Some(value.to_string())),
        ValueRef::Blob(bytes) => Ok(Some(String::from_utf8_lossy(bytes).into_owned())),
    }
}

fn u64_cell(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(value) => Ok(u64::try_from(value).ok()),
        ValueRef::Real(value) if value >= 0.0 => Ok(Some(value as u64)),
        ValueRef::Text(bytes) => Ok(String::from_utf8_lossy(bytes).parse::<u64>().ok()),
        ValueRef::Blob(_) | ValueRef::Real(_) => Ok(None),
    }
}

fn system_time_cell(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<SystemTime>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(value) => Ok(system_time_from_number(value)),
        ValueRef::Real(value) => Ok(system_time_from_number(value as i64)),
        ValueRef::Text(bytes) => Ok(system_time_from_text(&String::from_utf8_lossy(bytes))),
        ValueRef::Blob(_) => Ok(None),
    }
}

fn rfc3339_cell(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(index)? {
        ValueRef::Null => None,
        ValueRef::Integer(value) => system_time_from_number(value).map(system_time_to_rfc3339),
        ValueRef::Real(value) => system_time_from_number(value as i64).map(system_time_to_rfc3339),
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            system_time_from_text(&text)
                .map(system_time_to_rfc3339)
                .or_else(|| Some(text.into_owned()))
        }
        ValueRef::Blob(_) => None,
    })
}

fn system_time_from_text(value: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc).into())
        .or_else(|| value.parse::<i64>().ok().and_then(system_time_from_number))
}

fn system_time_from_number(value: i64) -> Option<SystemTime> {
    let unsigned = u64::try_from(value).ok()?;
    let duration = if unsigned > 10_000_000_000 {
        Duration::from_millis(unsigned)
    } else {
        Duration::from_secs(unsigned)
    };
    Some(UNIX_EPOCH + duration)
}

fn system_time_to_rfc3339(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339()
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::{
        ACTION_TYPE_ASSISTANT, ACTION_TYPE_RAW, ACTION_TYPE_THINKING, ACTION_TYPE_TOOL_CALL,
        FUNCTION_ASSISTANT, FUNCTION_RUN_COMMAND_LINE, FUNCTION_THINKING, FUNCTION_USER_MESSAGE,
    };

    fn temp_opencode_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brick-opencode-{name}-{}.db",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn profile(path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: OPENCODE_SOURCE_ID.to_string(),
            app_id: Some(OPENCODE_SOURCE_ID.to_string()),
            actor_id: None,
            actor_type: None,
            store_root: None,
            session_db_path: Some(path),
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    #[test]
    fn extracts_sessions_from_opencode_db_with_session_tokens() {
        let path = temp_opencode_db("session-tokens");
        let connection = Connection::open(&path).expect("open temp OpenCode DB");
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    title TEXT,
                    directory TEXT,
                    model TEXT,
                    tokens_input INTEGER,
                    tokens_cache_read INTEGER,
                    tokens_cache_write INTEGER,
                    tokens_output INTEGER,
                    tokens_reasoning INTEGER,
                    time_created INTEGER,
                    time_updated INTEGER,
                    time_archived INTEGER
                );
                INSERT INTO session VALUES (
                    'session-1', 'Build OpenCode provider', '/workspace/repo',
                    '{\"id\":\"anthropic/claude\"}', 10, 3, 2, 7, 4,
                    1766000000000, 1766000060000, NULL
                );
                INSERT INTO session VALUES (
                    'archived', 'Hidden', '/workspace/repo', 'model', 1, 1, 1, 1, 1,
                    1766000000000, 1766000060000, 1766000070000
                );",
            )
            .expect("create OpenCode session fixture");
        drop(connection);

        let sessions =
            list_sessions(&profile(path), Some(10), None).expect("list OpenCode sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, "session-1");
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Build OpenCode provider")
        );
        assert_eq!(
            sessions[0].repo_path.as_deref(),
            Some(Path::new("/workspace/repo"))
        );
        assert_eq!(sessions[0].model.as_deref(), Some("anthropic/claude"));
        assert_eq!(sessions[0].input_tokens, Some(15));
        assert_eq!(sessions[0].output_tokens, Some(11));
        assert_eq!(sessions[0].parser_version, OPENCODE_SQLITE_PARSER_VERSION);
    }

    #[test]
    fn aggregates_part_tokens_when_session_tokens_are_absent() {
        let path = temp_opencode_db("part-tokens");
        let connection = Connection::open(&path).expect("open temp OpenCode DB");
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    title TEXT,
                    directory TEXT,
                    model TEXT,
                    time_created INTEGER,
                    time_updated INTEGER
                );
                CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
                CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    session_id TEXT,
                    message_id TEXT,
                    data TEXT,
                    tokens_input INTEGER,
                    tokens_cache_read INTEGER,
                    tokens_cache_write INTEGER,
                    tokens_output INTEGER,
                    tokens_reasoning INTEGER
                );
                INSERT INTO session VALUES ('session-1', 'Token fallback', '/workspace/repo', 'gpt', 1766000000, 1766000060);
                INSERT INTO message VALUES ('message-1', 'session-1', '{\"role\":\"assistant\"}');
                INSERT INTO part VALUES ('part-1', 'session-1', 'message-1', '{}', 10, 2, 1, 5, 3);
                INSERT INTO part VALUES ('part-2', 'session-1', 'message-1', '{}', 4, 1, 0, 2, 1);",
            )
            .expect("create OpenCode token fixture");
        drop(connection);

        let sessions =
            list_sessions(&profile(path), Some(10), None).expect("list OpenCode sessions");

        assert_eq!(sessions[0].input_tokens, Some(18));
        assert_eq!(sessions[0].output_tokens, Some(11));
    }

    #[test]
    fn formats_opencode_parts_as_chunks() {
        let path = temp_opencode_db("chunks");
        let connection = Connection::open(&path).expect("open temp OpenCode DB");
        connection
            .execute_batch(
                "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
                CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    session_id TEXT,
                    message_id TEXT,
                    type TEXT,
                    data TEXT,
                    time_created INTEGER
                );
                INSERT INTO message VALUES ('message-user', 'session-1', '{\"role\":\"user\"}');
                INSERT INTO message VALUES ('message-assistant', 'session-1', '{\"role\":\"assistant\"}');
                INSERT INTO part VALUES ('part-user', 'session-1', 'message-user', 'text', '{\"text\":\"Run tests\"}', 1766000000000);
                INSERT INTO part VALUES ('part-assistant', 'session-1', 'message-assistant', 'text', '{\"text\":\"I will run them.\"}', 1766000001000);
                INSERT INTO part VALUES ('part-thinking', 'session-1', 'message-assistant', 'reasoning', '{\"text\":\"Need cargo test.\"}', 1766000002000);
                INSERT INTO part VALUES ('part-tool', 'session-1', 'message-assistant', 'tool', '{\"name\":\"bash\",\"arguments\":{\"command\":\"cargo test\"},\"output\":\"ok\"}', 1766000003000);",
            )
            .expect("create OpenCode chunk fixture");
        drop(connection);

        let chunks = format_chunks("session-1", Some(&path)).expect("format OpenCode chunks");

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_RAW);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(chunks[1].action_type, ACTION_TYPE_ASSISTANT);
        assert_eq!(chunks[1].function, FUNCTION_ASSISTANT);
        assert_eq!(chunks[2].action_type, ACTION_TYPE_THINKING);
        assert_eq!(chunks[2].function, FUNCTION_THINKING);
        assert_eq!(chunks[3].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[3].function, FUNCTION_RUN_COMMAND_LINE);
        assert_eq!(chunks[3].args["command"], "cargo test");
        assert_eq!(chunks[3].result["output"], "ok");
        assert_eq!(chunks[0].source_id.as_deref(), Some(OPENCODE_SOURCE_ID));
        assert_eq!(
            chunks[0].source_path.as_deref(),
            Some(path.display().to_string().as_str())
        );
        assert_eq!(chunks[0].source_message_id.as_deref(), Some("message-user"));
        assert_eq!(chunks[0].source_part_id.as_deref(), Some("part-user"));
        assert_eq!(
            chunks[3].source_message_id.as_deref(),
            Some("message-assistant")
        );
        assert_eq!(chunks[3].source_part_id.as_deref(), Some("part-tool"));
    }
}
