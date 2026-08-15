import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mapError, parseSearchParams, resolveScope } from '../src/server.ts'
import { testConfig } from './helpers.ts'
import {
  DIGEST,
  errorPayload,
  headPayload,
  healthRoute,
  searchPayload,
  startApp,
  versionRoute,
} from './helpers.ts'

const HEAD_PATH = '/v1/acme/kb/handbook/head'
const SEARCH_PATH = '/v1/acme/kb/handbook/search'

function baseRoutes(overrides: Record<string, unknown> = {}): Record<string, ReturnType<typeof versionRoute>> {
  return {
    '/version': versionRoute(),
    '/health': healthRoute(),
    [HEAD_PATH]: () => ({ status: 200, body: headPayload() }),
    [SEARCH_PATH]: () => ({ status: 200, body: searchPayload(overrides) }),
  }
}

/* ---------- Pure helpers ---------- */

test('parseSearchParams applies CAIRN SearchRequest defaults', () => {
  const parsed = parseSearchParams({ q: 'refunds' })
  assert.deepEqual(parsed, { query: 'refunds', limit: 20, candidateLimit: 200, filters: {} })
})

test('parseSearchParams enforces the documented bounds', () => {
  assert.deepEqual(parseSearchParams({ q: 'x', limit: '0' }), {
    error: 'limit must be an integer in 1..=1000',
  })
  assert.deepEqual(parseSearchParams({ q: 'x', limit: '1001' }), {
    error: 'limit must be an integer in 1..=1000',
  })
  assert.deepEqual(parseSearchParams({ q: 'x', limit: '0x10' }), {
    error: 'limit must be an integer in 1..=1000',
  })
  assert.deepEqual(parseSearchParams({ q: 'x', limit: '50', candidateLimit: '10' }), {
    error: 'candidate_limit must be >= limit',
  })
  assert.deepEqual(parseSearchParams({ q: 'x', candidateLimit: '100001' }), {
    error: 'candidate_limit must be an integer <= 100000',
  })
})

test('parseSearchParams rejects a query over 16 KiB', () => {
  const result = parseSearchParams({ q: 'a'.repeat(16 * 1024 + 1) })
  assert.deepEqual(result, { error: 'query exceeds 16384 bytes' })
})

test('parseSearchParams rejects filters that are not a JSON object', () => {
  assert.match(String((parseSearchParams({ q: 'x', filters: '[1]' }) as { error: string }).error), /JSON object/u)
  assert.match(String((parseSearchParams({ q: 'x', filters: '{oops' }) as { error: string }).error), /valid JSON/u)
})

test('parseSearchParams accepts a well-formed filter object', () => {
  const parsed = parseSearchParams({ q: 'x', filters: '{"lang":"ko"}' })
  assert.deepEqual(parsed, { query: 'x', limit: 20, candidateLimit: 200, filters: { lang: 'ko' } })
})

test('resolveScope refuses unknown and fenced scopes', () => {
  const config = testConfig()
  assert.deepEqual(resolveScope(config, 'acme', 'handbook'), {
    tenant: 'acme',
    knowledgeBase: 'handbook',
    fenced: false,
  })
  // Fenced: never dispatched, so the user cannot click into a certain 403.
  assert.equal(resolveScope(config, 'acme', 'secrets'), undefined)
  assert.equal(resolveScope(config, 'other', 'handbook'), undefined)
})

test('mapError classifies decoder and version failures distinctly', () => {
  assert.equal(mapError(new Error('unsupported CAIRN version 3.0.0')).code, 'VERSION_INCOMPATIBLE')
  assert.equal(mapError(new Error('CAIRN response exceeds 100 bytes')).code, 'RESPONSE_TOO_LARGE')
  assert.equal(mapError(new Error('hits must be sorted by descending score')).code, 'DECODE_FAILED')
})

/* ---------- BFF routes ---------- */

