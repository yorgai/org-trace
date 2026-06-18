import { useMemo, useState } from 'react'
import type { FormEvent } from 'react'
import './App.css'

type RequestState<T> = {
  loading: boolean
  data: T | null
  error: string | null
}

type ServerIndexStatus = {
  repo_id: string | null
  event_count: number
  mission_count: number
  session_count: number
  artifact_count: number
  file_count: number
  session_log_count: number
  diff_count: number
  rebuilt_at: string
}

type ListEventsResponse = {
  events: unknown[]
  cursor?: string
  next_cursor?: string
}

type ServerSessionsResponse = {
  repo_id: string | null
  sessions: Array<Record<string, unknown>>
}

type EndpointResult = ServerIndexStatus | ListEventsResponse | ServerSessionsResponse | { ok: boolean }

type EndpointKey = 'health' | 'index' | 'sessions' | 'events'

type EndpointConfig = {
  key: EndpointKey
  label: string
  description: string
  method: 'GET'
}

const endpoints: EndpointConfig[] = [
  {
    key: 'health',
    label: 'Health',
    description: 'Checks whether brick-server is reachable.',
    method: 'GET',
  },
  {
    key: 'index',
    label: 'Index status',
    description: 'Rebuilds and summarizes the server-side projection.',
    method: 'GET',
  },
  {
    key: 'sessions',
    label: 'Sessions',
    description: 'Lists indexed sessions with optional app and actor filters.',
    method: 'GET',
  },
  {
    key: 'events',
    label: 'Events',
    description: 'Pages through stored sync events for the selected repo.',
    method: 'GET',
  },
]

const cliRecipes = [
  {
    title: 'Start the API server',
    command: 'cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server',
  },
  {
    title: 'Seed local source defaults',
    command: 'cargo run -p brick -- init && cargo run -p brick -- source scan --write-defaults',
  },
  {
    title: 'Push this repo to the lab server',
    command: 'cargo run -p brick -- sync push --remote http://127.0.0.1:7821 --repo-id repo-a',
  },
  {
    title: 'Inspect repo sessions directly',
    command: "curl 'http://127.0.0.1:7821/v1/repos/repo-a/sessions?limit=20'",
  },
]

function App() {
  const [serverUrl, setServerUrl] = useState('')
  const [repoId, setRepoId] = useState('repo-a')
  const [limit, setLimit] = useState(20)
  const [appId, setAppId] = useState('')
  const [actorId, setActorId] = useState('')
  const [result, setResult] = useState<RequestState<EndpointResult>>({
    loading: false,
    data: null,
    error: null,
  })
  const [lastEndpoint, setLastEndpoint] = useState<EndpointKey>('health')

  const baseUrl = normalizeBaseUrl(serverUrl)
  const usingProxy = baseUrl === ''

  const previewUrls = useMemo(
    () => endpoints.map((endpoint) => ({ endpoint, url: buildEndpointUrl(endpoint.key, baseUrl, repoId, limit, appId, actorId) })),
    [actorId, appId, baseUrl, limit, repoId],
  )

  async function runEndpoint(endpoint: EndpointKey) {
    setLastEndpoint(endpoint)
    setResult({ loading: true, data: null, error: null })

    try {
      const response = await fetch(buildEndpointUrl(endpoint, baseUrl, repoId, limit, appId, actorId))
      const body = await parseResponse(response)

      if (!response.ok) {
        throw new Error(typeof body === 'string' ? body : JSON.stringify(body, null, 2))
      }

      setResult({ loading: false, data: body as EndpointResult, error: null })
    } catch (error) {
      setResult({
        loading: false,
        data: null,
        error: error instanceof Error ? error.message : 'Unknown request failure',
      })
    }
  }

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    void runEndpoint(lastEndpoint)
  }

  return (
    <main className="shell">
      <section className="hero-panel">
        <div>
          <p className="eyebrow">Brick Local Lab</p>
          <p className="hero-copy">
            Run `brick-server` on localhost, then use this dashboard to test health, event listing,
            repo-scoped index status, and session queries while building Brick features.
          </p>
        </div>
        <div className="status-card">
          <span className={usingProxy ? 'status-dot proxy' : 'status-dot'} />
          <div>
            <strong>{usingProxy ? 'Using Vite proxy' : 'Direct server URL'}</strong>
            <p>{usingProxy ? '/api → http://127.0.0.1:7821' : baseUrl}</p>
          </div>
        </div>
      </section>

      <section className="grid two-column">
        <form className="panel controls" onSubmit={handleSubmit}>
          <div className="panel-heading">
            <p className="eyebrow">Connection</p>
            <h2>Request controls</h2>
          </div>

          <label>
            Server URL
            <input
              placeholder="Leave blank to use /api proxy"
              value={serverUrl}
              onChange={(event) => setServerUrl(event.target.value)}
            />
          </label>

          <div className="field-row">
            <label>
              Repo ID
              <input value={repoId} onChange={(event) => setRepoId(event.target.value)} />
            </label>
            <label>
              Limit
              <input
                min="1"
                max="1000"
                type="number"
                value={limit}
                onChange={(event) => setLimit(Number(event.target.value))}
              />
            </label>
          </div>

          <div className="field-row">
            <label>
              App ID filter
              <input placeholder="cursor, claude_code…" value={appId} onChange={(event) => setAppId(event.target.value)} />
            </label>
            <label>
              Actor ID filter
              <input placeholder="agent-1" value={actorId} onChange={(event) => setActorId(event.target.value)} />
            </label>
          </div>

          <button className="primary-button" type="submit">
            Re-run {endpointLabel(lastEndpoint)}
          </button>
        </form>

        <section className="panel quick-actions">
          <div className="panel-heading">
            <p className="eyebrow">Endpoints</p>
            <h2>Feature probes</h2>
          </div>
          <div className="endpoint-list">
            {previewUrls.map(({ endpoint, url }) => (
              <button key={endpoint.key} className="endpoint-card" type="button" onClick={() => void runEndpoint(endpoint.key)}>
                <span>{endpoint.method}</span>
                <strong>{endpoint.label}</strong>
                <p>{endpoint.description}</p>
                <code>{url}</code>
              </button>
            ))}
          </div>
        </section>
      </section>

      <section className="grid two-column lower-grid">
        <section className="panel result-panel">
          <div className="panel-heading horizontal">
            <div>
              <p className="eyebrow">Response</p>
              <h2>{endpointLabel(lastEndpoint)}</h2>
            </div>
            {result.loading ? <span className="pill">Loading</span> : null}
          </div>

          {result.error ? <div className="error-box">{result.error}</div> : null}
          {!result.error && !result.data && !result.loading ? (
            <div className="empty-state">Choose a feature probe to see formatted JSON here.</div>
          ) : null}
          {result.data ? <ResponseSummary data={result.data} /> : null}
          {result.data ? <pre className="json-output">{JSON.stringify(result.data, null, 2)}</pre> : null}
        </section>

        <section className="panel recipes">
          <div className="panel-heading">
            <p className="eyebrow">Terminal recipes</p>
            <h2>Seed data and compare outputs</h2>
          </div>
          <div className="recipe-list">
            {cliRecipes.map((recipe) => (
              <article key={recipe.title} className="recipe-card">
                <strong>{recipe.title}</strong>
                <code>{recipe.command}</code>
              </article>
            ))}
          </div>
        </section>
      </section>
    </main>
  )
}

