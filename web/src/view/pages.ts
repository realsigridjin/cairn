/**
 * Page composition. Every page is a full server-rendered document; the
 * client-side script layer is progressive enhancement only, so the workbench
 * works with JavaScript disabled.
 */

import type { Scope, WebConfig } from '../config.ts'
import type { SearchRecord, SessionSummary } from '../history.ts'
import type { CairnHeadResponse, CairnSearchResponse } from '../protocol.ts'
import type { SessionPacket } from '../session-packet.ts'
import { SEARCH_BOUNDS } from '../config.ts'
import type { ErrorPresentation } from './errors.ts'
import {
  approximateChip,
  digestChip,
  emptyState,
  errorState,
  modeChip,
  notice,
  packetLookupForm,
  provenanceStrip,
  resultList,
  resultSummary,
  scopeRail,
  sessionPacketSection,
  sessionSummaryList,
  sessionTimeline,
  strataRail,
} from './components.ts'
import { formatCount, formatTimestamp, isoTimestamp } from './format.ts'
import { html, join, type SafeHtml } from './html.ts'

export interface ShellOptions {
  readonly title: string
  readonly scopes: readonly Scope[]
  readonly active?: { readonly tenant: string; readonly knowledgeBase: string }
  readonly head?: CairnHeadResponse
  readonly now: number
  readonly connection: ConnectionState
}

export interface ConnectionState {
  readonly baseUrl: string
  readonly tokenConfigured: boolean
  readonly version?: string
  readonly apiVersion?: number
  readonly ok: boolean
  readonly detail?: string
  /** Loopback with no token: a dev badge, not an error. */
  readonly devUnauthenticated: boolean
}

export function layout(options: ShellOptions, main: SafeHtml): SafeHtml {
  return html`<!doctype html>
<html lang="en" data-theme="dark">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <meta name="color-scheme" content="dark light" />
    <title>${options.title} · CAIRN</title>
    <link rel="stylesheet" href="/static/tokens.css" />
    <link rel="stylesheet" href="/static/app.css" />
    <link rel="icon" href="/static/favicon.svg" type="image/svg+xml" />
  </head>
  <body>
    <a class="skip-link" href="#main">Skip to main content</a>
    <div class="shell">
      <nav class="rail" aria-label="Scope and revision">
        <div class="brand">
          <span class="brand-mark">CAIRN</span>
          <span class="t-caption">console</span>
        </div>

        ${connectionSummary(options.connection)}

        <section class="stack" aria-labelledby="rail-scopes">
          <h2 class="t-caption" id="rail-scopes">Scopes</h2>
          ${scopeRail(options.scopes, options.active)}
        </section>

        <section class="stack" aria-labelledby="rail-strata">
          <h2 class="t-caption" id="rail-strata">Strata</h2>
          ${strataRail(options.head, options.now)}
        </section>

        <section class="stack" aria-labelledby="rail-nav">
          <h2 class="t-caption" id="rail-nav">Navigate</h2>
          <a class="btn" data-variant="ghost" href="/connect">Connection</a>
          <a class="btn" data-variant="ghost" href="/sessions">Sessions</a>
          <a class="btn" data-variant="ghost" href="/sessions/packet">Continuation packet</a>
        </section>
      </nav>

      <main class="main" id="main" tabindex="-1">${main}</main>
    </div>
    <script type="module" src="/static/app.js"></script>
  </body>
</html>`
}

function connectionSummary(connection: ConnectionState): SafeHtml {
  return html`<div class="stratum" data-state="${connection.ok ? 'committed' : 'draft'}">
    <p class="t-caption">Endpoint</p>
    <p class="t-small chip-mono">${connection.baseUrl}</p>
    <div class="row row-tight">
      ${connection.ok
        ? html`<span class="chip" data-tone="ok"><span aria-hidden="true">✓</span> reachable</span>`
        : html`<span class="chip" data-tone="error"
            ><span aria-hidden="true">✕</span> unreachable</span
          >`}
      ${connection.version === undefined
        ? ''
        : html`<span class="chip chip-mono"
            >${connection.version} · API ${connection.apiVersion}</span
          >`}
    </div>
    <div class="row row-tight">
      ${connection.tokenConfigured
        ? html`<span class="chip" data-tone="ok"
            ><span aria-hidden="true">⚿</span> token configured (server-side)</span
          >`
        : connection.devUnauthenticated
          ? html`<span class="chip" data-tone="warn"
              ><span aria-hidden="true">⌂</span> loopback dev, no token</span
            >`
          : html`<span class="chip" data-tone="error"
              ><span aria-hidden="true">⚠</span> no token configured</span
            >`}
    </div>
  </div>`
}

