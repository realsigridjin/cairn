/**
 * CAIRN Web BFF.
 *
 * Security posture, which is the load-bearing decision of this app:
 *   - The browser talks ONLY to this origin. It never sees CAIRN_SERVER_TOKEN.
 *   - This process holds the token and calls CAIRN server-to-server.
 *   - No CORS headers are emitted, because no cross-origin browser access is
 *     intended or supported.
 *
 * Everything the browser can reach is enumerated in `route()`. Anything else
 * is a 404.
 */

import { randomUUID } from 'node:crypto'
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http'
import { CairnClient, CairnClientError } from './cairn-client.ts'
import { SEARCH_BOUNDS, type Scope, type WebConfig } from './config.ts'
import { HistoryStore, type SearchRecord } from './history.ts'
import type { CairnHeadResponse, CairnSearchResponse, JsonValue } from './protocol.ts'
import { errorMessage } from './protocol.ts'
import { presentError, type ErrorPresentation } from './view/errors.ts'
import type { ConnectionState, ConnectRow, ShellOptions } from './view/pages.ts'
import {
  blockingErrorPage,
  connectPage,
  notFoundPage,
  scopeHomePage,
  sessionsPage,
  workbenchPage,
} from './view/pages.ts'
import { serveStatic } from './static.ts'

export interface AppOptions {
  readonly config: WebConfig
  readonly client: CairnClient
  readonly history?: HistoryStore
  readonly now?: () => number
}

export interface App {
  readonly handler: (request: IncomingMessage, response: ServerResponse) => void
  readonly history: HistoryStore
  readonly listen: (port: number, host: string) => Promise<Server>
}

const SAFE_ID = /^[A-Za-z0-9._:-]{1,256}$/u

