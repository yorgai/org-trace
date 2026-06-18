# Cursor Example

Record Cursor exports by passing explicit files to Brick. The MVP importer does not open or infer private Cursor workspace databases; source profile paths are only hints for humans and scripts.

```bash
cargo run -p brick -- source configure --name cursor --app-id cursor --actor-id cursor-agent --actor-type agent --session-log-path ./exports/cursor-session.jsonl --notes "Cursor import defaults"
cargo run -p brick -- source use --name cursor
cargo run -p brick -- --source cursor import cursor --path ./exports/cursor-session.jsonl --mission <mission-id> --session <session-id> --app-session-id <cursor-native-session> --app-session-name "Cursor: MVP"
cargo run -p brick -- --source cursor import cursor --path ./exports/cursor-transcript.md --mission <mission-id> --app-session-name "Cursor transcript"
```

JSONL lines may be full Brick `TraceEvent` records or simple records such as:

```json
{"title":"User request","message":"Implement importer MVP","role":"user"}
```

For a complete temporary end-to-end run that includes Cursor import plus server push/pull, see `../../scripts/smoke_mvp.sh`.
