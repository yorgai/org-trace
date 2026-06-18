import { useMemo, useState } from 'react'
import './App.css'

type RequestState<T> = {
  loading: boolean
  data: T | null
  error: string | null
}

type HistorySource = {
  source_id: string
  app_id?: string | null
  selected: boolean
  session_db_path?: string | null
  session_log_path?: string | null
  evidence_root?: string | null
  cursor_state_db_path?: string | null
  notes?: string | null
}

type HistorySourcesResponse = {
  sources: HistorySource[]
}

type SourceDetectionResponse = {
  sources: Array<{
    source_id: string
    app_id: string
    label: string
  }>
  written: string[]
}

type SourceIndexRunResult = {
  written: string[]
  refreshed: Array<{
    source_id: string
    total: number
    returned: number
    plans: number
  }>
}

type HistorySession = {
  source_id: string
  app_id: string
  session_id: string
  external_session_id: string
  title?: string | null
  path: string
  size_bytes: number
  modified_at?: string | null
  created_at?: string | null
  updated_at?: string | null
  model?: string | null
  total_tokens?: number | null
  repo_path?: string | null
  branch?: string | null
  files_changed?: number | null
  touched_files: string[]
}

type HistorySessionsResponse = {
  source_id: string
  limit: number
  offset: number
  total: number
  has_more: boolean
  sessions: HistorySession[]
}

type HistoryRefreshResponse = {
  source_id: string
  refreshed_at: string
  sessions: HistorySessionsResponse
  plans: {
    total?: number
    plans?: unknown[]
  }
}

type ExportFormat = 'json' | 'csv'
type ExportSchema = 'audit-v1' | 'source-metadata-v1'

const DEFAULT_LIMIT = 50

