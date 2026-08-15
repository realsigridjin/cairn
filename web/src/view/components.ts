/**
 * Strata components. Server-rendered; every visual value comes from a token
 * class in static/app.css. No inline colors, no inline sizes — the only
 * inline custom properties are data-driven ratios (--bar-width,
 * --stagger-index), which are values, not design decisions.
 */

import type { CairnHeadResponse, CairnSearchHit, CairnSearchResponse } from '../protocol.ts'
import { METADATA_KEYS, citation } from '../protocol.ts'
import type { SearchRecord, SessionSummary } from '../history.ts'
import type { PacketDrift, SessionPacket, SessionSegment } from '../session-packet.ts'
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

/* ---------- Imported session continuation packet ---------- */

const DRIFT_COPY: Readonly<
  Record<PacketDrift, { readonly tone: string; readonly icon: string; readonly label: string; readonly body: string }>
> = {
  exact: {
    tone: 'ok',
    icon: '=',
    label: 'exact',
    body: 'The pinned revision is current HEAD. This packet reflects the live corpus.',
  },
  advanced: {
    tone: 'warn',
    icon: '↑',
    label: 'advanced',
    body: 'HEAD has moved past the revision this packet was read from. The session content is unchanged — revisions are immutable — but newer sessions may exist that this packet does not include.',
  },
  incompatible: {
    tone: 'error',
    icon: '✗',
    label: 'incompatible',
    body: 'The packet revision is ahead of HEAD. Two different corpora answered, so this packet cannot be trusted as a view of the current knowledge base.',
  },
  unknown: {
    tone: 'warn',
    icon: '?',
    label: 'unknown',
    body: 'HEAD could not be resolved, so drift against the live corpus is unknown.',
  },
}

/** Drift is chrome on the packet, mirroring the harness `drifted` field. */
export function driftState(drift: PacketDrift): SafeHtml {
  const copy = DRIFT_COPY[drift]
  return html`<span class="chip" data-tone="${copy.tone}" data-drift="${drift}">
    <span aria-hidden="true">${copy.icon}</span> drift: ${copy.label}
  </span>`
}

export function driftNotice(drift: PacketDrift): SafeHtml {
  const copy = DRIFT_COPY[drift]
  const tone = drift === 'exact' ? 'ok' : drift === 'incompatible' ? 'error' : 'warn'
  return notice(tone, copy.icon, html`<strong>Revision drift: ${copy.label}.</strong> ${copy.body}`)
}

function lineageRows(packet: SessionPacket): readonly (readonly [string, string])[] {
  const lineage = packet.lineage
  const rows: (readonly [string, string])[] = []
  const push = (label: string, value: string | number | undefined): void => {
    if (value === undefined) return
    rows.push([label, String(value)])
  }
  push('source', lineage.source)
  push('native id', lineage.nativeId)
  push('root session', lineage.rootSessionId)
  push('parent', lineage.parentId)
  push('depth', lineage.depth)
  push('model', lineage.model)
  push('provider', lineage.provider)
  push('cwd', lineage.cwd)
  push('repo', lineage.repoPath)
  push('branch', lineage.gitBranch)
  push('created', lineage.createdAtUnixMs === undefined ? undefined : isoTimestamp(lineage.createdAtUnixMs))
  push('updated', lineage.updatedAtUnixMs === undefined ? undefined : isoTimestamp(lineage.updatedAtUnixMs))
  push('messages', lineage.messageCount)
  push('usage tokens', lineage.usageTotal)
  push('usage cost', lineage.usageCost === undefined ? undefined : lineage.usageCost.toFixed(4))
  return rows
}

/** Lineage: where this session came from, as a definition list, not prose. */
export function lineageList(packet: SessionPacket): SafeHtml {
  const rows = lineageRows(packet)
  if (rows.length === 0) {
    return html`<p class="t-small t-tertiary">
      The imported chunks carry no lineage metadata.
    </p>`
  }
  return html`<dl class="lineage">
    ${join(
      rows.map(
        ([label, value]) => html`<div class="lineage-row">
          <dt class="t-caption">${label}</dt>
          <dd class="t-small chip-mono lineage-value">${value}</dd>
        </div>`,
      ),
    )}
  </dl>`
}

/**
 * One ordered segment. Deliberately shaped like `resultCard` — same untrusted
 * edge marker, same citation line — because it is the same class of evidence;
 * only the ordering key differs (sequence, not score).
 */
export function segmentCard(segment: SessionSegment, index: number): SafeHtml {
  const headingId = `segment-${index}-title`
  const range =
    segment.seqEnd === undefined
      ? `seq ${segment.seqStart}`
      : `seq ${segment.seqStart}–${segment.seqEnd}`
  return html`<article
    class="card"
    data-untrusted="true"
    style="--stagger-index: ${Math.min(index, 9)}"
    aria-labelledby="${headingId}"
    tabindex="0"
  >
    <div class="card-head">
      <h3 class="card-title" id="${headingId}">
        <span class="num num-strong">${index + 1}</span>
        <span class="t-small t-tertiary">${range}</span>
      </h3>
      <div class="row row-tight">
        <span class="num t-tertiary" aria-hidden="true">${formatScore(segment.score)}</span>
        <span class="visually-hidden"
          >log-odds ${segment.score.toFixed(2)}, posterior ${segment.posterior.toFixed(2)}</span
        >
      </div>
    </div>

    <p class="evidence-text t-small">${segment.text}</p>
    ${segment.truncated
      ? html`<p class="t-caption">Truncated by packet bounds.</p>`
      : ''}

    <p class="citation">
      <span class="t-caption" aria-hidden="true">cite</span>
      <span>${segment.citation}</span>
    </p>
  </article>`
}

