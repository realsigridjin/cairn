/**
 * Strata components. Server-rendered; every visual value comes from a token
 * class in static/app.css. No inline colors, no inline sizes — the only
 * inline custom properties are data-driven ratios (--bar-width,
 * --stagger-index), which are values, not design decisions.
 */

import type { CairnHeadResponse, CairnSearchHit, CairnSearchResponse } from '../protocol.ts'
import { METADATA_KEYS, citation } from '../protocol.ts'
import type { SearchRecord, SessionSummary } from '../history.ts'
import type { ErrorPresentation } from './errors.ts'
import {
  digestAriaLabel,
  digestGlyph,
  digestPrefix,
  evidenceScale,
  evidenceWidths,
  formatBytes,
  formatCount,
  formatPosterior,
  formatScore,
  formatTimestamp,
  hitAriaLabel,
  hitLang,
  isoTimestamp,
  metadataDisplay,
  truncateText,
} from './format.ts'
import { html, join, raw, type SafeHtml } from './html.ts'

/* ---------- Digest glyph ---------- */

export function glyph(corpusSha256: string): SafeHtml {
  const cells = digestGlyph(corpusSha256)
  return html`<span class="glyph" aria-hidden="true"
    >${join(cells.map(on => html`<span class="glyph-cell" data-on="${on ? '1' : '0'}"></span>`))}</span
  >`
}

export function digestChip(corpusSha256: string): SafeHtml {
  return html`<span class="chip chip-mono" title="${corpusSha256}">
    ${glyph(corpusSha256)}
    <span aria-label="${digestAriaLabel(corpusSha256)}">${digestPrefix(corpusSha256)}…</span>
  </span>`
}

/* ---------- Provenance strip: chrome, never a detail view ---------- */

export function provenanceStrip(
  response: CairnSearchResponse,
  head: CairnHeadResponse | undefined,
): SafeHtml {
  const parent = head?.parentRevision
  return html`<div class="provenance" role="status" aria-label="Result provenance">
    <span class="chip" data-tone="head">
      <span aria-hidden="true">▲</span> rev
      <span class="num num-strong">${response.revision}</span>
    </span>
    ${parent === undefined
      ? html`<span class="chip">genesis revision</span>`
      : html`<span class="chip">parent <span class="num">${parent}</span></span>`}
    <span class="provenance-sep" aria-hidden="true">|</span>
    ${digestChip(response.corpusSha256)}
    <span class="provenance-sep" aria-hidden="true">|</span>
    ${modeChip(response.mode)} ${approximateChip(response.approximate)}
    <span class="provenance-sep" aria-hidden="true">|</span>
    <span class="chip chip-mono"
      >${response.embeddingProvider}/${response.embeddingModel}
      <span class="num">${response.dimension}d</span></span
    >
    <span class="spacer"></span>
    ${costMeter(response.remoteBytes, response.rangeReads)}
  </div>`
}

/** cold/warm carry distinct glyphs, not just hue. */
export function modeChip(mode: 'cold' | 'warm'): SafeHtml {
  return mode === 'cold'
    ? html`<span class="chip" data-tone="cold"><span aria-hidden="true">◇</span> cold</span>`
    : html`<span class="chip" data-tone="warm"><span aria-hidden="true">◆</span> warm</span>`
}

/** Approximation: striped edge AND the word AND an icon. */
export function approximateChip(approximate: boolean): SafeHtml {
  return approximate
    ? html`<span class="chip" data-tone="warn"
        ><span aria-hidden="true">≈</span> approximate</span
      >`
    : html`<span class="chip" data-tone="ok"><span aria-hidden="true">=</span> exact</span>`
}

export function costMeter(remoteBytes: number, rangeReads: number): SafeHtml {
  return html`<span class="cost">
    <span class="t-caption">cost</span>
    <span class="num" aria-label="${formatBytes(remoteBytes)} transferred"
      >${formatBytes(remoteBytes)}</span
    >
    <span class="provenance-sep" aria-hidden="true">·</span>
    <span class="num" aria-label="${formatCount(rangeReads)} range reads"
      >${formatCount(rangeReads)} reads</span
    >
  </span>`
}

/* ---------- Result card ---------- */

export interface ResultCardOptions {
  readonly tenant: string
  readonly knowledgeBase: string
  readonly revision: number
  readonly approximate: boolean
  readonly scale: number
  readonly index: number
  readonly maxTextChars: number
}

