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

create index if not exists brick_events_repo_occurred_idx
  on public.brick_events (repo_id, occurred_at);

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
grant execute on function public.brick_create_org(text) to authenticated;
grant execute on function public.brick_invite_org_member(text, text) to authenticated;
grant execute on function public.brick_accept_invites() to authenticated;

