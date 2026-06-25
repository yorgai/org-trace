-- Patch: deploy the brick_event_chunks INSERT RLS policy that production is missing.
--
-- Symptom this fixes
-- -------------------
-- `brick sync push` uploads events fine but every chunk insert fails with:
--   42501  new row violates row-level security policy for table "brick_event_chunks"
--
-- Root cause
-- ----------
-- Production deployed an OLDER schema: the helper
-- `public.brick_can_insert_event_chunk(...)` and the matching INSERT policy on
-- `public.brick_event_chunks` are absent (verified live: PGRST202
-- "Could not find the function public.brick_can_insert_event_chunk"). The
-- canonical schema in docs/self-hosting/supabase.sql already contains them, but
-- the live database drifted behind it. This patch is the exact, idempotent
-- subset needed to bring an existing project back in sync WITHOUT re-running the
-- whole schema file. It is a verbatim slice of supabase.sql lines 148-177.
--
-- How to apply (no DB password needed)
-- ------------------------------------
-- Supabase Dashboard -> SQL Editor -> paste this file -> Run. The editor uses
-- your authenticated console session, so no service-role key or direct Postgres
-- connection string is required. Safe to run repeatedly (drop-if-exists +
-- create-or-replace).
--
-- After applying, re-run:
--   brick sync push --remote supabase --org-id <your-org>
-- and the chunk inserts will succeed.

-- 1) RLS must be on (no-op if already enabled).
alter table public.brick_event_chunks enable row level security;

-- 2) A chunk row is insertable only when the caller can already see the parent
--    event row for the same (event_id, repo_id, org_id) AND is a member of that
--    event's org. SECURITY DEFINER so the check can read brick_events under the
--    table owner while still gating on the caller's org membership.
create or replace function public.brick_can_insert_event_chunk(
  p_event_id uuid,
  p_repo_id text,
  p_org_id text
)
returns boolean
language sql
stable
security definer
set search_path = public
as $$
  select exists (
    select 1
    from public.brick_events e
    where e.event_id = p_event_id
      and e.repo_id = p_repo_id
      and e.org_id = p_org_id
      and public.brick_is_org_member(e.org_id)
  );
$$;

-- 3) The INSERT policy itself.
drop policy if exists "members can insert brick event chunks" on public.brick_event_chunks;
create policy "members can insert brick event chunks"
  on public.brick_event_chunks for insert
  with check (public.brick_can_insert_event_chunk(event_id, repo_id, org_id));
