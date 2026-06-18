# Claude Code Example

Import Claude Code transcript files without depending on private application storage. Pass the transcript or JSONL export path explicitly.

```bash
cargo run -p brick -- source configure --name claude-code --app-id claude-code --actor-id claude-code-agent --actor-type agent --session-log-path ./exports/claude-code-transcript.txt --notes "Claude Code imports"
cargo run -p brick -- import claude-code --path ./exports/claude-code-transcript.txt --session <session-id> --mission <mission-id> --app-session-name "Claude Code task"
cargo run -p brick -- import claude-code --path ./exports/claude-code-events.jsonl --mission <mission-id> --app-session-id <claude-native-session>
```

Text and Markdown imports create a session start event and a `session.log_uploaded` metadata event that points at the supplied file path.
