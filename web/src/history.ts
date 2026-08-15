/**
 * Server-owned session history.
 *
 * CAIRN stores no query log — `src/server.rs` has no history route — so
 * retrieval history is the web app's own durable concern, keyed by
 * (tenant, knowledgeBase, revision, corpusSha256, queryId).
 *
 * This in-memory store is the 1.0 placeholder for that table. It is
 * deliberately shaped like the eventual SQLite schema (append + bounded
 * retention + cursor paging + import) so swapping the backing store is a
 * constructor change, not a rewrite. `importSessions` is the seam that
 * accepts sessions produced by another agent run.
 *
 * Failed searches are first-class records: an agent that hit
 * EMBEDDING_UNAVAILABLE mid-session is exactly what a human needs to see.
 */

import type { JsonValue } from './protocol.ts'

export interface SearchRecordRequest {
  readonly query: string
  readonly limit: number
  readonly candidateLimit: number
  readonly filters: Readonly<Record<string, JsonValue>>
}

export interface SearchRecordOutcomeOk {
  readonly status: 'ok'
  readonly revision: number
  readonly corpusSha256: string
  readonly mode: 'cold' | 'warm'
  readonly approximate: boolean
  readonly remoteBytes: number
  readonly rangeReads: number
  readonly hits: readonly { readonly id: string; readonly score: number }[]
}

export interface SearchRecordOutcomeError {
  readonly status: 'error'
  readonly code: string
  readonly message: string
  readonly retryable: boolean
}

export type SearchRecordOutcome = SearchRecordOutcomeOk | SearchRecordOutcomeError

export interface SearchRecord {
  readonly queryId: string
  readonly sessionId: string
  readonly tenant: string
  readonly knowledgeBase: string
  readonly requestId: string
  readonly toolCallId?: string
  readonly createdAtUnixMs: number
  readonly latencyMs: number
  readonly request: SearchRecordRequest
  readonly outcome: SearchRecordOutcome
}

export interface HistoryPage {
  readonly records: readonly SearchRecord[]
  readonly nextCursor: string | undefined
  readonly total: number
}

export interface HistoryQuery {
  readonly tenant?: string
  readonly knowledgeBase?: string
  readonly sessionId?: string
  readonly cursor?: string
  readonly pageSize?: number
}

export interface SessionSummary {
  readonly sessionId: string
  readonly tenant: string
  readonly knowledgeBase: string
  readonly startedAtUnixMs: number
  readonly updatedAtUnixMs: number
  readonly searchCount: number
  readonly errorCount: number
  readonly revisions: readonly number[]
  readonly corpusDigests: readonly string[]
  readonly imported: boolean
}

const DEFAULT_PAGE_SIZE = 25

export class HistoryStore {
  readonly #records: SearchRecord[] = []
  readonly #imported = new Set<string>()
  readonly #limit: number

  constructor(limit: number) {
    if (!Number.isSafeInteger(limit) || limit < 1) throw new Error('history limit must be >= 1')
    this.#limit = limit
  }

  append(record: SearchRecord): SearchRecord {
    this.#records.push(record)
    // Bounded retention: drop oldest first, mirroring a rolling table.
    while (this.#records.length > this.#limit) this.#records.shift()
    return record
  }

  /**
   * Placeholder seam for imported sessions (handoff packets, agent logs).
   * Deduplicates on queryId so re-importing is idempotent, and marks the
   * session so the UI can label provenance.
   */
  importSessions(records: readonly SearchRecord[]): { readonly imported: number; readonly skipped: number } {
    let imported = 0
    let skipped = 0
    const known = new Set(this.#records.map(record => record.queryId))
    for (const record of records) {
      if (known.has(record.queryId)) {
        skipped += 1
        continue
      }
      known.add(record.queryId)
      this.#imported.add(record.sessionId)
      this.append(record)
      imported += 1
    }
    // Keep chronological order after an out-of-band import.
    this.#records.sort((left, right) => left.createdAtUnixMs - right.createdAtUnixMs)
    return { imported, skipped }
  }

  get(queryId: string): SearchRecord | undefined {
    return this.#records.find(record => record.queryId === queryId)
  }

  /** Newest-first page. Cursor is an opaque queryId of the last seen record. */
  list(query: HistoryQuery = {}): HistoryPage {
    const pageSize = clampPageSize(query.pageSize)
    const filtered = this.#records
      .filter(record => query.tenant === undefined || record.tenant === query.tenant)
      .filter(
        record =>
          query.knowledgeBase === undefined || record.knowledgeBase === query.knowledgeBase,
      )
      .filter(record => query.sessionId === undefined || record.sessionId === query.sessionId)
      .slice()
      .reverse()

    let start = 0
    if (query.cursor !== undefined) {
      const index = filtered.findIndex(record => record.queryId === query.cursor)
      start = index < 0 ? filtered.length : index + 1
    }
    const page = filtered.slice(start, start + pageSize)
    const next = filtered.length > start + pageSize ? page.at(-1)?.queryId : undefined
    return {
      records: page,
      nextCursor: next,
      total: filtered.length,
    }
  }

  sessions(query: Pick<HistoryQuery, 'tenant' | 'knowledgeBase'> = {}): readonly SessionSummary[] {
    const grouped = new Map<string, SearchRecord[]>()
    for (const record of this.#records) {
      if (query.tenant !== undefined && record.tenant !== query.tenant) continue
      if (query.knowledgeBase !== undefined && record.knowledgeBase !== query.knowledgeBase) {
        continue
      }
      const bucket = grouped.get(record.sessionId)
      if (bucket === undefined) grouped.set(record.sessionId, [record])
      else bucket.push(record)
    }
    const summaries = [...grouped.entries()].map(([sessionId, records]) => {
      const first = records[0] as SearchRecord
      const revisions = new Set<number>()
      const digests = new Set<string>()
      let errorCount = 0
      let startedAt = first.createdAtUnixMs
      let updatedAt = first.createdAtUnixMs
      for (const record of records) {
        startedAt = Math.min(startedAt, record.createdAtUnixMs)
        updatedAt = Math.max(updatedAt, record.createdAtUnixMs)
        if (record.outcome.status === 'error') errorCount += 1
        else {
          revisions.add(record.outcome.revision)
          digests.add(record.outcome.corpusSha256)
        }
      }
      return {
        sessionId,
        tenant: first.tenant,
        knowledgeBase: first.knowledgeBase,
        startedAtUnixMs: startedAt,
        updatedAtUnixMs: updatedAt,
        searchCount: records.length,
        errorCount,
        revisions: [...revisions].sort((left, right) => right - left),
        corpusDigests: [...digests],
        imported: this.#imported.has(sessionId),
      }
    })
    return summaries.sort((left, right) => right.updatedAtUnixMs - left.updatedAtUnixMs)
  }

  get size(): number {
    return this.#records.length
  }
}

function clampPageSize(value: number | undefined): number {
  if (value === undefined || !Number.isSafeInteger(value) || value < 1) return DEFAULT_PAGE_SIZE
  return Math.min(value, 100)
}
