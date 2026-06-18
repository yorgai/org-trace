# Windsurf fixtures

Add sanitized Windsurf scenarios as text specs that generate temporary `state.vscdb` files during tests. Do not commit real `state.vscdb`, WAL, or SHM files.

Use `format: "cursor_kv_sqlite"` and a `dbSpecPath` pointing at a JSON spec with Cursor-family `composerData` and `bubbleId` rows. The `generated_composer` fixture validates the shared composer parser path with Windsurf-specific metadata such as context tokens.
