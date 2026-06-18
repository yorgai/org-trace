CREATE TABLE session (
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

CREATE TABLE message (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    role TEXT,
    data TEXT
);

CREATE TABLE part (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    message_id TEXT,
    type TEXT,
    data TEXT,
    time_created INTEGER
);

INSERT INTO session VALUES (
    'opencode-session-1',
    'Build OpenCode fixture',
    '/workspace/opencode-repo',
    '{"id":"anthropic/claude-example"}',
    10,
    3,
    2,
    5,
    2,
    1766200000000,
    1766200060000,
    NULL
);

INSERT INTO message VALUES (
    'opencode-message-user',
    'opencode-session-1',
    'user',
    '{"role":"user"}'
);

INSERT INTO message VALUES (
    'opencode-message-assistant',
    'opencode-session-1',
    'assistant',
    '{"role":"assistant"}'
);

INSERT INTO part VALUES (
    'opencode-part-user',
    'opencode-session-1',
    'opencode-message-user',
    'text',
    '{"text":"Run OpenCode fixture"}',
    1766200001000
);

INSERT INTO part VALUES (
    'opencode-part-assistant',
    'opencode-session-1',
    'opencode-message-assistant',
    'text',
    '{"text":"I will run the command."}',
    1766200002000
);

INSERT INTO part VALUES (
    'opencode-part-reasoning',
    'opencode-session-1',
    'opencode-message-assistant',
    'reasoning',
    '{"text":"Need a shell command."}',
    1766200003000
);

INSERT INTO part VALUES (
    'opencode-part-tool',
    'opencode-session-1',
    'opencode-message-assistant',
    'tool',
    '{"name":"bash","arguments":{"command":"cargo test -p brick-core external_source_provider_fixtures_match_expected_metadata_and_chunks"},"output":"ok"}',
    1766200004000
);