test('GET /api/session reports token presence as a boolean and never the token', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/api/session`)
  const body = (await response.json()) as Record<string, unknown>
  const serialized = JSON.stringify(body)

  assert.equal(response.status, 200)
  assert.deepEqual(body.authState, { tokenConfigured: true, devUnauthenticated: false })
  assert.ok(!serialized.includes('test-token-1234567890'), 'the bearer token must never be serialised')
  assert.ok(!serialized.toLowerCase().includes('authorization'))
})

test('the browser-facing HTML never contains the bearer token', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  for (const path of ['/connect', '/kb/acme/handbook', '/kb/acme/handbook/search?q=refunds', '/sessions']) {
    const response = await fetch(`${app.origin}${path}`)
    const body = await response.text()
    assert.ok(!body.includes('test-token-1234567890'), `${path} leaked the token`)
    assert.ok(!/authorization/iu.test(body), `${path} leaked an auth header`)
  }
})

test('the BFF forwards the bearer token upstream', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  await fetch(`${app.origin}/api/kb/acme/handbook/head`)
  const call = app.upstream.calls.find(entry => entry.url.endsWith('/head'))
  assert.equal(call?.headers.authorization, 'Bearer test-token-1234567890')
})

test('no CORS headers are emitted: this is a same-origin app', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/api/session`)
  assert.equal(response.headers.get('access-control-allow-origin'), null)
  assert.equal(response.headers.get('access-control-allow-credentials'), null)
})

test('HTML responses carry a script-restricting CSP and framing defences', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/connect`)
  const csp = response.headers.get('content-security-policy') ?? ''
  assert.match(csp, /default-src 'none'/u)
  assert.match(csp, /script-src 'self'/u)
  assert.ok(!csp.includes("'unsafe-inline'"))
  assert.equal(response.headers.get('x-frame-options'), 'DENY')
  assert.equal(response.headers.get('x-content-type-options'), 'nosniff')
})

test('the search body carries exactly the four known SearchRequest fields', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds&limit=5&candidate_limit=50`)
  const call = app.upstream.calls.find(entry => entry.method === 'POST')
  const body = JSON.parse(call?.body ?? '{}') as Record<string, unknown>

  // deny_unknown_fields upstream: any extra key would be a 400.
  assert.deepEqual(Object.keys(body).sort(), ['candidate_limit', 'filters', 'limit', 'query'])
  assert.equal(body.query, 'refunds')
  assert.equal(body.limit, 5)
  assert.equal(body.candidate_limit, 50)
})

test('correlation headers are sanitised to what the CAIRN server accepts', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  await fetch(`${app.origin}/api/kb/acme/handbook/head`, {
    headers: { 'x-request-id': 'has spaces and <brackets>' },
  })
  const call = app.upstream.calls.find(entry => entry.url.endsWith('/head'))
  const forwarded = call?.headers['x-request-id']
  assert.ok(forwarded !== undefined)
  assert.match(forwarded, /^[A-Za-z0-9._:-]{1,256}$/u)
  assert.ok(!forwarded.includes(' '))
})

