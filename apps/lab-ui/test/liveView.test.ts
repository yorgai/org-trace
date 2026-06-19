/// <reference types="node" />

import assert from 'node:assert/strict'
import test from 'node:test'
import {
  activeAnnouncements,
  announcementActive,
  liveHistoryUrl,
  liveScopeLabel,
  relativeAge,
  type Announcement,
  type LiveSession,
} from '../src/liveView.ts'

function session(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    source_id: 'orgii',
    app_id: 'orgii',
    external_session_id: 'sess-1',
    title: 'Refactor auth',
    path: '/Users/me/.orgii/sessions.db',
    work_scope: '/Users/me/Projects/Brick-Vault',
    repo_path: '/Users/me/Projects/Brick-Vault',
    branch: null,
    last_activity: null,
    touched_files: [],
    ...overrides,
  }
}

function claim(overrides: Partial<Announcement> = {}): Announcement {
  return {
    id: 'ann-1',
    source_id: 'claude_code',
    session_id: 'agentA',
    scope: 'crates/core/src/auth.rs',
    message: 'hands off',
    work_dir: '/repo',
    created_at: '2026-06-19T20:00:00Z',
    expires_at: '2026-06-19T21:00:00Z',
    ...overrides,
  }
}

test('liveScopeLabel uses the last path segment of the work scope', () => {
  assert.equal(liveScopeLabel(session()), 'Brick-Vault')
  assert.equal(liveScopeLabel(session({ work_scope: null, repo_path: '/a/b/proj' })), 'proj')
  assert.equal(liveScopeLabel(session({ work_scope: null, repo_path: null })), 'no shared scope')
})

test('relativeAge renders coarse buckets', () => {
  const now = Date.parse('2026-06-19T21:00:00Z')
  assert.equal(relativeAge('2026-06-19T20:59:30Z', now), '30s ago')
  assert.equal(relativeAge('2026-06-19T20:50:00Z', now), '10m ago')
  assert.equal(relativeAge('2026-06-19T18:00:00Z', now), '3h ago')
  assert.equal(relativeAge(null, now), 'unknown')
  assert.equal(relativeAge('not-a-date', now), 'unknown')
})

test('announcementActive respects expiry', () => {
  const now = Date.parse('2026-06-19T20:30:00Z')
  assert.equal(announcementActive(claim(), now), true)
  assert.equal(announcementActive(claim({ expires_at: '2026-06-19T20:00:00Z' }), now), false)
})

test('activeAnnouncements drops expired and sorts newest-first', () => {
  const now = Date.parse('2026-06-19T20:30:00Z')
  const older = claim({ id: 'a', created_at: '2026-06-19T20:00:00Z', expires_at: '2026-06-19T21:00:00Z' })
  const newer = claim({ id: 'b', created_at: '2026-06-19T20:20:00Z', expires_at: '2026-06-19T21:00:00Z' })
  const expired = claim({ id: 'c', created_at: '2026-06-19T20:25:00Z', expires_at: '2026-06-19T20:10:00Z' })
  const result = activeAnnouncements([older, expired, newer], now)
  assert.deepEqual(result.map((row) => row.id), ['b', 'a'])
})

test('liveHistoryUrl composes the bridge path', () => {
  assert.equal(liveHistoryUrl('/api', '/live?source=all'), '/api/v1/local-history/live?source=all')
  assert.equal(liveHistoryUrl('http://127.0.0.1:5353', '/announcements'), 'http://127.0.0.1:5353/v1/local-history/announcements')
})
