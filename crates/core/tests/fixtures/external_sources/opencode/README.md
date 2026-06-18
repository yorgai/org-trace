# OpenCode fixtures

Add sanitized OpenCode scenarios as SQL specs that generate temporary `opencode.db` files during tests. Do not commit real user databases.

Use `format: "opencode_sqlite"` and a `dbSpecPath` pointing at a minimal SQL file. The `basic_session` fixture covers the `session`, `message`, and `part` tables plus text, reasoning, tool-call chunk formatting, and token metadata.