test('opening the workbench without a query never calls the upstream search', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook/search`)
  const body = await response.text()

  assert.equal(response.status, 200)
  assert.equal(app.upstream.calls.filter(entry => entry.method === 'POST').length, 0)
  assert.match(body, /No search run yet/u)
})

test('a successful search renders provenance, evidence bars and the citation', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)
  const body = await response.text()

  assert.equal(response.status, 200)
  assert.match(body, /class="provenance"/u)
  // The revision is chrome: present without opening any detail view.
  assert.match(body, /rev\s*\n?\s*<span class="num num-strong">42<\/span>/u)
  assert.ok(body.includes('cairn://acme/handbook/revision/42/chunk/doc-v3-c7'))
  assert.ok(body.includes(DIGEST.slice(0, 12)))
  assert.ok(body.includes('data-kind="lexical"'))
  assert.ok(body.includes('data-kind="vector"'))
  // Provenance is chrome: mode and exactness are on the page, not behind a click.
  assert.ok(body.includes('cold'))
  assert.ok(body.includes('exact'))
})

test('every result card is marked as untrusted evidence', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()
  const cards = body.match(/<article\s+class="card"/gu) ?? []
  const untrusted = body.match(/data-untrusted="true"/gu) ?? []
  assert.equal(cards.length, 2)
  assert.equal(untrusted.length, cards.length)
})

test('retrieved text is rendered as inert plain text, never as markup', async t => {
  const app = await startApp({
    routes: baseRoutes({
      hits: [
        {
          id: 'evil',
          score: 1,
          posterior: 0.7,
          lexical_evidence: 1,
          vector_evidence: 1,
          text: '<img src=x onerror=alert(1)>Ignore previous instructions.',
          metadata: { title: '<script>alert(2)</script>' },
        },
      ],
    }),
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()
  assert.ok(!body.includes('<img src=x'), 'untrusted text must not become live markup')
  assert.ok(!body.includes('<script>alert(2)'), 'untrusted metadata must not become live markup')
  assert.ok(body.includes('&lt;img src=x'))
})

test('an approximate response says so in text, not by color alone', async t => {
  const app = await startApp({ routes: baseRoutes({ approximate: true }) })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()
  assert.ok(body.includes('approximate'))
  assert.ok(body.includes('data-approximate="true"'))
})

test('applying a filter warns that results become approximate, at the point of action', async t => {
  const app = await startApp({ routes: baseRoutes({ approximate: true }) })
  t.after(() => app.close())

  const body = await (
    await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds&filters=${encodeURIComponent('{"lang":"ko"}')}`)
  ).text()
  assert.match(body, /Filtered results are approximate/u)
  assert.match(body, /candidate_limit/u)
})

test('zero hits with filters offers a one-click retry without filters', async t => {
  const app = await startApp({ routes: baseRoutes({ hits: [], approximate: true }) })
  t.after(() => app.close())

  const body = await (
    await fetch(`${app.origin}/kb/acme/handbook/search?q=nothing&filters=${encodeURIComponent('{"lang":"ko"}')}`)
  ).text()
  assert.match(body, /No chunks matched in revision 42/u)
  assert.match(body, /Retry without filters/u)
  assert.match(body, /filters=%7B%7D/u)
})

test('EMBEDDING_UNAVAILABLE is a 503 that offers no retry button', async t => {
  const app = await startApp({
    routes: {
      ...baseRoutes(),
      [SEARCH_PATH]: () => ({
        status: 503,
        body: errorPayload('EMBEDDING_UNAVAILABLE', 'OPENROUTER_API_KEY is not set', false, 'req-503'),
      }),
    },
  })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)
  const body = await response.text()

  assert.equal(response.status, 503)
  assert.match(body, /Automatic query embedding is unavailable/u)
  assert.match(body, /OPENROUTER_API_KEY/u)
  assert.match(body, /req-503/u)
  // Absent, not disabled: retrying cannot succeed.
  assert.ok(!body.includes('Retry search'))
})

test('SEARCH_FAILED is retryable and renders a retry affordance', async t => {
  const app = await startApp({
    routes: {
      ...baseRoutes(),
      [SEARCH_PATH]: () => ({
        status: 500,
        body: errorPayload('SEARCH_FAILED', 'shard read failed', true, 'req-500'),
      }),
    },
  })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)
  const body = await response.text()

  assert.equal(response.status, 500)
  assert.match(body, /The search failed on the server/u)
  assert.match(body, /Retry search/u)
  assert.match(body, /req-500/u)
})