export function resultCard(hit: CairnSearchHit, options: ResultCardOptions): SafeHtml {
  const cite = citation(options.tenant, options.knowledgeBase, options.revision, hit.id)
  const widths = evidenceWidths(hit.lexicalEvidence, hit.vectorEvidence, options.scale)
  const body = truncateText(hit.text, options.maxTextChars)
  const lang = hitLang(hit.metadata)
  const headingId = `hit-${options.index}-title`
  const title = typeof hit.metadata.title === 'string' ? hit.metadata.title : hit.id

  const chips = METADATA_KEYS.filter(key => key !== 'title' && Object.hasOwn(hit.metadata, key)).map(
    key =>
      html`<span class="chip chip-mono"
        >${key}: ${metadataDisplay(hit.metadata[key])}</span
      >`,
  )

  return html`<article
    class="card"
    data-untrusted="true"
    data-approximate="${options.approximate ? 'true' : 'false'}"
    style="--stagger-index: ${Math.min(options.index, 9)}"
    aria-labelledby="${headingId}"
    tabindex="0"
  >
    <div class="card-head">
      <h3 class="card-title" id="${headingId}">${title}</h3>
      <div class="row row-tight">
        <span class="posterior" aria-hidden="true">${formatPosterior(hit.posterior)}</span>
        <span class="num t-tertiary" aria-hidden="true">${formatScore(hit.score)}</span>
        <span class="visually-hidden">${hitAriaLabel(hit)}</span>
      </div>
    </div>

    <div class="evidence">
      <span class="t-caption">lex</span>
      <span class="bar">
        <span
          class="bar-fill"
          data-kind="lexical"
          style="--bar-width: ${widths.lexical.toFixed(2)}%"
        ></span>
      </span>
      <span class="t-caption">vec</span>
      <span class="bar">
        <span
          class="bar-fill"
          data-kind="vector"
          style="--bar-width: ${widths.vector.toFixed(2)}%"
        ></span>
      </span>
    </div>

    <p class="evidence-text t-small"${lang === undefined ? raw('') : raw(` lang="${lang}"`)}>${body.text}</p>
    ${body.truncated
      ? html`<p class="t-caption">
          Truncated by trusted plugin limits (${formatCount(options.maxTextChars)} chars per hit).
        </p>`
      : ''}
    ${chips.length > 0 ? html`<div class="row row-tight">${join(chips)}</div>` : ''}

    <p class="citation">
      <span class="t-caption" aria-hidden="true">cite</span>
      <span>${cite}</span>
    </p>
  </article>`
}

export function resultList(
  response: CairnSearchResponse,
  options: { readonly tenant: string; readonly knowledgeBase: string; readonly maxTextChars: number },
): SafeHtml {
  const scale = evidenceScale(response.hits)
  return html`<div class="results">
    ${join(
      response.hits.map((hit, index) =>
        resultCard(hit, {
          tenant: options.tenant,
          knowledgeBase: options.knowledgeBase,
          revision: response.revision,
          approximate: response.approximate,
          scale,
          index,
          maxTextChars: options.maxTextChars,
        }),
      ),
    )}
  </div>`
}

/** The live-region summary carries provenance, not just a count. */
export function resultSummary(response: CairnSearchResponse): string {
  return `${response.hits.length} result${response.hits.length === 1 ? '' : 's'}, revision ${response.revision}, ${response.mode} mode, ${response.approximate ? 'approximate' : 'exact'}`
}

/* ---------- States ---------- */

export function emptyState(options: {
  readonly title: string
  readonly body: string
  readonly command?: string
  readonly actions?: readonly SafeHtml[]
}): SafeHtml {
  return html`<div class="state" data-tone="empty">
    <h3 class="t-h3">${options.title}</h3>
    <p class="t-body t-secondary measure">${options.body}</p>
    ${options.command === undefined ? '' : html`<pre class="code-block">${options.command}</pre>`}
    ${options.actions === undefined || options.actions.length === 0
      ? ''
      : html`<div class="row">${join(options.actions)}</div>`}
  </div>`
}

export function errorState(
  presentation: ErrorPresentation,
  detail: { readonly message: string; readonly requestId?: string; readonly retryHref?: string },
): SafeHtml {
  return html`<div class="state" data-tone="error" role="alert">
    <div class="row">
      <span class="notice-icon" aria-hidden="true">✕</span>
      <h3 class="t-h3">${presentation.headline}</h3>
    </div>
    <p class="t-body t-secondary measure">${presentation.remedy}</p>
    ${presentation.command === undefined
      ? ''
      : html`<pre class="code-block">${presentation.command}</pre>`}
    <p class="t-small t-secondary measure">Server said: ${detail.message}</p>
    <div class="row">
      <span class="chip chip-mono" data-tone="error">code ${presentation.code}</span>
      ${detail.requestId === undefined
        ? html`<span class="chip chip-mono">no request id</span>`
        : html`<span class="chip chip-mono">request ${detail.requestId}</span>`}
    </div>
    ${presentation.retryable && detail.retryHref !== undefined
      ? html`<div class="row">
          <a class="btn" data-variant="primary" href="${detail.retryHref}">Retry search</a>
        </div>`
      : ''}
  </div>`
}