function App() {
  const [serverUrl, setServerUrl] = useState('')
  const [selectedSource, setSelectedSource] = useState('')
  const [selectedSessionId, setSelectedSessionId] = useState('')
  const [offset, setOffset] = useState(0)
  const [exportFormat, setExportFormat] = useState<ExportFormat>('json')
  const [exportSchema, setExportSchema] = useState<ExportSchema>('audit-v1')
  const [sourcesState, setSourcesState] = useState<RequestState<HistorySourcesResponse>>({ loading: false, data: null, error: null })
  const [indexState, setIndexState] = useState<RequestState<SourceIndexRunResult>>({ loading: false, data: null, error: null })
  const [sessionsState, setSessionsState] = useState<RequestState<HistorySessionsResponse>>({ loading: false, data: null, error: null })
  const [exportState, setExportState] = useState<RequestState<string>>({ loading: false, data: null, error: null })
  const [lastExportUrl, setLastExportUrl] = useState<string | null>(null)

  const baseUrl = normalizeBaseUrl(serverUrl)
  const sources = sourcesState.data?.sources ?? []
  const selectedSourceRow = sources.find((source) => source.source_id === selectedSource) ?? null
  const currentSessions = sessionsState.data?.sessions ?? []
  const selectedSession = currentSessions.find((session) => session.session_id === selectedSessionId) ?? null
  const indexedTotals = useMemo(() => new Map(indexState.data?.refreshed.map((row) => [row.source_id, row]) ?? []), [indexState.data])

  async function loadSources() {
    setSourcesState({ loading: true, data: null, error: null })
    try {
      const data = await fetchJson<HistorySourcesResponse>(historyUrl(baseUrl, '/sources'))
      setSourcesState({ loading: false, data, error: null })
      const nextSource = data.sources.find((source) => source.source_id === selectedSource) ?? data.sources.find((source) => source.selected) ?? data.sources[0]
      if (nextSource) {
        setSelectedSource(nextSource.source_id)
        await loadSessionsForSource(nextSource.source_id, 0)
      }
    } catch (error) {
      setSourcesState({ loading: false, data: null, error: errorMessage(error) })
    }
  }

  async function indexAllSources() {
    setIndexState({ loading: true, data: null, error: null })
    setExportState({ loading: false, data: null, error: null })
    try {
      const detected = await fetchJson<SourceDetectionResponse>(historyUrl(baseUrl, '/source-detection'))
      const sourceIds = detected.sources.map((source) => source.source_id)
      if (sourceIds.length === 0) {
        setIndexState({ loading: false, data: { written: [], refreshed: [] }, error: null })
        return
      }

      const configured = await fetchJson<SourceDetectionResponse>(historyUrl(baseUrl, '/source-detection'), {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sources: sourceIds }),
      })

      const refreshed = []
      for (const sourceId of configured.written) {
        const refresh = await refreshSource(sourceId)
        refreshed.push({
          source_id: sourceId,
          total: refresh.sessions.total,
          returned: refresh.sessions.sessions.length,
          plans: refresh.plans.total ?? refresh.plans.plans?.length ?? 0,
        })
      }

      const result = { written: configured.written, refreshed }
      setIndexState({ loading: false, data: result, error: null })
      await loadSources()
      const focusSource = configured.written[0] ?? sourceIds[0]
      if (focusSource) {
        setSelectedSource(focusSource)
        await loadSessionsForSource(focusSource, 0)
      }
    } catch (error) {
      setIndexState({ loading: false, data: null, error: errorMessage(error) })
    }
  }

  async function refreshSelectedSource() {
    if (!selectedSource) return
    setIndexState({ loading: true, data: null, error: null })
    try {
      const refresh = await refreshSource(selectedSource)
      setSessionsState({ loading: false, data: refresh.sessions, error: null })
      setSelectedSessionId(refresh.sessions.sessions[0]?.session_id ?? '')
      setOffset(0)
      setIndexState({
        loading: false,
        data: {
          written: [selectedSource],
          refreshed: [
            {
              source_id: selectedSource,
              total: refresh.sessions.total,
              returned: refresh.sessions.sessions.length,
              plans: refresh.plans.total ?? refresh.plans.plans?.length ?? 0,
            },
          ],
        },
        error: null,
      })
    } catch (error) {
      setIndexState({ loading: false, data: null, error: errorMessage(error) })
    }
  }

  async function refreshSource(sourceId: string) {
    return fetchJson<HistoryRefreshResponse>(historyUrl(baseUrl, `/sources/${encodeURIComponent(sourceId)}/refresh?limit=${DEFAULT_LIMIT}`), {
      method: 'POST',
    })
  }

  async function loadSessionsForSource(sourceId: string, nextOffset = offset) {
    const safeOffset = Math.max(0, nextOffset)
    setOffset(safeOffset)
    setSessionsState({ loading: true, data: null, error: null })
    try {
      const params = new URLSearchParams({ limit: String(DEFAULT_LIMIT), offset: String(safeOffset) })
      const data = await fetchJson<HistorySessionsResponse>(historyUrl(baseUrl, `/sources/${encodeURIComponent(sourceId)}/sessions?${params.toString()}`))
      setSessionsState({ loading: false, data, error: null })
      setSelectedSessionId(data.sessions[0]?.session_id ?? '')
    } catch (error) {
      setSessionsState({ loading: false, data: null, error: errorMessage(error) })
    }
  }

  async function exportSession() {
    if (!selectedSource || !selectedSessionId) return
    setExportState({ loading: true, data: null, error: null })
    if (lastExportUrl) URL.revokeObjectURL(lastExportUrl)
    setLastExportUrl(null)
    try {
      const params = new URLSearchParams({ schema: exportSchema, format: exportFormat })
      const response = await fetch(historyUrl(baseUrl, `/sources/${encodeURIComponent(selectedSource)}/sessions/${encodeURIComponent(selectedSessionId)}/export?${params.toString()}`))
      const text = await response.text()
      if (!response.ok) throw new Error(text)
      const blob = new Blob([text], { type: exportFormat === 'csv' ? 'text/csv' : 'application/json' })
      const url = URL.createObjectURL(blob)
      setLastExportUrl(url)
      setExportState({ loading: false, data: exportFormat === 'json' ? prettyJson(text) : text, error: null })
    } catch (error) {
      setExportState({ loading: false, data: null, error: errorMessage(error) })
    }
  }

  return (
    <main className="shell">
      <section className="panel top-panel">
        <div className="panel-heading horizontal">
          <div>
            <p className="eyebrow">Brick local history</p>
            <h1>Source index and export</h1>
            <p>Detect local agent stores, index their session metadata, browse sessions, then export one session as JSON or CSV.</p>
          </div>
          <div className="primary-actions">
            <button className="secondary-button" type="button" onClick={() => void loadSources()}>
              {sourcesState.loading ? 'Loading…' : 'Load status'}
            </button>
            <button className="primary-button" type="button" disabled={indexState.loading} onClick={() => void indexAllSources()}>
              {indexState.loading ? 'Indexing…' : 'Detect + index'}
            </button>
          </div>
        </div>
        <label className="server-field">
          Server URL
          <input placeholder="Leave blank to use /api proxy" value={serverUrl} onChange={(event) => setServerUrl(event.target.value)} />
        </label>
        {sourcesState.error ? <div className="error-box">{sourcesState.error}</div> : null}
        {indexState.error ? <div className="error-box">{indexState.error}</div> : null}
        {indexState.data ? <IndexSummary result={indexState.data} /> : null}
      </section>

      <section className="grid main-grid">
        <section className="panel source-panel">
          <div className="panel-heading horizontal compact">
            <div>
              <p className="eyebrow">Sources</p>
              <h2>Index status</h2>
            </div>
            <button className="secondary-button" type="button" disabled={!selectedSource || indexState.loading} onClick={() => void refreshSelectedSource()}>
              {indexState.loading ? 'Refreshing…' : 'Refresh selected'}
            </button>
          </div>
          {sources.length === 0 && !sourcesState.loading ? <div className="empty-state">No indexed sources loaded yet. Click Detect + index.</div> : null}
          <div className="source-list">
            {sources.map((source) => {
              const total = indexedTotals.get(source.source_id)?.total
              return (
                <button
                  className={source.source_id === selectedSource ? 'source-card active' : 'source-card'}
                  key={source.source_id}
                  onClick={() => {
                    setSelectedSource(source.source_id)
                    void loadSessionsForSource(source.source_id, 0)
                  }}
                  type="button"
                >
                  <span>{source.selected ? 'Selected' : source.app_id ?? 'Source'}</span>
                  <strong>{source.source_id}</strong>
                  <p>{sourcePath(source)}</p>
                  <small>{total === undefined ? 'Indexed profile' : `${total} sessions indexed`}</small>
                </button>
              )
            })}
          </div>
        </section>

        <section className="panel sessions-panel">
          <div className="panel-heading horizontal compact">
            <div>
              <p className="eyebrow">Sessions</p>
              <h2>{selectedSource || 'Choose a source'}</h2>
              <p>{sessionsState.data ? `${sessionsState.data.offset + 1}-${sessionsState.data.offset + currentSessions.length} of ${sessionsState.data.total}` : 'Select a source to load sessions.'}</p>
            </div>
            <div className="pagination-buttons">
              <button className="secondary-button" type="button" disabled={!sessionsState.data || offset === 0} onClick={() => selectedSource && void loadSessionsForSource(selectedSource, Math.max(0, offset - DEFAULT_LIMIT))}>
                Previous
              </button>
              <button className="secondary-button" type="button" disabled={!sessionsState.data?.has_more} onClick={() => selectedSource && void loadSessionsForSource(selectedSource, offset + DEFAULT_LIMIT)}>
                Next
              </button>
            </div>
          </div>
          {sessionsState.error ? <div className="error-box">{sessionsState.error}</div> : null}
          <div className="session-list">
            {currentSessions.map((session) => (
              <button
                className={session.session_id === selectedSessionId ? 'session-card active' : 'session-card'}
                key={session.session_id}
                type="button"
                onClick={() => setSelectedSessionId(session.session_id)}
              >
                <strong>{session.title || session.session_id}</strong>
                <span>{session.updated_at ?? session.modified_at ?? 'No timestamp'}</span>
                <p>{session.path}</p>
              </button>
            ))}
          </div>
        </section>

        <section className="panel export-panel">
          <div className="panel-heading compact">
            <p className="eyebrow">Export</p>
            <h2>Selected session</h2>
            <p>{selectedSession ? selectedSession.session_id : 'Choose a session to export.'}</p>
          </div>
          {selectedSourceRow ? <SourceLine source={selectedSourceRow} /> : null}
          {selectedSession ? <SessionDetails session={selectedSession} /> : <div className="empty-state">No session selected.</div>}
          <div className="export-controls">
            <label>
              Schema
              <select value={exportSchema} onChange={(event) => setExportSchema(event.target.value as ExportSchema)}>
                <option value="audit-v1">audit-v1</option>
                <option value="source-metadata-v1">source-metadata-v1</option>
              </select>
            </label>
            <label>
              Format
              <select value={exportFormat} onChange={(event) => setExportFormat(event.target.value as ExportFormat)}>
                <option value="json">JSON</option>
                <option value="csv">CSV</option>
              </select>
            </label>
            <button className="primary-button" type="button" disabled={!selectedSession || exportState.loading} onClick={() => void exportSession()}>
              {exportState.loading ? 'Exporting…' : 'Export'}
            </button>
          </div>
          {lastExportUrl ? (
            <a className="download-link" download={`brick-history-${selectedSource}-${selectedSessionId}.${exportFormat}`} href={lastExportUrl}>
              Download {exportFormat.toUpperCase()}
            </a>
          ) : null}
          {exportState.error ? <div className="error-box">{exportState.error}</div> : null}
          {exportState.data ? <pre className="export-output">{exportState.data}</pre> : null}
        </section>
      </section>
    </main>
  )
}