export function createApp(options: AppOptions): App {
  const { config, client } = options
  const history = options.history ?? new HistoryStore(config.historyLimit)
  const now = options.now ?? Date.now

  /** Cached so every page render does not re-probe /version. */
  let connection: ConnectionState = {
    baseUrl: config.cairnBaseUrl,
    tokenConfigured: client.tokenConfigured,
    ok: false,
    devUnauthenticated: isLoopback(config.cairnBaseUrl) && !client.tokenConfigured,
  }

  async function refreshConnection(): Promise<ConnectionState> {
    try {
      const version = await client.version()
      connection = {
        baseUrl: config.cairnBaseUrl,
        tokenConfigured: client.tokenConfigured,
        version: version.version,
        apiVersion: version.apiVersion,
        ok: true,
        devUnauthenticated: isLoopback(config.cairnBaseUrl) && !client.tokenConfigured,
      }
    } catch (error) {
      connection = {
        baseUrl: config.cairnBaseUrl,
        tokenConfigured: client.tokenConfigured,
        ok: false,
        detail: errorMessage(error),
        devUnauthenticated: isLoopback(config.cairnBaseUrl) && !client.tokenConfigured,
      }
    }
    return connection
  }

  function shell(
    title: string,
    active?: { readonly tenant: string; readonly knowledgeBase: string },
    head?: CairnHeadResponse,
  ): ShellOptions {
    return {
      title,
      scopes: config.scopes,
      now: now(),
      connection,
      ...(active === undefined ? {} : { active }),
      ...(head === undefined ? {} : { head }),
    }
  }

  async function handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? '/', `http://${request.headers.host ?? 'localhost'}`)
    const path = url.pathname
    const method = request.method ?? 'GET'
    const requestId = correlationId(request)

    // Static assets never touch CAIRN.
    if (path.startsWith('/static/')) {
      await serveStatic(path.slice('/static/'.length), response)
      return
    }

    if (path === '/api/session' && method === 'GET') {
      await refreshConnection()
      sendJson(response, 200, {
        scopes: config.scopes,
        version: connection.version ?? null,
        apiVersion: connection.apiVersion ?? null,
        // Deliberately a boolean. The token itself is never serialised.
        authState: {
          tokenConfigured: client.tokenConfigured,
          devUnauthenticated: connection.devUnauthenticated,
        },
        ok: connection.ok,
      })
      return
    }

    if (path === '/api/health' && method === 'GET') {
      const state = await refreshConnection()
      sendJson(response, state.ok ? 200 : 502, state)
      return
    }

    const headMatch = /^\/api\/kb\/([^/]+)\/([^/]+)\/head$/u.exec(path)
    if (headMatch !== null && method === 'GET') {
      const [, tenantRaw, kbRaw] = headMatch
      const scope = resolveScope(config, tenantRaw, kbRaw)
      if (scope === undefined) {
        sendApiError(response, 404, 'INVALID_SCOPE', 'unknown or unsafe scope', requestId)
        return
      }
      try {
        const head = await client.head(scope.tenant, scope.knowledgeBase, { requestId })
        sendJson(response, 200, head)
      } catch (error) {
        const mapped = mapError(error)
        sendApiError(response, mapped.status, mapped.code, mapped.message, mapped.requestId ?? requestId)
      }
      return
    }

    const searchMatch = /^\/api\/kb\/([^/]+)\/([^/]+)\/search$/u.exec(path)
    if (searchMatch !== null && method === 'POST') {
      const [, tenantRaw, kbRaw] = searchMatch
      const scope = resolveScope(config, tenantRaw, kbRaw)
      if (scope === undefined) {
        sendApiError(response, 404, 'INVALID_SCOPE', 'unknown or unsafe scope', requestId)
        return
      }
      let body: Record<string, unknown>
      try {
        body = await readJsonBody(request, config.maxResponseBytes)
      } catch (error) {
        sendApiError(response, 400, 'INVALID_JSON', errorMessage(error), requestId)
        return
      }
      const parsed = parseSearchParams({
        q: typeof body.query === 'string' ? body.query : '',
        limit: body.limit === undefined ? undefined : String(body.limit),
        candidateLimit: body.candidate_limit === undefined ? undefined : String(body.candidate_limit),
        filters: body.filters === undefined ? undefined : JSON.stringify(body.filters),
      })
      if ('error' in parsed) {
        sendApiError(response, 400, 'INVALID_REQUEST', parsed.error, requestId)
        return
      }
      const outcome = await runSearch(scope, parsed, {
        requestId,
        ...(typeof body.session_id === 'string' && SAFE_ID.test(body.session_id)
          ? { sessionId: body.session_id }
          : {}),
        ...(typeof body.tool_call_id === 'string' ? { toolCallId: body.tool_call_id } : {}),
      })
      if (outcome.status === 'ok') {
        sendJson(response, 200, {
          ...outcome.response,
          queryId: outcome.queryId,
          requestId,
          latencyMs: outcome.latencyMs,
        })
      } else {
        sendApiError(response, outcome.status_code, outcome.code, outcome.message, outcome.requestId ?? requestId)
      }
      return
    }

    if (path === '/api/history' && method === 'GET') {
      const page = history.list({
        ...optionalParam('tenant', url.searchParams.get('tenant')),
        ...optionalParam('knowledgeBase', url.searchParams.get('kb')),
        ...optionalParam('sessionId', url.searchParams.get('session')),
        ...optionalParam('cursor', url.searchParams.get('cursor')),
      })
      sendJson(response, 200, page)
      return
    }

    if (path === '/api/history/import' && method === 'POST') {
      let body: Record<string, unknown>
      try {
        body = await readJsonBody(request, config.maxResponseBytes)
      } catch (error) {
        sendApiError(response, 400, 'INVALID_JSON', errorMessage(error), requestId)
        return
      }
      const records = Array.isArray(body.records) ? (body.records as SearchRecord[]) : undefined
      if (records === undefined) {
        sendApiError(response, 400, 'INVALID_REQUEST', 'records must be an array', requestId)
        return
      }
      const result = history.importSessions(records)
      sendJson(response, 200, result)
      return
    }

    const historyItem = /^\/api\/history\/([A-Za-z0-9._:-]{1,256})$/u.exec(path)
    if (historyItem !== null && method === 'GET') {
      const record = history.get(historyItem[1] as string)
      if (record === undefined) {
        sendApiError(response, 404, 'NOT_FOUND', 'no such query id', requestId)
        return
      }
      sendJson(response, 200, record)
      return
    }

    if (path === '/api/sessions' && method === 'GET') {
      sendJson(response, 200, { sessions: history.sessions() })
      return
    }

    /* ---------- HTML routes ---------- */

    if (path === '/' || path === '/connect') {
      await refreshConnection()
      const first = config.defaultScope
      if (path === '/' && first !== undefined) {
        redirect(response, `/kb/${encodeURIComponent(first.tenant)}/${encodeURIComponent(first.knowledgeBase)}`)
        return
      }
      let head: CairnHeadResponse | undefined
      let headRow: ConnectRow = { label: 'Revision', state: 'pending', value: 'not resolved' }
      let embeddingRow: ConnectRow = { label: 'Embedding', state: 'pending', value: 'unknown' }
      let warmRow: ConnectRow = { label: 'UQA warm', state: 'pending', value: 'unknown' }
      if (first !== undefined && connection.ok) {
        try {
          head = await client.head(first.tenant, first.knowledgeBase, { requestId })
          headRow = {
            label: 'Revision',
            state: 'ok',
            value: `${head.revision} ← HEAD${head.parentRevision === undefined ? ' (genesis)' : ` · parent ${head.parentRevision}`}`,
          }
          embeddingRow = {
            label: 'Embedding',
            state: 'ok',
            value: `${head.embeddingProvider}/${head.embeddingModel} · ${head.dimension}d`,
          }
          warmRow = head.hasUqaBundle
            ? { label: 'UQA warm', state: 'ok', value: 'bundle available' }
            : { label: 'UQA warm', state: 'warn', value: 'not materialized' }
        } catch (error) {
          const mapped = mapError(error)
          headRow = { label: 'Revision', state: 'error', value: `${mapped.code}: ${mapped.message}` }
        }
      }
      const rows: ConnectRow[] = [
        {
          label: 'Service',
          state: connection.ok ? 'ok' : 'error',
          value: connection.ok ? config.cairnBaseUrl : (connection.detail ?? 'unreachable'),
        },
        {
          label: 'Version',
          state: connection.version === undefined ? 'error' : 'ok',
          value:
            connection.version === undefined
              ? 'unknown'
              : `${connection.version} (API ${connection.apiVersion})`,
        },
        {
          label: 'Auth',
          state: client.tokenConfigured ? 'ok' : connection.devUnauthenticated ? 'warn' : 'error',
          value: client.tokenConfigured
            ? 'token configured (server-side)'
            : connection.devUnauthenticated
              ? 'loopback dev, unauthenticated'
              : 'no token configured',
        },
        {
          label: 'Scope',
          state: first === undefined ? 'error' : 'ok',
          value:
            first === undefined
              ? 'no scopes configured (set CAIRN_WEB_SCOPES)'
              : `${first.tenant} / ${first.knowledgeBase}`,
        },
        headRow,
        embeddingRow,
        warmRow,
      ]
      sendHtml(
        response,
        200,
        connectPage({ shell: shell('Connection', first, head), rows }).value,
      )
      return
    }

    const scopeHome = /^\/kb\/([^/]+)\/([^/]+)$/u.exec(path)
    if (scopeHome !== null && method === 'GET') {
      const [, tenantRaw, kbRaw] = scopeHome
      const scope = resolveScope(config, tenantRaw, kbRaw)
      if (scope === undefined) {
        sendHtml(response, 404, notFoundPage(shell('Not found'), path).value)
        return
      }
      await refreshConnection()
      try {
        const head = await client.head(scope.tenant, scope.knowledgeBase, { requestId })
        sendHtml(
          response,
          200,
          scopeHomePage({
            shell: shell(`${scope.tenant}/${scope.knowledgeBase}`, scope, head),
            tenant: scope.tenant,
            knowledgeBase: scope.knowledgeBase,
            head,
            sessions: history.sessions({
              tenant: scope.tenant,
              knowledgeBase: scope.knowledgeBase,
            }),
          }).value,
        )
      } catch (error) {
        const mapped = mapError(error)
        const presentation = presentError(mapped.code, mapped.retryable)
        const page = presentation.blocking === true
          ? blockingErrorPage({
              shell: shell('Blocked', scope),
              presentation,
              message: mapped.message,
              ...(mapped.requestId === undefined ? {} : { requestId: mapped.requestId }),
            })
          : scopeHomePage({
              shell: shell(`${scope.tenant}/${scope.knowledgeBase}`, scope),
              tenant: scope.tenant,
              knowledgeBase: scope.knowledgeBase,
              error: {
                presentation,
                message: mapped.message,
                ...(mapped.requestId === undefined ? {} : { requestId: mapped.requestId }),
              },
              sessions: history.sessions({
                tenant: scope.tenant,
                knowledgeBase: scope.knowledgeBase,
              }),
            })
        sendHtml(response, mapped.status, page.value)
      }
      return
    }

    const workbench = /^\/kb\/([^/]+)\/([^/]+)\/search$/u.exec(path)
    if (workbench !== null && method === 'GET') {
      const [, tenantRaw, kbRaw] = workbench
      const scope = resolveScope(config, tenantRaw, kbRaw)
      if (scope === undefined) {
        sendHtml(response, 404, notFoundPage(shell('Not found'), path).value)
        return
      }
      await refreshConnection()
      let head: CairnHeadResponse | undefined
      try {
        head = await client.head(scope.tenant, scope.knowledgeBase, { requestId })
      } catch {
        head = undefined
      }

      const parsed = parseSearchParams({
        q: url.searchParams.get('q') ?? '',
        limit: url.searchParams.get('limit') ?? undefined,
        candidateLimit: url.searchParams.get('candidate_limit') ?? undefined,
        filters: url.searchParams.get('filters') ?? undefined,
      })

      const state = {
        query: 'error' in parsed ? (url.searchParams.get('q') ?? '') : parsed.query,
        limit: 'error' in parsed ? SEARCH_BOUNDS.limitDefault : parsed.limit,
        candidateLimit:
          'error' in parsed ? SEARCH_BOUNDS.candidateLimitDefault : parsed.candidateLimit,
        filtersJson: url.searchParams.get('filters') ?? '{}',
        filterCount: 'error' in parsed ? 0 : Object.keys(parsed.filters).length,
      }

      if ('error' in parsed) {
        sendHtml(
          response,
          400,
          workbenchPage({
            shell: shell('Workbench', scope, head),
            tenant: scope.tenant,
            knowledgeBase: scope.knowledgeBase,
            state,
            error: {
              presentation: presentError('INVALID_REQUEST'),
              message: parsed.error,
            },
            maxTextChars: config.maxTextCharsPerHit,
          }).value,
        )
        return
      }

      // No query means no request: opening the page must not spend credits.
      if (parsed.query.length === 0) {
        sendHtml(
          response,
          200,
          workbenchPage({
            shell: shell('Workbench', scope, head),
            tenant: scope.tenant,
            knowledgeBase: scope.knowledgeBase,
            state,
            maxTextChars: config.maxTextCharsPerHit,
          }).value,
        )
        return
      }

      const outcome = await runSearch(scope, parsed, { requestId })
      if (outcome.status === 'ok') {
        sendHtml(
          response,
          200,
          workbenchPage({
            shell: shell('Workbench', scope, head),
            tenant: scope.tenant,
            knowledgeBase: scope.knowledgeBase,
            state,
            response: outcome.response,
            latencyMs: outcome.latencyMs,
            requestId,
            maxTextChars: config.maxTextCharsPerHit,
          }).value,
        )
        return
      }
      const presentation = presentError(outcome.code, outcome.retryable)
      sendHtml(
        response,
        outcome.status_code,
        workbenchPage({
          shell: shell('Workbench', scope, head),
          tenant: scope.tenant,
          knowledgeBase: scope.knowledgeBase,
          state,
          error: {
            presentation,
            message: outcome.message,
            ...(outcome.requestId === undefined ? {} : { requestId: outcome.requestId }),
            ...(presentation.retryable ? { retryHref: url.pathname + url.search } : {}),
          },
          maxTextChars: config.maxTextCharsPerHit,
        }).value,
      )
      return
    }

    if (path === '/sessions' && method === 'GET') {
      await refreshConnection()
      sendHtml(
        response,
        200,
        sessionsPage({
          shell: shell('Sessions'),
          sessions: history.sessions(),
          records: history.list({ pageSize: 50 }).records,
        }).value,
      )
      return
    }

    sendHtml(response, 404, notFoundPage(shell('Not found'), path).value)
  }

  type SearchOutcome =
    | {
        readonly status: 'ok'
        readonly response: CairnSearchResponse
        readonly queryId: string
        readonly latencyMs: number
      }
    | {
        readonly status: 'error'
        readonly status_code: number
        readonly code: string
        readonly message: string
        readonly retryable: boolean
        readonly requestId?: string
      }

  async function runSearch(
    scope: Scope,
    params: SearchParams,
    context: {
      readonly requestId: string
      readonly sessionId?: string
      readonly toolCallId?: string
    },
  ): Promise<SearchOutcome> {
    const queryId = randomUUID()
    const sessionId = context.sessionId ?? 'console'
    const startedAt = now()
    try {
      const result = await client.search({
        tenant: scope.tenant,
        knowledgeBase: scope.knowledgeBase,
        query: params.query,
        limit: params.limit,
        candidateLimit: params.candidateLimit,
        filters: params.filters,
        requestId: context.requestId,
        ...(context.toolCallId === undefined ? {} : { callId: context.toolCallId }),
      })
      const latencyMs = now() - startedAt
      history.append({
        queryId,
        sessionId,
        tenant: scope.tenant,
        knowledgeBase: scope.knowledgeBase,
        requestId: context.requestId,
        ...(context.toolCallId === undefined ? {} : { toolCallId: context.toolCallId }),
        createdAtUnixMs: startedAt,
        latencyMs,
        request: {
          query: params.query,
          limit: params.limit,
          candidateLimit: params.candidateLimit,
          filters: params.filters,
        },
        outcome: {
          status: 'ok',
          revision: result.revision,
          corpusSha256: result.corpusSha256,
          mode: result.mode,
          approximate: result.approximate,
          remoteBytes: result.remoteBytes,
          rangeReads: result.rangeReads,
          hits: result.hits.map(hit => ({ id: hit.id, score: hit.score })),
        },
      })
      return { status: 'ok', response: result, queryId, latencyMs }
    } catch (error) {
      const mapped = mapError(error)
      // Failed searches are first-class history entries.
      history.append({
        queryId,
        sessionId,
        tenant: scope.tenant,
        knowledgeBase: scope.knowledgeBase,
        requestId: context.requestId,
        ...(context.toolCallId === undefined ? {} : { toolCallId: context.toolCallId }),
        createdAtUnixMs: startedAt,
        latencyMs: now() - startedAt,
        request: {
          query: params.query,
          limit: params.limit,
          candidateLimit: params.candidateLimit,
          filters: params.filters,
        },
        outcome: {
          status: 'error',
          code: mapped.code,
          message: mapped.message,
          retryable: mapped.retryable,
        },
      })
      return {
        status: 'error',
        status_code: mapped.status,
        code: mapped.code,
        message: mapped.message,
        retryable: mapped.retryable,
        ...(mapped.requestId === undefined ? {} : { requestId: mapped.requestId }),
      }
    }
  }

  const handler = (request: IncomingMessage, response: ServerResponse): void => {
    handle(request, response).catch((error: unknown) => {
      if (!response.headersSent) {
        sendApiError(response, 500, 'INTERNAL', errorMessage(error), correlationId(request))
      } else {
        response.end()
      }
    })
  }

  return {
    handler,
    history,
    listen: (port: number, host: string) =>
      new Promise<Server>(resolve => {
        const server = createServer(handler)
        server.listen(port, host, () => resolve(server))
      }),
  }
}