export function notice(
  tone: 'info' | 'warn' | 'error' | 'ok',
  icon: string,
  body: SafeHtml | string,
): SafeHtml {
  return html`<div class="notice" data-tone="${tone}">
    <span class="notice-icon" aria-hidden="true">${icon}</span>
    <div class="t-small measure">${body}</div>
  </div>`
}

/* ---------- Scope rail ---------- */

export function scopeRail(
  scopes: readonly { readonly tenant: string; readonly knowledgeBase: string; readonly fenced: boolean }[],
  active: { readonly tenant: string; readonly knowledgeBase: string } | undefined,
): SafeHtml {
  if (scopes.length === 0) {
    return html`<p class="t-small t-tertiary">No scopes configured.</p>`
  }
  return html`<ul class="scope-list">
    ${join(
      scopes.map(scope => {
        const current =
          active !== undefined &&
          active.tenant === scope.tenant &&
          active.knowledgeBase === scope.knowledgeBase
        const href = `/kb/${encodeURIComponent(scope.tenant)}/${encodeURIComponent(scope.knowledgeBase)}`
        const label = `${scope.tenant} / ${scope.knowledgeBase}`
        return html`<li>
          ${scope.fenced
            ? html`<button
                class="scope-item"
                type="button"
                disabled
                aria-disabled="true"
                title="This CAIRN server was started with --restrict-to-default-scope and will answer SCOPE_FORBIDDEN for this scope."
              >
                <span class="scope-name">${label}</span>
                <span class="chip" data-tone="warn"
                  ><span aria-hidden="true">⊘</span> fenced</span
                >
              </button>`
            : html`<a
                class="scope-item"
                href="${href}"
                aria-current="${current ? 'true' : 'false'}"
              >
                <span class="scope-name">${label}</span>
                ${current ? html`<span class="chip" data-tone="head">active</span>` : ''}
              </a>`}
        </li>`
      }),
    )}
  </ul>`
}

/** Strata rail: HEAD anchored top, ancestors descending via parentRevision. */
export function strataRail(head: CairnHeadResponse | undefined, now: number): SafeHtml {
  if (head === undefined) {
    return html`<p class="t-small t-tertiary">No revision resolved.</p>`
  }
  const layers: SafeHtml[] = [
    html`<li class="strata-layer" data-state="head">
      <span class="num num-strong">rev ${head.revision}</span>
      <span class="chip" data-tone="head">HEAD</span>
      <time class="t-caption" datetime="${isoTimestamp(head.createdAtUnixMs)}"
        >${formatTimestamp(head.createdAtUnixMs, now)}</time
      >
    </li>`,
  ]
  if (head.parentRevision !== undefined) {
    layers.push(
      html`<li class="strata-layer" data-state="committed">
        <span class="num">rev ${head.parentRevision}</span>
        <span class="t-caption">parent</span>
      </li>`,
    )
  }
  return html`<ul class="strata">${join(layers)}</ul>`
}

/* ---------- Session history ---------- */

export function sessionTimeline(
  records: readonly SearchRecord[],
  now: number,
): SafeHtml {
  return html`<ul class="timeline">
    ${join(
      records.map(record => {
        const ok = record.outcome.status === 'ok'
        return html`<li class="timeline-item" data-outcome="${ok ? 'ok' : 'error'}">
          <time class="num t-tertiary" datetime="${isoTimestamp(record.createdAtUnixMs)}"
            >${formatTimestamp(record.createdAtUnixMs, now)}</time
          >
          <span class="t-small">${record.request.query}</span>
          <span class="row row-tight">
            ${ok
              ? html`${modeChip((record.outcome as { mode: 'cold' | 'warm' }).mode)}
                  <span class="chip chip-mono"
                    >rev ${(record.outcome as { revision: number }).revision}</span
                  >`
              : html`<span class="chip" data-tone="error"
                  ><span aria-hidden="true">✕</span>
                  ${(record.outcome as { code: string }).code}</span
                >`}
            <span class="num t-tertiary">${formatCount(record.latencyMs)}ms</span>
          </span>
        </li>`
      }),
    )}
  </ul>`
}

export function sessionSummaryList(sessions: readonly SessionSummary[], now: number): SafeHtml {
  return html`<ul class="timeline">
    ${join(
      sessions.map(
        session => html`<li
          class="timeline-item"
          data-outcome="${session.errorCount > 0 ? 'error' : 'ok'}"
        >
          <time class="num t-tertiary" datetime="${isoTimestamp(session.updatedAtUnixMs)}"
            >${formatTimestamp(session.updatedAtUnixMs, now)}</time
          >
          <span class="t-small chip-mono">${session.sessionId}</span>
          <span class="row row-tight">
            <span class="chip">${session.searchCount} searches</span>
            ${session.errorCount > 0
              ? html`<span class="chip" data-tone="error">${session.errorCount} failed</span>`
              : ''}
            ${session.imported ? html`<span class="chip" data-tone="warn">imported</span>` : ''}
          </span>
        </li>`,
      ),
    )}
  </ul>`
}