/* ---------- /connect ---------- */

export interface ConnectRow {
  readonly label: string
  readonly state: 'ok' | 'pending' | 'warn' | 'error'
  readonly value: string
}

export function connectPage(options: {
  readonly shell: ShellOptions
  readonly rows: readonly ConnectRow[]
}): SafeHtml {
  const icon = (state: ConnectRow['state']): string =>
    state === 'ok' ? '✓' : state === 'error' ? '✕' : state === 'warn' ? '!' : '◦'
  const tone = (state: ConnectRow['state']): string =>
    state === 'ok' ? 'ok' : state === 'error' ? 'error' : state === 'warn' ? 'warn' : 'info'

  return layout(
    options.shell,
    html`<section class="section">
      <div class="section-head">
        <h1 class="t-h1">Connection</h1>
        <p class="t-small t-tertiary">
          The bearer token is held by this server and never sent to the browser.
        </p>
      </div>

      <ul class="stack" aria-label="Connection checklist">
        ${join(
          options.rows.map(
            row => html`<li class="stratum" data-state="committed">
              <div class="row">
                <span class="chip" data-tone="${tone(row.state)}"
                  ><span aria-hidden="true">${icon(row.state)}</span> ${row.label}</span
                >
                <span class="t-small chip-mono">${row.value}</span>
              </div>
            </li>`,
          ),
        )}
      </ul>

      ${notice(
        'info',
        'i',
        html`A live query probe spends embedding credits. Run one from the workbench with an
        explicit query rather than automatically on connect.`,
      )}
    </section>`,
  )
}

/* ---------- /kb/:tenant/:kb — scope home ---------- */

export function scopeHomePage(options: {
  readonly shell: ShellOptions
  readonly tenant: string
  readonly knowledgeBase: string
  readonly head?: CairnHeadResponse
  readonly error?: { readonly presentation: ErrorPresentation; readonly message: string; readonly requestId?: string }
  readonly sessions: readonly SessionSummary[]
}): SafeHtml {
  const head = options.head
  return layout(
    options.shell,
    html`<section class="section">
      <div class="section-head">
        <h1 class="t-h1">${options.tenant} / ${options.knowledgeBase}</h1>
        <a
          class="btn"
          data-variant="primary"
          href="/kb/${encodeURIComponent(options.tenant)}/${encodeURIComponent(options.knowledgeBase)}/search"
          >Open workbench</a
        >
      </div>

      ${options.error !== undefined
        ? errorState(options.error.presentation, {
            message: options.error.message,
            ...(options.error.requestId === undefined ? {} : { requestId: options.error.requestId }),
          })
        : head === undefined
          ? emptyState({
              title: 'This knowledge base has no committed revision.',
              body: 'Ingest a corpus to create revision 1, then reload.',
              command: 'cairn ingest chunks.jsonl --dev-calibration',
            })
          : html`<div class="stratum" data-state="head">
              <div class="row">
                <span class="chip" data-tone="head">rev ${head.revision} · HEAD</span>
                ${head.parentRevision === undefined
                  ? html`<span class="chip">genesis</span>`
                  : html`<span class="chip">parent ${head.parentRevision}</span>`}
                <time class="t-small t-tertiary" datetime="${isoTimestamp(head.createdAtUnixMs)}"
                  >published ${formatTimestamp(head.createdAtUnixMs, options.shell.now)}</time
                >
              </div>
              <div class="row">
                <span class="chip chip-mono"
                  >${head.embeddingProvider}/${head.embeddingModel} ${head.dimension}d</span
                >
                <span class="chip chip-mono">${formatCount(head.shardCount)} shards</span>
                ${head.hasUqaBundle
                  ? html`<span class="chip" data-tone="warm"
                      ><span aria-hidden="true">◆</span> UQA warm available</span
                    >`
                  : html`<span class="chip" data-tone="cold"
                      ><span aria-hidden="true">◇</span> cold only</span
                    >`}
              </div>
            </div>`}

      <div class="section-head">
        <h2 class="t-h2">Recent sessions</h2>
      </div>
      ${options.sessions.length === 0
        ? emptyState({
            title: 'No retrieval sessions recorded yet.',
            body: 'Sessions appear here once searches run through this console or an agent session is imported.',
          })
        : sessionSummaryList(options.sessions, options.shell.now)}
    </section>`,
  )
}

