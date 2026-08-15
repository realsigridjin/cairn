/**
 * Error catalogue. Every failure renders: plain-language cause -> concrete
 * remedy -> code -> request_id.
 *
 * The retry affordance is driven by `retryable` from the catalogue, not by
 * HTTP status: EMBEDDING_UNAVAILABLE is a 503 that must NOT offer retry,
 * because `classify_runtime_error` marks it non-retryable and the client
 * honours that. A retry button that cannot succeed is worse than no button,
 * so for non-retryable codes it is ABSENT, not disabled.
 */

export interface ErrorPresentation {
  readonly code: string
  readonly headline: string
  readonly remedy: string
  /** Optional shell command that fixes the condition. */
  readonly command?: string
  /** Non-retrying alternative action, e.g. "search current HEAD instead". */
  readonly alternative?: string
  readonly retryable: boolean
  /** Blocks the whole app rather than just the current view. */
  readonly blocking?: boolean
}

const CATALOGUE: Readonly<Record<string, ErrorPresentation>> = {
  UNAUTHORIZED: {
    code: 'UNAUTHORIZED',
    headline: 'The server rejected this token.',
    remedy:
      'Confirm CAIRN_SERVER_TOKEN matches the value used by `cairn serve`. The token is held by this web server and is never sent to the browser.',
    retryable: false,
  },
  SCOPE_FORBIDDEN: {
    code: 'SCOPE_FORBIDDEN',
    headline: 'This server instance is fenced to a different scope.',
    remedy:
      'The CAIRN server was started with --restrict-to-default-scope. Switch to the allowed scope, or start a server for this tenant and knowledge base.',
    retryable: false,
  },
  HISTORICAL_REVISION_DISABLED: {
    code: 'HISTORICAL_REVISION_DISABLED',
    headline: 'Historical revision search is disabled.',
    remedy:
      'Restart the CAIRN server with --allow-historical-revisions. Note that an older revision can predate a deletion, so historical access is privileged.',
    command: 'cairn serve --allow-historical-revisions',
    alternative: 'Search current HEAD instead',
    retryable: false,
  },
  EMBEDDING_UNAVAILABLE: {
    code: 'EMBEDDING_UNAVAILABLE',
    headline: 'Automatic query embedding is unavailable.',
    remedy:
      'Set OPENROUTER_API_KEY on the CAIRN server, or supply a query vector. Retrying will not help until the provider is reachable.',
    retryable: false,
  },
  QUERY_VECTOR_REQUIRED: {
    code: 'QUERY_VECTOR_REQUIRED',
    headline: 'This revision uses external embeddings.',
    remedy:
      'Provide a query vector matching the revision dimension, or run a lexical-phrased query against a revision with a managed embedding provider.',
    retryable: false,
  },
  INVALID_REQUEST: {
    code: 'INVALID_REQUEST',
    headline: 'The server rejected a parameter.',
    remedy:
      'Check limit (1..=1000), candidate_limit (>= limit, <= 100000), and filters (<= 64 keys). The server message below names the offending field.',
    retryable: false,
  },
  INVALID_JSON: {
    code: 'INVALID_JSON',
    headline: 'Malformed request.',
    remedy:
      'This is a bug in CAIRN Web, not in your input. Report it with the request id below.',
    retryable: false,
  },
  INVALID_SCOPE: {
    code: 'INVALID_SCOPE',
    headline: 'The tenant or knowledge base name is not valid.',
    remedy:
      'Scope components must be 1..=128 characters of [A-Za-z0-9._-] and start with an alphanumeric.',
    retryable: false,
  },
  KNOWLEDGE_BASE_NOT_FOUND: {
    code: 'KNOWLEDGE_BASE_NOT_FOUND',
    headline: 'No committed revision for this scope.',
    remedy: 'Ingest a corpus to create the first revision, then reload this page.',
    command: 'cairn ingest chunks.jsonl --dev-calibration',
    retryable: false,
  },
  SEARCH_FAILED: {
    code: 'SEARCH_FAILED',
    headline: 'The search failed on the server.',
    remedy:
      'This is transient. Retry, and if it persists check the CAIRN server logs using the request id below.',
    retryable: true,
  },
  TRANSPORT_ERROR: {
    code: 'TRANSPORT_ERROR',
    headline: 'Could not reach the CAIRN server.',
    remedy:
      'Check that `cairn serve` is running and CAIRN_BASE_URL is correct. Plain HTTP is accepted only for exact loopback hosts; everything else must be HTTPS.',
    retryable: true,
  },
  VERSION_INCOMPATIBLE: {
    code: 'VERSION_INCOMPATIBLE',
    headline: 'Unsupported CAIRN version.',
    remedy: 'CAIRN Web supports >=1.0.0 <2.0.0 with api_version 1. Upgrade or downgrade to match.',
    retryable: false,
    blocking: true,
  },
  ENDPOINT_IDENTITY: {
    code: 'ENDPOINT_IDENTITY',
    headline: 'That endpoint is not a CAIRN server.',
    remedy: 'The /version response did not report service "cairn". Check CAIRN_BASE_URL.',
    retryable: false,
    blocking: true,
  },
  SCOPE_DRIFT: {
    code: 'SCOPE_DRIFT',
    headline: 'The server returned a different scope than requested.',
    remedy:
      'Treat this as security-relevant: a proxy or misconfigured server may be serving another tenant. Stop and verify CAIRN_BASE_URL before trusting any result.',
    retryable: false,
    blocking: true,
  },
  RESPONSE_TOO_LARGE: {
    code: 'RESPONSE_TOO_LARGE',
    headline: 'The response exceeded the configured size budget.',
    remedy: 'Lower limit, or reduce the per-hit text budget, then run the search again.',
    retryable: false,
  },
  DECODE_FAILED: {
    code: 'DECODE_FAILED',
    headline: 'The server returned a response CAIRN Web could not validate.',
    remedy:
      'No results are shown because the payload failed invariant checks. The validator message is reproduced verbatim below.',
    retryable: false,
  },
  SESSIONS_KB_NOT_CONFIGURED: {
    code: 'SESSIONS_KB_NOT_CONFIGURED',
    headline: 'No sessions knowledge base is configured.',
    remedy:
      'Continuation packets read imported agent sessions from a dedicated knowledge base. Import and ingest local sessions, then set CAIRN_WEB_SESSIONS_SCOPE=tenant/kb to a scope that also appears in CAIRN_WEB_SCOPES.',
    command: 'python3 scripts/session_import.py --out .cairn/session-chunks.jsonl',
    retryable: false,
  },
  NO_SCOPES: {
    code: 'NO_SCOPES',
    headline: 'No knowledge base is reachable.',
    remedy: 'Configure CAIRN_WEB_SCOPES with one or more tenant/kb pairs, then restart CAIRN Web.',
    command: 'cairn init --tenant acme --kb handbook',
    retryable: false,
  },
}

const FALLBACK: ErrorPresentation = {
  code: 'UNKNOWN',
  headline: 'The request failed.',
  remedy:
    'CAIRN Web has no specific guidance for this code. The server message and request id below are the fastest route to a diagnosis.',
  retryable: false,
}

/**
 * Resolve a code to its presentation. `retryable` from the wire wins only
 * when the code is unknown to the catalogue — known codes have deliberate,
 * verified retry semantics that must not be overridden by a server envelope.
 */
export function presentError(
  code: string,
  wireRetryable?: boolean,
): ErrorPresentation {
  const known = CATALOGUE[code]
  if (known !== undefined) return known
  return {
    ...FALLBACK,
    code,
    ...(wireRetryable === undefined ? {} : { retryable: wireRetryable }),
  }
}

export function isKnownErrorCode(code: string): boolean {
  return Object.hasOwn(CATALOGUE, code)
}

export function errorCodes(): readonly string[] {
  return Object.keys(CATALOGUE)
}
