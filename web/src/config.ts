/**
 * BFF configuration. Read once at boot from the environment.
 *
 * `CAIRN_SERVER_TOKEN` is read here and never leaves the server process:
 * `/api/session` reports `tokenConfigured: boolean` only, mirroring
 * `cairn-dsh-doctor`.
 */

import { isSafeScopeComponent, normalizeBaseUrl } from './cairn-client.ts'

export interface Scope {
  readonly tenant: string
  readonly knowledgeBase: string
  /** True when `--restrict-to-default-scope` fences the server away from it. */
  readonly fenced: boolean
}

export interface WebConfig {
  readonly host: string
  readonly port: number
  readonly cairnBaseUrl: string
  readonly token: string
  readonly scopes: readonly Scope[]
  readonly defaultScope: Scope | undefined
  /**
   * The knowledge base holding imported CAIRN session memory, if one is
   * configured. Session packets are only served from this scope: it is a
   * different kind of corpus (agent transcripts, not project knowledge) and
   * conflating the two would let a session-packet lookup read arbitrary KBs.
   */
  readonly sessionsScope: Scope | undefined
  readonly timeoutMs: number
  readonly retries: number
  readonly maxRetryDelayMs: number
  readonly maxResponseBytes: number
  readonly maxMetadataBytesPerHit: number
  readonly maxTextCharsPerHit: number
  readonly historyLimit: number
  /**
   * Optional JSONL file backing server-owned query history. Unset means the
   * store is purely in memory, which keeps local dev free of stray files.
   */
  readonly historyPath: string | undefined
  readonly allowHistoricalRevisions: boolean
}

export const DEFAULTS = {
  host: '127.0.0.1',
  port: 8787,
  cairnBaseUrl: 'http://127.0.0.1:8080',
  timeoutMs: 30_000,
  retries: 2,
  maxRetryDelayMs: 2_000,
  maxResponseBytes: 8 * 1024 * 1024,
  maxMetadataBytesPerHit: 4 * 1024,
  maxTextCharsPerHit: 4_000,
  historyLimit: 500,
} as const

/**
 * `CAIRN_WEB_SESSIONS_SCOPE=local/sessions`
 *
 * Must also appear in `CAIRN_WEB_SCOPES` (and not be fenced) so the sessions
 * KB is reachable through the same scope allowlist as everything else. An
 * unreachable sessions scope is a boot error, not a runtime 404.
 */
export function resolveSessionsScope(
  raw: string | undefined,
  scopes: readonly Scope[],
): Scope | undefined {
  const trimmed = raw?.trim()
  if (trimmed === undefined || trimmed.length === 0) return undefined
  const parsed = parseScopes(trimmed)
  const wanted = parsed[0]
  if (parsed.length !== 1 || wanted === undefined) {
    throw new Error('CAIRN_WEB_SESSIONS_SCOPE must name exactly one tenant/kb')
  }
  const known = scopes.find(
    scope => scope.tenant === wanted.tenant && scope.knowledgeBase === wanted.knowledgeBase,
  )
  if (known === undefined) {
    throw new Error(
      `CAIRN_WEB_SESSIONS_SCOPE ${wanted.tenant}/${wanted.knowledgeBase} must also appear in CAIRN_WEB_SCOPES`,
    )
  }
  if (known.fenced) {
    throw new Error(
      `CAIRN_WEB_SESSIONS_SCOPE ${wanted.tenant}/${wanted.knowledgeBase} is fenced and cannot serve session packets`,
    )
  }
  return known
}

/** Search request bounds enforced by `SearchRequest::validate` in `src/model.rs`. */
export const SEARCH_BOUNDS = {
  limitMin: 1,
  limitMax: 1_000,
  limitDefault: 20,
  candidateLimitMax: 100_000,
  candidateLimitDefault: 200,
  queryMaxBytes: 16 * 1024,
  filterMaxKeys: 64,
} as const

function integerEnv(env: NodeJS.ProcessEnv, key: string, fallback: number): number {
  const raw = env[key]
  if (raw === undefined || raw.trim().length === 0) return fallback
  const parsed = Number(raw)
  if (!Number.isSafeInteger(parsed) || parsed < 0) {
    throw new Error(`${key} must be a non-negative integer`)
  }
  return parsed
}