/* ---------- /kb/:tenant/:kb/search — the workbench ---------- */

export interface WorkbenchState {
  readonly query: string
  readonly limit: number
  readonly candidateLimit: number
  readonly filtersJson: string
  readonly filterCount: number
}

export function workbenchPage(options: {
  readonly shell: ShellOptions
  readonly tenant: string
  readonly knowledgeBase: string
  readonly state: WorkbenchState
  readonly response?: CairnSearchResponse
  readonly latencyMs?: number
  readonly requestId?: string
  readonly error?: {
    readonly presentation: ErrorPresentation
    readonly message: string
    readonly requestId?: string
    readonly retryHref?: string
  }
  readonly maxTextChars: number
}): SafeHtml {
  const action = `/kb/${encodeURIComponent(options.tenant)}/${encodeURIComponent(options.knowledgeBase)}/search`
  const response = options.response
  const filtersActive = options.state.filterCount > 0

  return layout(
    options.shell,
    html`<section class="section">
      <div class="section-head">
        <h1 class="t-h1">Workbench</h1>
        <p class="t-small t-tertiary">${options.tenant} / ${options.knowledgeBase}</p>
      </div>

      <form class="stratum composer" data-state="draft" method="get" action="${action}">
        <div class="field">
          <label class="t-caption" for="q">Query</label>
          <textarea
            class="textarea"
            id="q"
            name="q"
            rows="3"
            maxlength="${SEARCH_BOUNDS.queryMaxBytes}"
            placeholder="Ask the corpus. Press Ctrl/Cmd+Enter to run."
            data-shortcut="query"
          >
${options.state.query}</textarea
          >
        </div>

        <div class="budget-grid">
          <div class="field">
            <label class="t-caption" for="limit">limit</label>
            <input
              class="input num"
              id="limit"
              name="limit"
              type="number"
              min="${SEARCH_BOUNDS.limitMin}"
              max="${SEARCH_BOUNDS.limitMax}"
              value="${options.state.limit}"
            />
          </div>
          <div class="field">
            <label class="t-caption" for="candidate_limit">candidate_limit</label>
            <input
              class="input num"
              id="candidate_limit"
              name="candidate_limit"
              type="number"
              min="${options.state.limit}"
              max="${SEARCH_BOUNDS.candidateLimitMax}"
              value="${options.state.candidateLimit}"
            />
          </div>
        </div>

        <div class="field">
          <label class="t-caption" for="filters">filters (JSON object)</label>
          <textarea class="textarea" id="filters" name="filters" rows="2" spellcheck="false">
${options.state.filtersJson}</textarea
          >
        </div>

        ${filtersActive
          ? notice(
              'warn',
              '≈',
              html`<strong>Filtered results are approximate.</strong> Metadata filtering happens
              after a bounded candidate pool, so a selective filter can hide real matches. Raise
              <code>candidate_limit</code> (currently ${options.state.candidateLimit}) for highly
              selective filters.`,
            )
          : ''}

        <div class="composer-actions">
          <p class="t-caption">
            Query is embedded server-side; the bearer token stays on this server.
          </p>
          <button class="btn" data-variant="primary" type="submit">Run search</button>
        </div>
      </form>

      ${response === undefined ? '' : provenanceStrip(response, options.shell.head)}

      <div aria-live="polite" aria-atomic="true" class="stack">
        ${options.error !== undefined
          ? errorState(options.error.presentation, {
              message: options.error.message,
              ...(options.error.requestId === undefined
                ? {}
                : { requestId: options.error.requestId }),
              ...(options.error.retryHref === undefined
                ? {}
                : { retryHref: options.error.retryHref }),
            })
          : response === undefined
            ? emptyState({
                title: 'No search run yet.',
                body: 'Enter a query above. Nothing is sent to the CAIRN server until you run it, so no embedding credits are spent by opening this page.',
              })
            : response.hits.length === 0
              ? zeroHits(options, action)
              : html`<p class="visually-hidden">${resultSummary(response)}</p>
                  <div class="row">
                    <span class="chip"
                      >${formatCount(response.hits.length)} results</span
                    >
                    ${modeChip(response.mode)} ${approximateChip(response.approximate)}
                    ${digestChip(response.corpusSha256)}
                    ${options.latencyMs === undefined
                      ? ''
                      : html`<span class="chip chip-mono"
                          >${formatCount(options.latencyMs)}ms</span
                        >`}
                    ${options.requestId === undefined
                      ? ''
                      : html`<span class="chip chip-mono">req ${options.requestId}</span>`}
                  </div>
                  ${resultList(response, {
                    tenant: options.tenant,
                    knowledgeBase: options.knowledgeBase,
                    maxTextChars: options.maxTextChars,
                  })}`}
      </div>
    </section>`,
  )
}

