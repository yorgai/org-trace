# Codex Example

Import Codex transcripts from explicit JSONL, text, or Markdown files. Brick records imported events with `confidence=imported` and appends them to the local JSONL queue.

```bash
cargo run -p brick -- source configure --name codex --app-id codex --actor-id codex-agent --actor-type agent --session-log-path ./exports/codex-transcript.md --notes "Codex transcript imports"
cargo run -p brick -- import codex --path ./exports/codex-transcript.md --mission <mission-id> --app-session-name "Codex refactor"
cargo run -p brick -- import codex --path ./exports/codex-events.jsonl --session <session-id> --mission <mission-id> --app-session-id <codex-session-id>
```

Use JSONL when you can export structured records; use text or Markdown when the available artifact is a transcript log.