function ResponseSummary({ data }: { data: EndpointResult }) {
  if ('event_count' in data) {
    return (
      <div className="metric-grid">
        <Metric label="Events" value={data.event_count} />
        <Metric label="Missions" value={data.mission_count} />
        <Metric label="Sessions" value={data.session_count} />
        <Metric label="Artifacts" value={data.artifact_count} />
      </div>
    )
  }

  if ('events' in data) {
    return (
      <div className="metric-grid">
        <Metric label="Events returned" value={data.events.length} />
        <Metric label="Next cursor" value={data.next_cursor ?? '—'} />
      </div>
    )
  }

  if ('sessions' in data) {
    return (
      <div className="metric-grid">
        <Metric label="Sessions returned" value={data.sessions.length} />
        <Metric label="Repo" value={data.repo_id ?? 'global'} />
      </div>
    )
  }

  return (
    <div className="metric-grid">
      <Metric label="OK" value={data.ok ? 'true' : 'false'} />
    </div>
  )
}

function Metric({ label, value }: { label: string; value: number | string }) {
  return (
    <div className="metric-card">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  )
}

function endpointLabel(endpoint: EndpointKey) {
  return endpoints.find((item) => item.key === endpoint)?.label ?? endpoint
}

function normalizeBaseUrl(value: string) {
  return value.trim().replace(/\/$/, '')
}

function buildEndpointUrl(endpoint: EndpointKey, baseUrl: string, repoId: string, limit: number, appId: string, actorId: string) {
  const prefix = baseUrl || '/api'
  const safeRepoId = encodeURIComponent(repoId.trim() || 'repo-a')
  const params = new URLSearchParams()

  switch (endpoint) {
    case 'health':
      return `${prefix}/health`
    case 'index':
      return `${prefix}/v1/repos/${safeRepoId}/index/status`
    case 'events':
      params.set('limit', String(clampLimit(limit)))
      return `${prefix}/v1/repos/${safeRepoId}/events?${params.toString()}`
    case 'sessions':
      params.set('limit', String(clampLimit(limit)))
      if (appId.trim()) params.set('app_id', appId.trim())
      if (actorId.trim()) params.set('actor_id', actorId.trim())
      return `${prefix}/v1/repos/${safeRepoId}/sessions?${params.toString()}`
  }
}

function clampLimit(value: number) {
  if (!Number.isFinite(value)) return 20
  return Math.min(1000, Math.max(1, Math.round(value)))
}

async function parseResponse(response: Response) {
  const contentType = response.headers.get('content-type') ?? ''
  if (contentType.includes('application/json')) {
    return response.json()
  }

  return response.text()
}

export default App