function IndexSummary({ result }: { result: SourceIndexRunResult }) {
  if (result.written.length === 0) {
    return <div className="empty-state">No local sources were detected.</div>
  }

  return (
    <div className="status-grid">
      {result.refreshed.map((source) => (
        <div className="status-card" key={source.source_id}>
          <span>{source.source_id}</span>
          <strong>{source.total} sessions</strong>
          <small>{source.plans} plans</small>
        </div>
      ))}
    </div>
  )
}

function SourceLine({ source }: { source: HistorySource }) {
  return (
    <div className="source-line">
      <strong>{source.source_id}</strong>
      <span>{sourcePath(source)}</span>
    </div>
  )
}

function SessionDetails({ session }: { session: HistorySession }) {
  return (
    <dl className="detail-list">
      <div>
        <dt>Model</dt>
        <dd>{session.model ?? '—'}</dd>
      </div>
      <div>
        <dt>Tokens</dt>
        <dd>{session.total_tokens ?? '—'}</dd>
      </div>
      <div>
        <dt>Repo</dt>
        <dd>{session.repo_path ?? '—'}</dd>
      </div>
      <div>
        <dt>Branch</dt>
        <dd>{session.branch ?? '—'}</dd>
      </div>
      <div>
        <dt>Touched files</dt>
        <dd>{formatTouchedFiles(session)}</dd>
      </div>
      <div>
        <dt>Size</dt>
        <dd>{formatBytes(session.size_bytes)}</dd>
      </div>
    </dl>
  )
}

