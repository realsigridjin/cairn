/**
 * Server-side CAIRN client. Runs ONLY in the BFF process.
 *
 * Holds `CAIRN_SERVER_TOKEN` and never emits it. Mirrors the transport rules
 * verified in `integrations/deepseek-harness/src/client.ts`:
 *   - HTTPS required except for exact loopback hosts
 *   - major version 1 and api_version 1 only
 *   - `retryable: false` is honoured even on 5xx (EMBEDDING_UNAVAILABLE is 503
 *     but must not be retried)
 *   - correlation headers sanitised to [A-Za-z0-9._:-]{1,256} or the Rust
 *     server drops them silently (`src/server.rs:safe_correlation_header`)
 *   - search body carries ONLY known SearchRequest fields; the server uses
 *     `deny_unknown_fields`, so an extra key is a 400
 */

import {
  decodeApiError,
  decodeHeadResponse,
  decodeSearchResponse,
  type CairnHeadResponse,
  type CairnSearchResponse,
  type JsonValue,
} from './protocol.ts'

export interface CairnClientOptions {
  readonly baseUrl: string
  readonly token: string
  readonly timeoutMs: number
  readonly retries: number
  readonly maxRetryDelayMs: number
  readonly maxResponseBytes: number
  readonly maxMetadataBytesPerHit: number
  readonly fetchImpl?: typeof fetch
}

export interface SearchInput {
  readonly tenant: string
  readonly knowledgeBase: string
  readonly query: string
  readonly limit: number
  readonly candidateLimit: number
  readonly filters: Readonly<Record<string, JsonValue>>
  readonly callId?: string
  readonly requestId?: string
}

export class CairnClientError extends Error {
  readonly code: string
  readonly retryable: boolean
  readonly status: number | undefined
  readonly requestId: string | undefined

  constructor(
    message: string,
    options: {
      readonly code: string
      readonly retryable: boolean
      readonly status?: number
      readonly requestId?: string
      readonly cause?: unknown
    },
  ) {
    super(message, { cause: options.cause })
    this.name = 'CairnClientError'
    this.code = options.code
    this.retryable = options.retryable
    this.status = options.status
    this.requestId = options.requestId
  }
}

const SAFE_SCOPE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u
const SAFE_CORRELATION = /^[A-Za-z0-9._:-]{1,256}$/u
const JSON_CONTENT_TYPE = /^(application\/json|[^;]+\+json)(?:;|$)/iu
const RETRYABLE_STATUS = new Set([408, 425, 429, 500, 502, 503, 504])

export function isSafeScopeComponent(value: string): boolean {
  return SAFE_SCOPE.test(value)
}

/** Correlation headers the Rust server will actually keep. */
export function sanitizeCorrelationId(value: string | undefined): string | undefined {
  if (value === undefined) return undefined
  return SAFE_CORRELATION.test(value) ? value : undefined
}

export function normalizeBaseUrl(raw: string): URL {
  const parsed = new URL(raw.endsWith('/') ? raw : `${raw}/`)
  const host = parsed.hostname
  const localHttp =
    parsed.protocol === 'http:' &&
    (host === 'localhost' || host === '127.0.0.1' || host === '[::1]' || host === '::1')
  if (parsed.protocol !== 'https:' && !localHttp) {
    throw new Error(
      'CAIRN baseUrl must use HTTPS; plain HTTP is allowed only for exact loopback hosts',
    )
  }
  if (parsed.username.length > 0 || parsed.password.length > 0) {
    throw new Error('CAIRN baseUrl must not contain credentials')
  }
  if (parsed.search.length > 0 || parsed.hash.length > 0) {
    throw new Error('CAIRN baseUrl must not contain a query string or fragment')
  }
  const decodedPath = decodeURIComponent(parsed.pathname)
  if (decodedPath.split('/').some(part => part === '..') || /%2f|%5c/iu.test(parsed.pathname)) {
    throw new Error('CAIRN baseUrl contains an unsafe path')
  }
  return parsed
}

export function assertCompatibleVersion(version: string, apiVersion: number): void {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/u.exec(version)
  if (match === null || Number(match[1]) !== 1) {
    throw new Error(`unsupported CAIRN version ${version}; expected >=1.0.0 <2.0.0`)
  }
  if (apiVersion !== 1) throw new Error(`unsupported CAIRN HTTP API version ${apiVersion}`)
}

export interface CairnVersion {
  readonly service: string
  readonly version: string
  readonly apiVersion: number
}

export class CairnClient {
  readonly #options: CairnClientOptions
  readonly #base: URL
  readonly #fetch: typeof fetch

