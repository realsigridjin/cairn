/**
 * Test fixtures and a deterministic fake CAIRN upstream.
 *
 * The fake is a `fetch` implementation, not a listening socket: no ports, no
 * sleeps, no timing luck. Every test that needs a server binds to port 0 and
 * reads the assigned port back.
 */

import type { AddressInfo } from 'node:net'
import type { Server } from 'node:http'
import { CairnClient } from '../src/cairn-client.ts'
import { loadConfig, type WebConfig } from '../src/config.ts'
import { createApp, type App } from '../src/server.ts'
import { HistoryStore } from '../src/history.ts'

export const DIGEST = 'a3f9c1b2'.repeat(8)
export const DIGEST_ALT = 'ffee0011'.repeat(8)

export function headPayload(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    tenant: 'acme',
    knowledge_base: 'handbook',
    revision: 42,
    parent_revision: 41,
    created_at_unix_ms: 1_700_000_000_000,
    embedding_provider: 'openrouter',
    embedding_model: 'qwen/qwen3-embedding-8b',
    dimension: 1024,
    shard_count: 8,
    has_uqa_bundle: false,
    ...overrides,
  }
}

export function searchPayload(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    revision: 42,
    embedding_provider: 'openrouter',
    embedding_model: 'qwen/qwen3-embedding-8b',
    dimension: 1024,
    corpus_sha256: DIGEST,
    mode: 'cold',
    score_domain: 'revision_calibrated_log_odds',
    approximate: false,
    remote_bytes: 1_258_291,
    range_reads: 34,
    hits: [
      {
        id: 'doc-v3-c7',
        score: 1.58,
        posterior: 0.8293,
        lexical_evidence: 2.1,
        vector_evidence: 0.4,
        text: 'Refunds are processed within five business days.',
        metadata: { title: 'Refund policy', source: 'handbook.pdf', page: 12, lang: 'en' },
      },
      {
        id: 'doc-v3-c9',
        score: -0.22,
        posterior: 0.4452,
        lexical_evidence: 0.3,
        vector_evidence: 1.7,
        text: 'Chargebacks follow a separate escalation path.',
        metadata: { title: 'Chargebacks', source: 'handbook.pdf', page: 31 },
      },
    ],
    ...overrides,
  }
}

export function errorPayload(
  code: string,
  message: string,
  retryable: boolean,
  requestId = 'req-test-1',
): Record<string, unknown> {
  return { error: { code, message, retryable, request_id: requestId } }
}

export interface FakeCall {
  readonly url: string
  readonly method: string
  readonly headers: Record<string, string>
  readonly body: string | undefined
}

export interface FakeUpstream {
  readonly fetch: typeof fetch
  readonly calls: FakeCall[]
}

export type Route = (call: FakeCall) => { status: number; body: unknown; headers?: Record<string, string> }

/** Builds a `fetch` that answers from a path -> handler table. */
export function fakeUpstream(routes: Record<string, Route>): FakeUpstream {
  const calls: FakeCall[] = []
  const impl: typeof fetch = async (input, init) => {
    const url = input instanceof URL ? input.toString() : String(input)
    const headers: Record<string, string> = {}
    const rawHeaders = (init?.headers ?? {}) as Record<string, string>
    for (const [key, value] of Object.entries(rawHeaders)) headers[key.toLowerCase()] = value
    const call: FakeCall = {
      url,
      method: init?.method ?? 'GET',
      headers,
      body: typeof init?.body === 'string' ? init.body : undefined,
    }
    calls.push(call)

    const path = new URL(url).pathname
    const route = routes[path]
    if (route === undefined) {
      return new Response(JSON.stringify(errorPayload('NOT_FOUND', `no route ${path}`, false)), {
        status: 404,
        headers: { 'content-type': 'application/json' },
      })
    }
    const result = route(call)
    return new Response(result.body === undefined ? null : JSON.stringify(result.body), {
      status: result.status,
      headers: { 'content-type': 'application/json', ...(result.headers ?? {}) },
    })
  }
  return { fetch: impl, calls }
}

export function versionRoute(version = '1.4.2', apiVersion = 1): Route {
  return () => ({ status: 200, body: { service: 'cairn', version, api_version: apiVersion } })
}

export function healthRoute(version = '1.4.2', apiVersion = 1): Route {
  return () => ({ status: 200, body: { ok: true, service: 'cairn', version, api_version: apiVersion } })
}

export function testConfig(overrides: Partial<WebConfig> = {}): WebConfig {
  const base = loadConfig({
    CAIRN_BASE_URL: 'http://127.0.0.1:8080',
    CAIRN_WEB_SCOPES: 'acme/handbook,acme/secrets!fenced',
    CAIRN_SERVER_TOKEN: 'test-token-1234567890',
  } as NodeJS.ProcessEnv)
  return { ...base, ...overrides }
}

export interface TestApp {
  readonly app: App
  readonly server: Server
  readonly origin: string
  readonly upstream: FakeUpstream
  readonly close: () => Promise<void>
}

/** Boots the BFF on an ephemeral port with a fake upstream. */
export async function startApp(options: {
  readonly routes: Record<string, Route>
  readonly config?: Partial<WebConfig>
  readonly now?: () => number
}): Promise<TestApp> {
  const config = testConfig(options.config)
  const upstream = fakeUpstream(options.routes)
  const client = new CairnClient({
    baseUrl: config.cairnBaseUrl,
    token: config.token,
    timeoutMs: config.timeoutMs,
    // Deterministic: no retry sleeps in tests.
    retries: 0,
    maxRetryDelayMs: 1,
    maxResponseBytes: config.maxResponseBytes,
    maxMetadataBytesPerHit: config.maxMetadataBytesPerHit,
    fetchImpl: upstream.fetch,
  })
  const app = createApp({
    config,
    client,
    history: new HistoryStore(config.historyLimit),
    ...(options.now === undefined ? {} : { now: options.now }),
  })
  const server = await app.listen(0, '127.0.0.1')
  const address = server.address() as AddressInfo
  return {
    app,
    server,
    origin: `http://127.0.0.1:${address.port}`,
    upstream,
    close: () =>
      new Promise<void>((resolve, reject) => {
        server.close(error => (error === undefined || error === null ? resolve() : reject(error)))
      }),
  }
}

export function record(overrides: Record<string, unknown> = {}): Parameters<HistoryStore['append']>[0] {
  return {
    queryId: 'q-1',
    sessionId: 's-1',
    tenant: 'acme',
    knowledgeBase: 'handbook',
    requestId: 'r-1',
    createdAtUnixMs: 1_700_000_000_000,
    latencyMs: 120,
    request: { query: 'refunds', limit: 20, candidateLimit: 200, filters: {} },
    outcome: {
      status: 'ok',
      revision: 42,
      corpusSha256: DIGEST,
      mode: 'cold',
      approximate: false,
      remoteBytes: 1024,
      rangeReads: 4,
      hits: [{ id: 'doc-v3-c7', score: 1.58 }],
    },
    ...overrides,
  } as Parameters<HistoryStore['append']>[0]
}