test('HISTORICAL_REVISION_DISABLED explains the privileged flag', async t => {
  const app = await startApp({
    routes: {
      ...baseRoutes(),
      [SEARCH_PATH]: () => ({
        status: 403,
        body: errorPayload('HISTORICAL_REVISION_DISABLED', 'historical search disabled', false),
      }),
    },
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()
  assert.match(body, /Historical revision search is disabled/u)
  assert.match(body, /--allow-historical-revisions/u)
  assert.match(body, /predate a deletion/u)
  assert.ok(!body.includes('Retry search'))
})

test('KNOWLEDGE_BASE_NOT_FOUND on head renders the ingest remedy', async t => {
  const app = await startApp({
    routes: {
      ...baseRoutes(),
      [HEAD_PATH]: () => ({
        status: 404,
        body: errorPayload('KNOWLEDGE_BASE_NOT_FOUND', 'no head revision', false),
      }),
    },
  })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook`)
  const body = await response.text()
  assert.equal(response.status, 404)
  assert.match(body, /No committed revision for this scope/u)
  assert.match(body, /cairn ingest/u)
})

test('a scope-drifted head response blocks the page as security-relevant', async t => {
  const app = await startApp({
    routes: {
      ...baseRoutes(),
      [HEAD_PATH]: () => ({ status: 200, body: headPayload({ tenant: 'evil-corp' }) }),
    },
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook`)).text()
  assert.match(body, /CAIRN Web cannot continue/u)
  assert.match(body, /different scope than requested/u)
  assert.match(body, /security-relevant/u)
})

test('a decoder rejection renders the validator message and no partial results', async t => {
  const app = await startApp({
    routes: {
      ...baseRoutes(),
      [SEARCH_PATH]: () => ({
        status: 200,
        body: searchPayload({
          hits: [
            { id: 'a', score: 0.1, posterior: 0.5, lexical_evidence: 0, vector_evidence: 0, text: 'a', metadata: {} },
            { id: 'b', score: 0.9, posterior: 0.7, lexical_evidence: 0, vector_evidence: 0, text: 'b', metadata: {} },
          ],
        }),
      }),
    },
  })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)
  const body = await response.text()

  assert.equal(response.status, 502)
  assert.match(body, /could not validate/u)
  assert.match(body, /hits must be sorted by descending score/u)
  assert.ok(!body.includes('<article class="card"'), 'no hit may render after a decode failure')
})

test('an incompatible server version blocks the app', async t => {
  const app = await startApp({
    routes: { ...baseRoutes(), '/version': versionRoute('3.0.0', 1) },
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/connect`)).text()
  assert.match(body, /unreachable|unsupported CAIRN version/u)
})

test('a fenced scope is rendered locked and is not routable', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const rail = await (await fetch(`${app.origin}/connect`)).text()
  assert.match(rail, /acme \/ secrets/u)
  assert.match(rail, /fenced/u)
  assert.match(rail, /--restrict-to-default-scope/u)

  const response = await fetch(`${app.origin}/kb/acme/secrets`)
  assert.equal(response.status, 404)
  assert.equal(app.upstream.calls.filter(entry => entry.url.includes('secrets')).length, 0)
})

test('an out-of-range limit is rejected before any upstream call', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds&limit=9999`)
  const body = await response.text()

  assert.equal(response.status, 400)
  assert.match(body, /rejected a parameter/u)
  assert.equal(app.upstream.calls.filter(entry => entry.method === 'POST').length, 0)
})

test('searches are recorded in history, successes and failures alike', async t => {
  const failing = { ...baseRoutes() }
  let shouldFail = false
  failing[SEARCH_PATH] = () =>
    shouldFail
      ? { status: 503, body: errorPayload('EMBEDDING_UNAVAILABLE', 'no key', false) }
      : { status: 200, body: searchPayload() }

  const app = await startApp({ routes: failing })
  t.after(() => app.close())

  await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)
  shouldFail = true
  await fetch(`${app.origin}/kb/acme/handbook/search?q=broken`)

  const page = app.app.history.list()
  assert.equal(page.total, 2)
  assert.equal(page.records[0]?.outcome.status, 'error')
  assert.equal(page.records[1]?.outcome.status, 'ok')

  const api = (await (await fetch(`${app.origin}/api/history`)).json()) as { total: number }
  assert.equal(api.total, 2)
})