/* ---------- Request parsing ---------- */

export interface SearchParams {
  readonly query: string
  readonly limit: number
  readonly candidateLimit: number
  readonly filters: Readonly<Record<string, JsonValue>>
}

/**
 * Validate against the same bounds as `SearchRequest::validate` so an
 * out-of-range value is reported at the composer field instead of costing a
 * round trip and a 400.
 */
export function parseSearchParams(input: {
  readonly q: string
  readonly limit?: string | undefined
  readonly candidateLimit?: string | undefined
  readonly filters?: string | undefined
}): SearchParams | { readonly error: string } {
  const query = input.q
  if (new TextEncoder().encode(query).byteLength > SEARCH_BOUNDS.queryMaxBytes) {
    return { error: `query exceeds ${SEARCH_BOUNDS.queryMaxBytes} bytes` }
  }

  const limit = numberOr(input.limit, SEARCH_BOUNDS.limitDefault)
  if (limit === undefined || limit < SEARCH_BOUNDS.limitMin || limit > SEARCH_BOUNDS.limitMax) {
    return { error: `limit must be an integer in ${SEARCH_BOUNDS.limitMin}..=${SEARCH_BOUNDS.limitMax}` }
  }

  const candidateLimit = numberOr(input.candidateLimit, SEARCH_BOUNDS.candidateLimitDefault)
  if (candidateLimit === undefined || candidateLimit > SEARCH_BOUNDS.candidateLimitMax) {
    return { error: `candidate_limit must be an integer <= ${SEARCH_BOUNDS.candidateLimitMax}` }
  }
  if (candidateLimit < limit) {
    return { error: 'candidate_limit must be >= limit' }
  }

  let filters: Record<string, JsonValue> = {}
  const rawFilters = input.filters?.trim()
  if (rawFilters !== undefined && rawFilters.length > 0) {
    let parsed: unknown
    try {
      parsed = JSON.parse(rawFilters)
    } catch (error) {
      return { error: `filters must be valid JSON: ${errorMessage(error)}` }
    }
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return { error: 'filters must be a JSON object' }
    }
    filters = parsed as Record<string, JsonValue>
    const keys = Object.keys(filters)
    if (keys.length > SEARCH_BOUNDS.filterMaxKeys) {
      return { error: `filters must have at most ${SEARCH_BOUNDS.filterMaxKeys} keys` }
    }
    for (const key of keys) {
      const bytes = new TextEncoder().encode(key).byteLength
      if (bytes < 1 || bytes > 256) {
        return { error: `filter key "${key}" must be 1..=256 bytes` }
      }
    }
  }

  return { query, limit, candidateLimit, filters }
}

