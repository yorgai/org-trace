-- Supabase-native Brick sharing schema.
-- Run this once in the Supabase SQL editor for your project.

create table if not exists public.brick_orgs (
  org_id text primary key,
  created_by uuid not null references auth.users(id) on delete cascade,
  created_at timestamptz not null default now()
);

create table if not exists public.brick_org_members (
  org_id text not null references public.brick_orgs(org_id) on delete cascade,
  user_id uuid not null references auth.users(id) on delete cascade,
  role text not null default 'member' check (role in ('owner', 'member')),
  created_at timestamptz not null default now(),
  primary key (org_id, user_id)
);

create table if not exists public.brick_org_invites (
  org_id text not null references public.brick_orgs(org_id) on delete cascade,
  email text not null,
  role text not null default 'member' check (role in ('owner', 'member')),
  invited_by uuid not null references auth.users(id) on delete cascade,
  created_at timestamptz not null default now(),
  accepted_at timestamptz,
  primary key (org_id, email)
);

alter table public.brick_orgs enable row level security;
alter table public.brick_org_members enable row level security;
alter table public.brick_org_invites enable row level security;

create table if not exists public.brick_events (
  event_id uuid primary key,
  repo_id text not null,
  org_id text not null references public.brick_orgs(org_id) on delete cascade,
  occurred_at timestamptz not null,
  event jsonb not null,
  inserted_by uuid not null references auth.users(id) on delete cascade default auth.uid(),
  inserted_at timestamptz not null default now()
);

alter table public.brick_events enable row level security;

create table if not exists public.brick_event_chunks (
  event_id uuid not null references public.brick_events(event_id) on delete cascade,
  repo_id text not null,
  org_id text not null references public.brick_orgs(org_id) on delete cascade,
  source_id text not null,
  external_session_id text not null,
  chunk_index integer not null,
  chunk_kind text,
  role text,
  actor_id text,
  occurred_at timestamptz,
  text text,
  raw jsonb not null,
  inserted_at timestamptz not null default now(),
  primary key (event_id, chunk_index)
);

alter table public.brick_event_chunks enable row level security;

create index if not exists brick_events_repo_occurred_idx
  on public.brick_events (repo_id, occurred_at);

create index if not exists brick_event_chunks_repo_session_idx
  on public.brick_event_chunks (repo_id, source_id, external_session_id, chunk_index);

create index if not exists brick_event_chunks_repo_occurred_idx
  on public.brick_event_chunks (repo_id, occurred_at);

create index if not exists brick_event_chunks_text_fts_idx
  on public.brick_event_chunks
  using gin (to_tsvector('english', coalesce(text, '')));

create or replace function public.brick_is_org_member(p_org_id text)
returns boolean
language sql
stable
security definer
set search_path = public
as $$
  select exists (
    select 1
    from public.brick_org_members m
    where m.org_id = p_org_id and m.user_id = auth.uid()
  );
$$;

create or replace function public.brick_is_org_owner(p_org_id text)
returns boolean
language sql
stable
security definer
set search_path = public
as $$
  select exists (
    select 1
    from public.brick_org_members m
    where m.org_id = p_org_id and m.user_id = auth.uid() and m.role = 'owner'
  );
$$;

drop policy if exists "members can read orgs" on public.brick_orgs;
create policy "members can read orgs"
  on public.brick_orgs for select
  using (public.brick_is_org_member(org_id));

drop policy if exists "owners can manage orgs" on public.brick_orgs;
create policy "owners can manage orgs"
  on public.brick_orgs for all
  using (public.brick_is_org_owner(org_id))
  with check (created_by = auth.uid());

drop policy if exists "members can read memberships" on public.brick_org_members;
create policy "members can read memberships"
  on public.brick_org_members for select
  using (public.brick_is_org_member(org_id));

drop policy if exists "owners can manage memberships" on public.brick_org_members;
create policy "owners can manage memberships"
  on public.brick_org_members for all
  using (public.brick_is_org_owner(org_id));

drop policy if exists "owners can read invites" on public.brick_org_invites;
create policy "owners can read invites"
  on public.brick_org_invites for select
  using (public.brick_is_org_owner(org_id));

drop policy if exists "owners can manage invites" on public.brick_org_invites;
create policy "owners can manage invites"
  on public.brick_org_invites for all
  using (public.brick_is_org_owner(org_id));

drop policy if exists "members can read brick events" on public.brick_events;
create policy "members can read brick events"
  on public.brick_events for select
  using (public.brick_is_org_member(org_id));

drop policy if exists "members can insert brick events" on public.brick_events;
create policy "members can insert brick events"
  on public.brick_events for insert
  with check (
    inserted_by = auth.uid()
    and public.brick_is_org_member(org_id)
  );

drop policy if exists "members can read brick event chunks" on public.brick_event_chunks;
create policy "members can read brick event chunks"
  on public.brick_event_chunks for select
  using (public.brick_is_org_member(org_id));

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

drop policy if exists "members can insert brick event chunks" on public.brick_event_chunks;
create policy "members can insert brick event chunks"
  on public.brick_event_chunks for insert
  with check (public.brick_can_insert_event_chunk(event_id, repo_id, org_id));

create or replace function public.brick_backfill_event_chunks(
  p_event_id uuid,
  p_offset integer default 0,
  p_limit integer default 500
)
returns bigint
language plpgsql
security definer
set search_path = public
as $$
declare
  indexed_count bigint;