function zeroHits(
  options: {
    readonly state: WorkbenchState
    readonly response?: CairnSearchResponse
  },
  action: string,
): SafeHtml {
  const revision = options.response?.revision ?? 0
  const filtersActive = options.state.filterCount > 0
  const retryWithoutFilters = `${action}?q=${encodeURIComponent(options.state.query)}&limit=${options.state.limit}&candidate_limit=${options.state.candidateLimit}&filters=%7B%7D`
  return emptyState({
    title: `No chunks matched in revision ${revision}.`,
    body: filtersActive
      ? 'Filters are applied after a bounded candidate pool, so selective filters can hide matches. Retry without filters, or raise candidate_limit.'
      : 'Widen candidate_limit, or try lexical phrasing closer to the corpus wording.',
    ...(filtersActive
      ? {
          actions: [
            html`<a class="btn" data-variant="primary" href="${retryWithoutFilters}"
              >Retry without filters</a
            >`,
          ],
        }
      : {}),
  })
}

/* ---------- /sessions ---------- */

/**
 * Two different things are called a "session" in this product, and conflating
 * them is the mistake this page exists to prevent:
 *
 *   - **Query history** is server-owned. It is this console's own record of
 *     searches it ran. CAIRN stores no query log, so nothing else has it.
 *   - **CAIRN session memory** is corpus. It is prior coding-agent sessions
 *     imported by `scripts/session_import.py`, ingested as ordinary revisioned
 *     chunks, and retrieved like any other evidence.
 *
 * The first is mutable and local. The second is immutable, revisioned, and
 * untrusted. They are rendered as separate sections with separate provenance
 * language, never interleaved in one list.
 */
export function sessionsPage(options: {
  readonly shell: ShellOptions
  readonly sessions: readonly SessionSummary[]
  readonly records: readonly SearchRecord[]
  readonly sessionsScope?: Scope | undefined
  readonly sessionUid?: string
}): SafeHtml {
  const scope = options.sessionsScope
  const imported = options.sessions.filter(session => session.imported)
  const owned = options.sessions.filter(session => !session.imported)
  const searchHref =
    scope === undefined
      ? undefined
      : `/kb/${encodeURIComponent(scope.tenant)}/${encodeURIComponent(scope.knowledgeBase)}/search`

  return layout(
    options.shell,
    html`<section class="section">
      <div class="section-head">
        <h1 class="t-h1">Sessions</h1>
        <p class="t-small t-tertiary">
          Two distinct records: this console's own query history, and imported CAIRN session
          memory held as revisioned corpus.
        </p>
      </div>

      <section class="stack" aria-labelledby="imported-memory">
        <div class="section-head">
          <h2 class="t-h2" id="imported-memory">CAIRN session memory</h2>
          <p class="t-small t-tertiary">
            ${scope === undefined
              ? 'No sessions knowledge base configured.'
              : html`<span class="chip chip-mono"
                  >${scope.tenant} / ${scope.knowledgeBase}</span
                >`}
          </p>
        </div>

        ${scope === undefined
          ? emptyState({
              title: 'No sessions knowledge base is configured.',
              body: 'Import local agent sessions, ingest them as a CAIRN knowledge base, then point this console at that scope with CAIRN_WEB_SESSIONS_SCOPE=tenant/kb.',
              command: 'python3 scripts/session_import.py export --out .cairn/session-chunks.jsonl',
            })
          : html`${notice(
              'info',
              'i',
              html`Imported sessions are <strong>corpus, not history</strong>: immutable,
              revisioned, and retrieved as untrusted evidence. Look up an exact
              <code>session_uid</code> for a bounded continuation packet, or search the whole
              knowledge base from the workbench.`,
            )}
            ${packetLookupForm({
              sessionUid: options.sessionUid ?? '',
              configured: true,
            })}
            <div class="row">
              <a class="btn" href="${searchHref as string}">Search session memory</a>
              <a
                class="btn"
                data-variant="ghost"
                href="${`${searchHref as string}?q=${encodeURIComponent('doc_type session_meta')}&filters=${encodeURIComponent('{"doc_type":"session_meta"}')}`}"
                >Browse session headers</a
              >
            </div>`}
      </section>

      <section class="stack" aria-labelledby="owned-history">
        <div class="section-head">
          <h2 class="t-h2" id="owned-history">Query history</h2>
          <p class="t-small t-tertiary">
            Server-owned. CAIRN stores no query log, so this record lives with the console.
          </p>
        </div>

        ${owned.length === 0
          ? emptyState({
              title: 'No searches recorded yet.',
              body: 'Run a search from the workbench. Failed searches are recorded too.',
            })
          : sessionSummaryList(owned, options.shell.now)}

        ${imported.length === 0
          ? ''
          : html`<h3 class="t-h3">Imported history records</h3>
              <p class="t-small t-tertiary measure">
                Query records handed to this console through
                <code>POST /api/history/import</code>. These are other runs' search logs, not
                corpus.
              </p>
              ${sessionSummaryList(imported, options.shell.now)}`}

        ${options.records.length === 0
          ? ''
          : html`<h3 class="t-h3">Searches</h3>
              ${sessionTimeline(options.records, options.shell.now)}`}
      </section>
    </section>`,
  )
}