function optionalPath(raw: string | undefined): string | undefined {
  const trimmed = raw?.trim()
  return trimmed === undefined || trimmed.length === 0 ? undefined : trimmed
}

function booleanEnv(env: NodeJS.ProcessEnv, key: string): boolean {
  const raw = env[key]?.trim().toLowerCase()
  return raw === '1' || raw === 'true' || raw === 'yes'
}

/**
 * `CAIRN_WEB_SCOPES=acme/handbook,acme/runbooks!fenced`
 * A trailing `!fenced` marks a scope the server will refuse with
 * SCOPE_FORBIDDEN — the picker renders it locked instead of letting the user
 * click into a guaranteed 403.
 */
export function parseScopes(raw: string | undefined): readonly Scope[] {
  if (raw === undefined || raw.trim().length === 0) return []
  const scopes: Scope[] = []
  const seen = new Set<string>()
  for (const entry of raw.split(',')) {
    const trimmed = entry.trim()
    if (trimmed.length === 0) continue
    const fenced = trimmed.endsWith('!fenced')
    const body = fenced ? trimmed.slice(0, -'!fenced'.length) : trimmed
    const parts = body.split('/')
    if (parts.length !== 2) throw new Error(`invalid scope "${trimmed}": expected tenant/kb`)
    const [tenant, knowledgeBase] = parts
    if (
      tenant === undefined ||
      knowledgeBase === undefined ||
      !isSafeScopeComponent(tenant) ||
      !isSafeScopeComponent(knowledgeBase)
    ) {
      throw new Error(`invalid scope "${trimmed}": unsafe tenant or knowledge base component`)
    }
    const key = `${tenant}/${knowledgeBase}`
    if (seen.has(key)) continue
    seen.add(key)
    scopes.push({ tenant, knowledgeBase, fenced })
  }
  return scopes
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): WebConfig {
  const cairnBaseUrl = env.CAIRN_BASE_URL?.trim() ?? DEFAULTS.cairnBaseUrl
  // Fail at boot, not at first request.
  normalizeBaseUrl(cairnBaseUrl)

  const scopes = parseScopes(env.CAIRN_WEB_SCOPES)
  const defaultScope = scopes.find(scope => !scope.fenced)
  const port = integerEnv(env, 'CAIRN_WEB_PORT', DEFAULTS.port)
  if (port > 65_535) throw new Error('CAIRN_WEB_PORT must be <= 65535')

  return {
    host: env.CAIRN_WEB_HOST?.trim() ?? DEFAULTS.host,
    port,
    cairnBaseUrl,
    token: env.CAIRN_SERVER_TOKEN?.trim() ?? '',
    scopes,
    defaultScope,
    sessionsScope: resolveSessionsScope(env.CAIRN_WEB_SESSIONS_SCOPE, scopes),
    timeoutMs: integerEnv(env, 'CAIRN_WEB_TIMEOUT_MS', DEFAULTS.timeoutMs) || DEFAULTS.timeoutMs,
    retries: integerEnv(env, 'CAIRN_WEB_RETRIES', DEFAULTS.retries),
    maxRetryDelayMs: DEFAULTS.maxRetryDelayMs,
    maxResponseBytes: integerEnv(env, 'CAIRN_WEB_MAX_RESPONSE_BYTES', DEFAULTS.maxResponseBytes),
    maxMetadataBytesPerHit: DEFAULTS.maxMetadataBytesPerHit,
    maxTextCharsPerHit: integerEnv(
      env,
      'CAIRN_WEB_MAX_TEXT_CHARS',
      DEFAULTS.maxTextCharsPerHit,
    ),
    historyLimit: integerEnv(env, 'CAIRN_WEB_HISTORY_LIMIT', DEFAULTS.historyLimit),
    historyPath: optionalPath(env.CAIRN_WEB_HISTORY_PATH),
    allowHistoricalRevisions: booleanEnv(env, 'CAIRN_ALLOW_HISTORICAL_REVISIONS'),
  }
}
