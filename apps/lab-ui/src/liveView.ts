// Pure helpers for the Live & Announcements panel, kept free of React/DOM so
// they can be unit-tested under `node --test` like exportFilename.ts.

export type LiveSession = {
  source_id: string
  app_id: string
  external_session_id: string
  title?: string | null
  path: string
  work_scope?: string | null
  repo_path?: string | null
  branch?: string | null
  last_activity?: string | null
  touched_files: string[]
}

export type LiveResponse = {
  source_id: string
  window_secs: number
  count: number
  sessions: LiveSession[]
}

export type Announcement = {
  id: string
  source_id: string
  session_id: string
  scope: string
  message: string
  work_dir?: string | null
  created_at: string
  expires_at: string
}

export type AnnouncementsResponse = {
  count: number
  announcements: Announcement[]
}

/** Short, human-friendly label for a live session row's work scope. */
export function liveScopeLabel(session: LiveSession): string {
  const scope = session.work_scope ?? session.repo_path ?? null
  if (!scope) return 'no shared scope'
  const segments = scope.split('/').filter(Boolean)
  return segments.length === 0 ? scope : segments[segments.length - 1]
}

/** Relative "x ago" string from an ISO timestamp to `now` (ms). */
export function relativeAge(iso: string | null | undefined, now: number = Date.now()): string {
  if (!iso) return 'unknown'
  const then = Date.parse(iso)
  if (Number.isNaN(then)) return 'unknown'
  const seconds = Math.max(0, Math.round((now - then) / 1000))
  if (seconds < 60) return `${seconds}s ago`
  const minutes = Math.round(seconds / 60)
  if (minutes < 60) return `${minutes}m ago`
  const hours = Math.round(minutes / 60)
  if (hours < 24) return `${hours}h ago`
  return `${Math.round(hours / 24)}d ago`
}

/** Whether an announcement is still active (not past its expiry) at `now`. */
export function announcementActive(announcement: Announcement, now: number = Date.now()): boolean {
  const expires = Date.parse(announcement.expires_at)
  return Number.isNaN(expires) ? true : expires > now
}

/** Filters announcements to the active ones, newest-created first. */
export function activeAnnouncements(
  announcements: Announcement[],
  now: number = Date.now(),
): Announcement[] {
  return announcements
    .filter((announcement) => announcementActive(announcement, now))
    .sort((left, right) => Date.parse(right.created_at) - Date.parse(left.created_at))
}

/** Builds the local-history bridge URL for a sub-path under a base prefix. */
export function liveHistoryUrl(apiPrefix: string, path: string): string {
  return `${apiPrefix}/v1/local-history${path}`
}