/* ---------- /sessions/packet ---------- */

export function sessionPacketPage(options: {
  readonly shell: ShellOptions
  readonly scope?: Scope | undefined
  readonly sessionUid: string
  readonly packet?: SessionPacket
  readonly error?: {
    readonly presentation: ErrorPresentation
    readonly message: string
    readonly requestId?: string
    readonly retryHref?: string
  }
}): SafeHtml {
  const scope = options.scope
  return layout(
    options.shell,
    html`<section class="section">
      <div class="section-head">
        <h1 class="t-h1">Continuation packet</h1>
        <p class="t-small t-tertiary">
          ${scope === undefined
            ? 'No sessions knowledge base configured.'
            : `${scope.tenant} / ${scope.knowledgeBase}`}
        </p>
      </div>

      ${scope === undefined
        ? emptyState({
            title: 'No sessions knowledge base is configured.',
            body: 'Set CAIRN_WEB_SESSIONS_SCOPE=tenant/kb to a scope that is also listed in CAIRN_WEB_SCOPES, then reload.',
          })
        : html`${packetLookupForm({ sessionUid: options.sessionUid, configured: true })}
            ${options.error !== undefined
              ? errorState(options.error.presentation, {
                  message: options.error.message,
                  ...(options.error.requestId === undefined
                    ? {}
                    : { requestId: options.error.requestId }),
                  ...(options.error.retryHref === undefined
                    ? {}
                    : { retryHref: options.error.retryHref }),
                })
              : options.packet === undefined
                ? emptyState({
                    title: 'No session loaded.',
                    body: 'Enter an exact session_uid above. Nothing is sent to the CAIRN server until you do, so opening this page spends no embedding credits.',
                  })
                : sessionPacketSection(options.packet)}`}
    </section>`,
  )
}

/* ---------- Blocking states ---------- */

export function blockingErrorPage(options: {
  readonly shell: ShellOptions
  readonly presentation: ErrorPresentation
  readonly message: string
  readonly requestId?: string
}): SafeHtml {
  return layout(
    options.shell,
    html`<section class="section">
      <div class="section-head"><h1 class="t-h1">CAIRN Web cannot continue</h1></div>
      ${errorState(options.presentation, {
        message: options.message,
        ...(options.requestId === undefined ? {} : { requestId: options.requestId }),
      })}
    </section>`,
  )
}

export function notFoundPage(shell: ShellOptions, path: string): SafeHtml {
  return layout(
    shell,
    html`<section class="section">
      <div class="section-head"><h1 class="t-h1">Not found</h1></div>
      ${emptyState({
        title: 'No such page.',
        body: `Nothing is routed at ${path}. Pick a scope from the rail, or open the connection view.`,
      })}
    </section>`,
  )
}
