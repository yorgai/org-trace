use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

const CURSOR_DISK_KV_TABLE: &str = "cursorDiskKV";

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

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