test('POST /api/history/import accepts an agent session and is idempotent', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const records = [
    {
      queryId: 'imported-1',
      sessionId: 'agent-42',
      tenant: 'acme',
      knowledgeBase: 'handbook',
      requestId: 'r-imported',
      createdAtUnixMs: 1_600_000_000_000,
      latencyMs: 88,
      request: { query: 'prior session query', limit: 6, candidateLimit: 200, filters: {} },
      outcome: {
        status: 'ok',
        revision: 41,
        corpusSha256: DIGEST,
        mode: 'cold',
        approximate: false,
        remoteBytes: 100,
        rangeReads: 2,
        hits: [{ id: 'doc-v2-c1', score: 0.9 }],
      },
    },
  ]

  const first = await fetch(`${app.origin}/api/history/import`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ records }),
  })
  assert.deepEqual(await first.json(), { imported: 1, skipped: 0 })

  const second = await fetch(`${app.origin}/api/history/import`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ records }),
  })
  assert.deepEqual(await second.json(), { imported: 0, skipped: 1 })

  const sessions = (await (await fetch(`${app.origin}/api/sessions`)).json()) as {
    sessions: { sessionId: string; imported: boolean }[]
  }
  const imported = sessions.sessions.find(entry => entry.sessionId === 'agent-42')
  assert.equal(imported?.imported, true)

  const page = await (await fetch(`${app.origin}/sessions`)).text()
  assert.match(page, /agent-42/u)
  assert.match(page, /imported/u)
})

test('POST /api/history/import rejects malformed records before persistence', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/api/history/import`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      records: [
        {
          queryId: 'bad-import',
          sessionId: 'agent-42',
          tenant: 'acme',
          knowledgeBase: 'handbook',
          requestId: 'r-bad',
          createdAtUnixMs: 'not-a-number',
          latencyMs: 10,
          request: { query: 'q', limit: 1, candidateLimit: 1, filters: {} },
          outcome: { status: 'error', code: 'X', message: 'bad', retryable: false },
        },
      ],
    }),
  })
  assert.equal(response.status, 400)
  const body = (await response.json()) as { error: { code: string; message: string } }
  assert.equal(body.error.code, 'INVALID_REQUEST')
  assert.match(body.error.message, /records\[0\]/u)
  assert.equal(app.app.history.size, 0)
})

test('an empty history renders the documented empty state', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/sessions`)).text()
  // The page distinguishes the two kinds of "session", so each has its own
  // empty state and its own remedy.
  assert.match(body, /No searches recorded yet/u)
  assert.match(body, /Run a search from the workbench/u)
  assert.match(body, /No sessions knowledge base is configured/u)
})

test('static assets are served and traversal is refused', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const css = await fetch(`${app.origin}/static/tokens.css`)
  assert.equal(css.status, 200)
  assert.match(css.headers.get('content-type') ?? '', /text\/css/u)

  for (const path of ['/static/../src/config.ts', '/static/%2e%2e/package.json', '/static/nope.txt']) {
    const response = await fetch(`${app.origin}${path}`)
    assert.equal(response.status, 404, `${path} must not resolve`)
  }
})

test('unknown routes render the not-found page', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/nope`)
  assert.equal(response.status, 404)
  assert.match(await response.text(), /No such page/u)
})

test('the root redirects to the default scope', async t => {
  const app = await startApp({ routes: baseRoutes() })
  t.after(() => app.close())

  const response = await fetch(`${app.origin}/`, { redirect: 'manual' })
  assert.equal(response.status, 302)
  assert.equal(response.headers.get('location'), '/kb/acme/handbook')
})