function numberOr(raw: string | undefined, fallback: number): number | undefined {
  if (raw === undefined || raw.trim().length === 0) return fallback
  const parsed = Number(raw)
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : undefined
}

export function resolveScope(
  config: WebConfig,
  tenantRaw: string | undefined,
  kbRaw: string | undefined,
): Scope | undefined {
  if (tenantRaw === undefined || kbRaw === undefined) return undefined
  let tenant: string
  let knowledgeBase: string
  try {
    tenant = decodeURIComponent(tenantRaw)
    knowledgeBase = decodeURIComponent(kbRaw)
  } catch {
    return undefined
  }
  const known = config.scopes.find(
    scope => scope.tenant === tenant && scope.knowledgeBase === knowledgeBase,
  )
  // Fenced scopes are never dispatched: the picker renders them locked.
  if (known === undefined || known.fenced) return undefined
  return known
}

interface MappedError {
  readonly status: number
  readonly code: string
  readonly message: string
  readonly retryable: boolean
  readonly requestId?: string
}

export function mapError(error: unknown): MappedError {
  if (error instanceof CairnClientError) {
    return {
      status: error.status ?? statusForCode(error.code),
      code: error.code,
      message: error.message,
      retryable: error.retryable,
      ...(error.requestId === undefined ? {} : { requestId: error.requestId }),
    }
  }
  const message = errorMessage(error)
  if (/unsupported CAIRN (?:HTTP API )?version/u.test(message)) {
    return { status: 502, code: 'VERSION_INCOMPATIBLE', message, retryable: false }
  }
  if (/exceeds \d+ bytes/u.test(message)) {
    return { status: 502, code: 'RESPONSE_TOO_LARGE', message, retryable: false }
  }
  // A decoder rejection means the payload failed an invariant: never render
  // partial results, surface the validator message verbatim.
  return { status: 502, code: 'DECODE_FAILED', message, retryable: false }
}

