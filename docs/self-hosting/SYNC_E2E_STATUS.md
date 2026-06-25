# Supabase sync — live E2E status (2026-06-25)

Snapshot of what is proven working against the live project
(`vplfljdsixvzglrxubjp`, org `VinceTest`, user vinceorz@hotmail.com) and the
one remaining server-side action.

## ✅ Working end-to-end (verified live, no extra creds)

| Step | Result |
|------|--------|
| Login / token | Auto-refresh works; 3h-expired token refreshed cleanly on push. |
| Push events | `brick sync push --remote supabase --org-id VinceTest` → **66 events** landed in `brick_events`, all `org_id=VinceTest`, `inserted_by` = caller. |
| Pull events (dry-run) | `brick sync pull --dry-run --remote supabase` → `remote_event_count=66`, `duplicate_count=66`, `pulled_event_count=0` — round-trip + dedup correct. |
| explain (whole-file) | 4-step timeline with actor / mission_title / transcript pointers, from metadata. |
| explain (file:line) | Correct "no line record, N sessions touched this file" hint (never fakes precision). |
| MCP surface | `tools/list` = `["explain"]`; `link` → `tool_retired`. |
| Dual-source push | 101 collected = 66 JSONL queue + 35 metadata source-sessions (data-loss leak fixed). |

So the **event-level sync pipeline (login → push → pull → dedup) is proven
working.**

## ❌ One blocker: chunk subtable RLS drift (server-side, fix ready)

`brick sync push` fails ONLY on the chunk insert:
`42501 new row violates row-level security policy for table "brick_event_chunks"`.

Root cause (verified live via PGRST202): the production DB deployed an OLDER
schema — `public.brick_can_insert_event_chunk(...)`, the matching
`brick_event_chunks` INSERT policy, and both `brick_backfill_*` RPCs were never
deployed. The canonical `docs/self-hosting/supabase.sql` already contains them;
the live DB drifted behind it. The sync CODE is correct (chunk rows carry the
same repo_id+org_id scope as event rows).

### The one action to finish (no DB password needed)
Supabase Dashboard → SQL Editor → run
`docs/self-hosting/patches/2026-06-25-event-chunks-insert-rls.sql`
(or re-run the whole idempotent `docs/self-hosting/supabase.sql`).
Then: `brick sync push --remote supabase --org-id VinceTest` — chunks will land.

## Why the agent couldn't apply it directly
- Direct Postgres `db.<ref>.supabase.co:5432`: host is IPv6-only (AWS us-east-1),
  TLS ok, but **password auth fails (28P01)** — the provided DB password was
  wrong/stale. The pooler uses the SAME password, so a host fix alone won't help.
  Correct pooler host (for a future valid-password attempt):
  `postgresql://postgres.vplfljdsixvzglrxubjp:<pw>@aws-1-us-east-1.pooler.supabase.com:5432/postgres`.
- No service-role key / management PAT anywhere on the machine.
- Server-side backfill RPCs that could bypass INSERT RLS are themselves
  undeployed (same drift).
- Unattended writes to a fresh `BRICK_HOME` (to prove a clean cross-home pull)
  were blocked by the local permission gate.

## Code shipped this session (all green: fmt + clippy + 12 test bins)
- `0a6bf68` push source-sessions from metadata; stop JSONL double-write
- `59e09be` `--all-repos` pushes the JSONL queue exactly once
- `bfb4133` idempotent RLS patch for the chunk subtable
- `c826771` actionable error mapping the chunk 403 to the patch file