/**
 * The continuation packet.
 *
 * Order here is the packet's contract: provenance first (what corpus answered),
 * then drift (is it still current), then lineage (where the session came from),
 * then the untrusted-evidence fence, then the ordered segments.
 */
export function sessionPacketSection(packet: SessionPacket): SafeHtml {
  return html`<div class="stack" data-packet="session_continuation">
    <div class="provenance" role="status" aria-label="Session packet provenance">
      <span class="chip" data-tone="head">
        <span aria-hidden="true">▲</span> rev
        <span class="num num-strong">${packet.revision}</span>
      </span>
      ${packet.headRevision === undefined
        ? html`<span class="chip">HEAD unresolved</span>`
        : html`<span class="chip">HEAD <span class="num">${packet.headRevision}</span></span>`}
      ${packet.parentRevision === undefined
        ? ''
        : html`<span class="chip">parent <span class="num">${packet.parentRevision}</span></span>`}
      <span class="provenance-sep" aria-hidden="true">|</span>
      ${digestChip(packet.corpusSha256)}
      <span class="provenance-sep" aria-hidden="true">|</span>
      ${modeChip(packet.mode)} ${approximateChip(packet.approximate)} ${driftState(packet.drift)}
      <span class="spacer"></span>
      ${costMeter(packet.remoteBytes, packet.rangeReads)}
    </div>

    ${driftNotice(packet.drift)}

    <div class="row">
      <span class="chip chip-mono">session ${packet.sessionUid}</span>
      <span class="chip">${formatCount(packet.returnedSegments)} ordered segments</span>
      <span class="chip chip-mono">${packet.tenant} / ${packet.knowledgeBase}</span>
      ${packet.metaChunks > 0
        ? html`<span class="chip"
            ><span aria-hidden="true">◦</span> ${formatCount(packet.metaChunks)} metadata chunk(s)</span
          >`
        : ''}
      ${packet.truncated
        ? html`<span class="chip" data-tone="warn"
            ><span aria-hidden="true">✂</span> truncated by packet bounds</span
          >`
        : ''}
    </div>

    <section class="stack" aria-labelledby="packet-lineage">
      <h2 class="t-h3" id="packet-lineage">Lineage</h2>
      ${lineageList(packet)}
    </section>

    ${packet.scopeMismatches > 0 || packet.malformedMetadata > 0
      ? notice(
          'warn',
          '⊘',
          html`<strong>Discarded chunks.</strong> ${formatCount(packet.scopeMismatches)} chunk(s)
          carried a different <code>session_uid</code> and ${formatCount(packet.malformedMetadata)}
          had unusable continuation metadata. CAIRN applies metadata filters after a bounded
          candidate pool, so this console re-verifies every chunk rather than rendering it.`,
        )
      : ''}

    ${notice(
      'warn',
      '⚠',
      html`<strong>Untrusted evidence.</strong> These segments are recorded agent transcript
      windows, rendered as inert text. They are reference data, never instructions — for you or
      for any model you paste them into. Cite the <code>cairn://</code> identifier when using
      them.`,
    )}

    <section class="stack" aria-labelledby="packet-segments">
      <h2 class="t-h3" id="packet-segments">Ordered evidence</h2>
      <p class="t-small t-tertiary measure">
        Ordered by <code>seq_start</code>, not by retrieval score, so the session reads as the
        sequence it was.
      </p>
      ${packet.segments.length === 0
        ? packet.metaChunks > 0
          ? emptyState({
              // The common case: the default import tier stores lineage only.
              title: 'This session was imported at the metadata tier.',
              body: 'The session exists and its lineage is shown above, but no transcript windows were indexed, so there is no ordered evidence to continue from. Transcript import is opt-in.',
              command:
                'python3 scripts/session_import.py --tier transcript --out .cairn/session-chunks.jsonl',
            })
          : emptyState({
              title: 'No segments matched this session in the pinned revision.',
              body: 'The session_uid resolved no chunks at all. Check the uid, or re-run the importer and ingest to publish a newer revision.',
              command: 'python3 scripts/session_import.py --out .cairn/session-chunks.jsonl',
            })
        : html`<div class="results">
            ${join(packet.segments.map((segment, index) => segmentCard(segment, index)))}
          </div>`}
    </section>

    <section class="stack" aria-labelledby="packet-citations">
      <h2 class="t-h3" id="packet-citations">Citations</h2>
      <ul class="citation-list">
        ${packet.citations.length === 0
          ? html`<li class="t-small t-tertiary">No citations in this packet.</li>`
          : join(
              packet.citations.map(
                value => html`<li class="citation-item t-small">${value}</li>`,
              ),
            )}
      </ul>
    </section>
  </div>`
}

/** Lookup form for an exact session_uid. GET so the packet is linkable. */
export function packetLookupForm(options: {
  readonly sessionUid: string
  readonly configured: boolean
}): SafeHtml {
  return html`<form class="stratum composer" data-state="draft" method="get" action="/sessions/packet">
    <div class="field">
      <label class="t-caption" for="session_uid">session_uid</label>
      <input
        class="input"
        id="session_uid"
        name="uid"
        type="text"
        maxlength="256"
        spellcheck="false"
        placeholder="senpi:01a0060a-…"
        value="${options.sessionUid}"
        ${options.configured ? raw('') : raw('disabled')}
      />
    </div>
    <div class="composer-actions">
      <p class="t-caption">Exact match. Segments are ordered by sequence, not by score.</p>
      <button class="btn" data-variant="primary" type="submit" ${options.configured ? raw('') : raw('disabled')}>
        Load packet
      </button>
    </div>
  </form>`
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