function statusForCode(code: string): number {
  switch (code) {
    case 'TRANSPORT_ERROR':
      return 502
    case 'SCOPE_DRIFT':
    case 'ENDPOINT_IDENTITY':
      return 502
    case 'INVALID_SCOPE':
      return 400
    default:
      return 500
  }
}

/* ---------- HTTP helpers ---------- */

function correlationId(request: IncomingMessage): string {
  const incoming = request.headers['x-request-id']
  const candidate = Array.isArray(incoming) ? incoming[0] : incoming
  if (candidate !== undefined && SAFE_ID.test(candidate)) return candidate
  return randomUUID()
}

function optionalParam<K extends string>(
  key: K,
  value: string | null,
): Partial<Record<K, string>> {
  return value === null || value.length === 0 ? {} : ({ [key]: value } as Record<K, string>)
}

async function readJsonBody(
  request: IncomingMessage,
  maxBytes: number,
): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = []
  let total = 0
  for await (const chunk of request) {
    const buffer = chunk as Buffer
    total += buffer.byteLength
    if (total > maxBytes) throw new Error(`request body exceeds ${maxBytes} bytes`)
    chunks.push(buffer)
  }
  if (total === 0) return {}
  const parsed = JSON.parse(Buffer.concat(chunks).toString('utf8')) as unknown
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('request body must be a JSON object')
  }
  return parsed as Record<string, unknown>
}