begin
  insert into public.brick_event_chunks (
    event_id,
    repo_id,
    org_id,
    source_id,
    external_session_id,
    chunk_index,
    chunk_kind,
    role,
    actor_id,
    occurred_at,
    text,
    raw
  )
  select
    e.event_id,
    e.repo_id,
    e.org_id,
    e.event->'payload'->>'source_id',
    e.event->'payload'->>'external_session_id',
    chunk.ordinality - 1,
    coalesce(chunk.value->>'action_type', chunk.value->>'kind', chunk.value->>'type', chunk.value->>'function'),
    coalesce(chunk.value->>'role', chunk.value->'result'->>'role', chunk.value->'result'->'message'->>'role'),
    coalesce(chunk.value->>'actor_id', chunk.value->'actor'->>'actor_id'),
    case
      when coalesce(chunk.value->>'created_at', chunk.value->>'occurred_at', chunk.value->>'timestamp') ~ '^\d{4}-\d{2}-\d{2}T'
        then coalesce(chunk.value->>'created_at', chunk.value->>'occurred_at', chunk.value->>'timestamp')::timestamptz
      else null
    end,
    coalesce(
      chunk.value->>'text',
      chunk.value->>'content',
      chunk.value->'message'->>'content',
      chunk.value->'result'->>'content',
      chunk.value->'result'->'message'->>'content',
      chunk.value->'result'->>'observation',
      chunk.value->'result'->>'output'
    ),
    chunk.value
  from public.brick_events e
  cross join lateral jsonb_array_elements(coalesce(e.event->'payload'->'normalized_chunks', '[]'::jsonb))
    with ordinality as chunk(value, ordinality)
  where e.event_id = p_event_id
    and e.event->>'event_type' = 'source.session_observed'
    and chunk.ordinality > greatest(p_offset, 0)
    and chunk.ordinality <= greatest(p_offset, 0) + greatest(p_limit, 1)
  on conflict (event_id, chunk_index) do nothing;

  get diagnostics indexed_count = row_count;
  return indexed_count;
end;
$$;

create or replace function public.brick_backfill_next_event_chunk_batch(p_limit integer default 500)
returns table(event_id uuid, chunk_count bigint)
language plpgsql
security definer
set search_path = public
as $$
begin
  return query
  with pending as (
    select
      e.event_id,
      coalesce(max(c.chunk_index) + 1, 0) as chunk_offset
    from public.brick_events e
    left join public.brick_event_chunks c on c.event_id = e.event_id
    where e.event->>'event_type' = 'source.session_observed'
    group by e.event_id, e.event, e.occurred_at
    having jsonb_array_length(coalesce(e.event->'payload'->'normalized_chunks', '[]'::jsonb)) > coalesce(max(c.chunk_index) + 1, 0)
    order by e.occurred_at, e.event_id
    limit 1
  )
  select pending.event_id, public.brick_backfill_event_chunks(pending.event_id, pending.chunk_offset::integer, p_limit)
  from pending;
end;
$$;

create or replace function public.brick_create_org(p_org_id text)
returns void
language plpgsql
security definer
set search_path = public
as $$
begin
  if auth.uid() is null then
    raise exception 'not authenticated' using errcode = '42501';
  end if;

  insert into public.brick_orgs (org_id, created_by)
  values (p_org_id, auth.uid())
  on conflict (org_id) do nothing;

  insert into public.brick_org_members (org_id, user_id, role)
  values (p_org_id, auth.uid(), 'owner')
  on conflict (org_id, user_id) do update set role = 'owner';
end;
$$;

create or replace function public.brick_invite_org_member(p_org_id text, p_email text)
returns void
language plpgsql
security definer
set search_path = public
as $$
begin
  if not public.brick_is_org_owner(p_org_id) then
    raise exception 'not an owner of org %', p_org_id using errcode = '42501';
  end if;

  insert into public.brick_org_invites (org_id, email, role, invited_by)
  values (p_org_id, lower(p_email), 'member', auth.uid())
  on conflict (org_id, email) do update
    set role = excluded.role,
        invited_by = excluded.invited_by,
        created_at = now(),
        accepted_at = null;
end;
$$;

create or replace function public.brick_accept_invites()
returns void
language plpgsql
security definer
set search_path = public
as $$
declare
  user_email text;
begin
  if auth.uid() is null then
    raise exception 'not authenticated' using errcode = '42501';
  end if;

  select lower(email) into user_email from auth.users where id = auth.uid();

  insert into public.brick_org_members (org_id, user_id, role)
  select org_id, auth.uid(), role
  from public.brick_org_invites
  where email = user_email and accepted_at is null
  on conflict (org_id, user_id) do update set role = excluded.role;

  update public.brick_org_invites
  set accepted_at = now()
  where email = user_email and accepted_at is null;
end;
$$;

grant execute on function public.brick_is_org_member(text) to authenticated;
grant execute on function public.brick_is_org_owner(text) to authenticated;
grant execute on function public.brick_can_insert_event_chunk(uuid, text, text) to authenticated;
grant execute on function public.brick_backfill_event_chunks(uuid, integer, integer) to authenticated;
grant execute on function public.brick_backfill_next_event_chunk_batch(integer) to authenticated;
grant execute on function public.brick_create_org(text) to authenticated;
grant execute on function public.brick_invite_org_member(text, text) to authenticated;
grant execute on function public.brick_accept_invites() to authenticated;

