# Cursor IDE fixtures

Add sanitized Cursor scenarios as text specs that generate temporary `state.vscdb` files during tests. Do not commit real `state.vscdb`, WAL, or SHM files.

Use `format: "cursor_kv_sqlite"` and a `dbSpecPath` pointing at a JSON spec with `cursorDiskKV` rows. The `generated_composer` fixture covers composer headers, `composerData`, `bubbleId` rows, content blob dereferencing, and plan registry extraction.