  constructor(options: CairnClientOptions) {
    if (!Number.isSafeInteger(options.timeoutMs) || options.timeoutMs < 1) {
      throw new Error('invalid timeoutMs')
    }
    if (!Number.isSafeInteger(options.retries) || options.retries < 0 || options.retries > 5) {
      throw new Error('invalid retries')
    }
    this.#options = options
    this.#base = normalizeBaseUrl(options.baseUrl)
    this.#fetch = options.fetchImpl ?? fetch
  }

  get tokenConfigured(): boolean {
    return this.#options.token.length > 0
  }

  get baseUrl(): string {
    return this.#base.toString()
  }

  /** `/version` is public; it is the compatibility gate before anything else. */
  async version(signal?: AbortSignal): Promise<CairnVersion> {
    const value = await this.#request('version', { method: 'GET' }, signal)
    const root = asRecord(value)
    const version = typeof root.version === 'string' ? root.version : ''
    const apiVersion = Number(root.api_version)
    const service = typeof root.service === 'string' ? root.service : ''
    if (service !== 'cairn') {
      throw new CairnClientError(`endpoint is not a CAIRN server (service=${service || 'unknown'})`, {
        code: 'ENDPOINT_IDENTITY',
        retryable: false,
      })
    }
    assertCompatibleVersion(version, apiVersion)
    return { service, version, apiVersion }
  }

  async health(signal?: AbortSignal): Promise<{ readonly ok: boolean } & CairnVersion> {
    const value = await this.#request('health', { method: 'GET' }, signal)
    const root = asRecord(value)
    const version = typeof root.version === 'string' ? root.version : ''
    const apiVersion = Number(root.api_version)
    const service = typeof root.service === 'string' ? root.service : ''
    assertCompatibleVersion(version, apiVersion)
    return { ok: root.ok === true, service, version, apiVersion }
  }

  async head(
    tenant: string,
    knowledgeBase: string,
    options: { readonly requestId?: string; readonly signal?: AbortSignal } = {},
  ): Promise<CairnHeadResponse> {
    assertScope(tenant, knowledgeBase)
    const value = await this.#request(
      kbPath(tenant, knowledgeBase, 'head'),
      { method: 'GET', headers: correlationHeaders(options.requestId, undefined) },
      options.signal,
    )
    const head = decodeHeadResponse(value)
    // Scope drift is security-relevant, not a generic failure.
    if (head.tenant !== tenant || head.knowledgeBase !== knowledgeBase) {
      throw new CairnClientError(
        `CAIRN returned scope ${head.tenant}/${head.knowledgeBase} but ${tenant}/${knowledgeBase} was requested`,
        { code: 'SCOPE_DRIFT', retryable: false },
      )
    }
    return head
  }

  async search(input: SearchInput, signal?: AbortSignal): Promise<CairnSearchResponse> {
    assertScope(input.tenant, input.knowledgeBase)
    const value = await this.#request(
      kbPath(input.tenant, input.knowledgeBase, 'search'),
      {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          ...correlationHeaders(input.requestId, input.callId),
        },
        // Exactly the known SearchRequest fields: deny_unknown_fields is a 400.
        body: JSON.stringify({
          query: input.query,
          limit: input.limit,
          candidate_limit: input.candidateLimit,
          filters: input.filters,
        }),
      },
      signal,
    )
    return decodeSearchResponse(value, {
      maxHits: input.limit,
      maxMetadataBytesPerHit: this.#options.maxMetadataBytesPerHit,
    })
  }

  async #request(path: string, init: RequestInit, parentSignal?: AbortSignal): Promise<unknown> {
    const deadline = new AbortController()
    const onAbort = (): void => deadline.abort(parentSignal?.reason)
    parentSignal?.addEventListener('abort', onAbort, { once: true })
    const timer = setTimeout(
      () => deadline.abort(new Error('CAIRN request deadline exceeded')),
      this.#options.timeoutMs,
    )
    try {
      let attempt = 0
      for (;;) {
        try {
          const response = await this.#fetch(new URL(path, this.#base), {
            ...init,
            redirect: 'error',
            signal: deadline.signal,
            headers: {
              accept: 'application/json',
              ...this.#authHeaders(),
              ...((init.headers as Record<string, string> | undefined) ?? {}),
            },
          })
          const requestId = response.headers.get('x-request-id') ?? undefined
          const contentType = response.headers.get('content-type') ?? ''
          const bytes = await readBoundedBody(
            response,
            this.#options.maxResponseBytes,
            deadline.signal,
          )
          const parsed = parseJson(bytes, contentType)
          if (response.ok) return parsed
          const apiError = decodeApiError(parsed, response.status)
          const shouldRetry =
            attempt < this.#options.retries &&
            RETRYABLE_STATUS.has(response.status) &&
            apiError.retryable
          if (shouldRetry) {
            await sleep(this.#retryDelay(response, attempt), deadline.signal)
            attempt += 1
            continue
          }
          const errorRequestId = apiError.requestId ?? requestId
          throw new CairnClientError(apiError.message, {
            code: apiError.code,
            retryable: apiError.retryable,
            status: response.status,
            ...(errorRequestId === undefined ? {} : { requestId: errorRequestId }),
          })
        } catch (error) {
          if (deadline.signal.aborted) throw deadline.signal.reason ?? error
          if (error instanceof CairnClientError) throw error
          if (attempt >= this.#options.retries) {
            throw new CairnClientError(`CAIRN transport failed: ${message(error)}`, {
              code: 'TRANSPORT_ERROR',
              retryable: true,
              cause: error,
            })
          }
          await sleep(this.#retryDelay(undefined, attempt), deadline.signal)
          attempt += 1
        }
      }
    } finally {
      clearTimeout(timer)
      parentSignal?.removeEventListener('abort', onAbort)
    }
  }

  #authHeaders(): Record<string, string> {
    const token = this.#options.token.trim()
    if (token.length === 0) return {}
    if (token.length < 16 || token.length > 4096 || !/^[!-~]+$/u.test(token)) {
      throw new Error('CAIRN_SERVER_TOKEN must contain 16..=4096 visible ASCII bytes')
    }
    return { authorization: `Bearer ${token}` }
  }

  #retryDelay(response: Response | undefined, attempt: number): number {
    const retryAfter = response?.headers.get('retry-after')
    if (retryAfter !== null && retryAfter !== undefined) {
      const seconds = Number(retryAfter)
      if (Number.isFinite(seconds) && seconds >= 0) {
        return Math.min(this.#options.maxRetryDelayMs, Math.round(seconds * 1000))
      }
    }
    return Math.min(this.#options.maxRetryDelayMs, 100 * 2 ** attempt)
  }
}

