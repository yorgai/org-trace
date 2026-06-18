# Codex App fixtures

Add sanitized Codex App scenarios as tiny JSONL transcripts under scenario-specific `logs/` directories. Keep prompts, paths, model names, patch contents, and command output synthetic.

The `patch_session` fixture covers the expected real-ish JSONL shapes for:

- `token_count` records and token metadata.
- User and assistant text records.
- Reasoning summaries.
- `apply_patch` tool calls and patch impact metadata.