const SECURITY_HEADERS: Readonly<Record<string, string>> = {
  // No inline scripts or styles; retrieved text is never interpreted as HTML.
  'content-security-policy':
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
  'referrer-policy': 'no-referrer',
  'x-content-type-options': 'nosniff',
  'x-frame-options': 'DENY',
}

export function sendHtml(response: ServerResponse, status: number, body: string): void {
  response.writeHead(status, {
    'content-type': 'text/html; charset=utf-8',
    'cache-control': 'no-store',
    ...SECURITY_HEADERS,
  })
  response.end(body)
}

export function sendJson(response: ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    ...SECURITY_HEADERS,
  })
  response.end(JSON.stringify(body))
}

export function sendApiError(
  response: ServerResponse,
  status: number,
  code: string,
  message: string,
  requestId: string,
): void {
  sendJson(response, status, { error: { code, message, retryable: false, request_id: requestId } })
}

function redirect(response: ServerResponse, location: string): void {
  response.writeHead(302, { location, 'cache-control': 'no-store' })
  response.end()
}

function isLoopback(baseUrl: string): boolean {
  try {
    const host = new URL(baseUrl).hostname
    return host === 'localhost' || host === '127.0.0.1' || host === '::1' || host === '[::1]'
  } catch {
    return false
  }
}

export type { ErrorPresentation }