function assertScope(tenant: string, knowledgeBase: string): void {
  if (!SAFE_SCOPE.test(tenant) || !SAFE_SCOPE.test(knowledgeBase)) {
    throw new CairnClientError('tenant and knowledgeBase must be safe CAIRN scope components', {
      code: 'INVALID_SCOPE',
      retryable: false,
    })
  }
}

function kbPath(tenant: string, kb: string, action: 'head' | 'search'): string {
  return `v1/${encodeURIComponent(tenant)}/kb/${encodeURIComponent(kb)}/${action}`
}

function correlationHeaders(
  requestId: string | undefined,
  callId: string | undefined,
): Record<string, string> {
  const safeRequestId = sanitizeCorrelationId(requestId)
  const safeCallId = sanitizeCorrelationId(callId)
  return {
    ...(safeRequestId === undefined ? {} : { 'x-request-id': safeRequestId }),
    ...(safeCallId === undefined ? {} : { 'x-dsh-tool-call-id': safeCallId }),
  }
}

async function readBoundedBody(
  response: Response,
  maxBytes: number,
  signal: AbortSignal,
): Promise<Uint8Array> {
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > maxBytes) {
    throw new Error(`CAIRN response exceeds ${maxBytes} bytes`)
  }
  if (response.body === null) return new Uint8Array()
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    for (;;) {
      if (signal.aborted) throw signal.reason
      const { done, value } = await reader.read()
      if (done) break
      total += value.byteLength
      if (total > maxBytes) throw new Error(`CAIRN response exceeds ${maxBytes} bytes`)
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
  const merged = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    merged.set(chunk, offset)
    offset += chunk.byteLength
  }
  return merged
}

function parseJson(bytes: Uint8Array, contentType: string): unknown {
  if (bytes.byteLength === 0) return undefined
  if (contentType.length > 0 && !JSON_CONTENT_TYPE.test(contentType)) {
    throw new Error(`CAIRN returned unexpected content-type ${contentType}`)
  }
  return JSON.parse(new TextDecoder().decode(bytes)) as unknown
}

function asRecord(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('CAIRN response must be a JSON object')
  }
  return value as Record<string, unknown>
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(signal.reason as Error)
      return
    }
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort)
      resolve()
    }, ms)
    const onAbort = (): void => {
      clearTimeout(timer)
      reject(signal.reason as Error)
    }
    signal.addEventListener('abort', onAbort, { once: true })
  })
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