function formatTouchedFiles(session: HistorySession) {
  if (session.touched_files.length > 0) {
    return session.touched_files.length.toString()
  }
  if (typeof session.files_changed === 'number') {
    return `${session.files_changed} changed`
  }
  return '—'
}

function normalizeBaseUrl(value: string) {
  return value.trim().replace(/\/$/, '')
}

function apiPrefix(baseUrl: string) {
  return baseUrl || '/api'
}

function historyUrl(baseUrl: string, path: string) {
  return `${apiPrefix(baseUrl)}/v1/local-history${path}`
}

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, init)
  const body = await parseResponse(response)

  if (!response.ok) {
    throw new Error(typeof body === 'string' ? body : JSON.stringify(body, null, 2))
  }

  return body as T
}

async function parseResponse(response: Response) {
  const contentType = response.headers.get('content-type') ?? ''
  if (contentType.includes('application/json')) {
    return response.json()
  }

  return response.text()
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : 'Unknown request failure'
}

function prettyJson(text: string) {
  try {
    return JSON.stringify(JSON.parse(text), null, 2)
  } catch {
    return text
  }
}

function sourcePath(source: HistorySource) {
  return source.notes ?? source.session_log_path ?? source.session_db_path ?? source.cursor_state_db_path ?? source.evidence_root ?? 'Indexed source profile'
}

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '0 B'
  if (value < 1024) return `${value} B`
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${(value / (1024 * 1024)).toFixed(1)} MB`
}

export default App
