/**
 * Durable backing for server-owned query history.
 *
 * CAIRN stores no query log, so this console's history is the only record that
 * a search ever happened. Keeping it purely in memory means a restart silently
 * destroys it, which is exactly the moment a human most wants to know what the
 * previous run did. `CAIRN_WEB_HISTORY_PATH` makes it survive.
 *
 * Format: **JSONL, one record per line**. The append path is the hot path —
 * every search writes one record — and appending a line is a single `write(2)`
 * at the end of the file. A JSON array would require re-serialising the whole
 * history on every search, turning an O(1) write into O(n).
 *
 * Durability rules, in order of how much they matter:
 *
 *   1. **A corrupt line never blocks startup.** A crash mid-append leaves a
 *      truncated final line. Loading skips unparseable and structurally invalid
 *      lines and reports how many it dropped, because losing one record is
 *      strictly better than refusing to boot.
 *   2. **Rewrites are atomic.** Compaction writes a sibling temp file, fsyncs
 *      it, then `rename()`s over the target. `rename` within a directory is
 *      atomic, so a crash mid-compaction leaves either the old file or the new
 *      one — never a half-written history.
 *   3. **Retention is bounded on disk, not just in memory.** The file is
 *      compacted when it exceeds the retention limit, so it cannot grow without
 *      bound on a long-running server.
 *
 * The token is never part of a `SearchRecord` and therefore never reaches this
 * file; `test/history-durable.test.ts` asserts that against the real on-disk
 * bytes rather than trusting the type.
 */

import { closeSync, fsyncSync, openSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { appendFileSync, existsSync, mkdirSync, unlinkSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import type { SearchRecord } from './history.ts'

export interface HistoryPersistence {
  /** Records recovered at boot, oldest first. */
  load(): readonly SearchRecord[]
  /** Append one record. Must be cheap: this runs on every search. */
  append(record: SearchRecord): void
  /** Replace the entire file atomically (eviction, import reordering). */
  rewrite(records: readonly SearchRecord[]): void
}

export interface LoadReport {
  readonly recovered: number
  readonly skipped: number
  readonly repairFailed: boolean
}

/**
 * A record is only accepted from disk if it still satisfies the shape the rest
 * of the app relies on. A file is an untrusted input: it can be hand-edited,
 * truncated, or written by an older version.
 */
export function decodeStoredRecord(value: unknown): SearchRecord | undefined {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return undefined
  const root = value as Record<string, unknown>
  if (
    !isNonEmptyString(root.queryId) ||
    !isNonEmptyString(root.sessionId) ||
    !isNonEmptyString(root.tenant) ||
    !isNonEmptyString(root.knowledgeBase) ||
    !isNonEmptyString(root.requestId) ||
    (root.imported !== undefined && typeof root.imported !== 'boolean') ||
    !isSafeInteger(root.createdAtUnixMs) ||
    !isSafeInteger(root.latencyMs)
  ) {
    return undefined
  }
  const request = root.request
  if (request === null || typeof request !== 'object' || Array.isArray(request)) return undefined
  const requestRecord = request as Record<string, unknown>
  if (
    typeof requestRecord.query !== 'string' ||
    !isSafeInteger(requestRecord.limit) ||
    !isSafeInteger(requestRecord.candidateLimit)
  ) {
    return undefined
  }

  const outcome = root.outcome
  if (outcome === null || typeof outcome !== 'object' || Array.isArray(outcome)) return undefined
  const outcomeRecord = outcome as Record<string, unknown>
  if (outcomeRecord.status === 'ok') {
    if (
      !isSafeInteger(outcomeRecord.revision) ||
      !isNonEmptyString(outcomeRecord.corpusSha256) ||
      (outcomeRecord.mode !== 'cold' && outcomeRecord.mode !== 'warm') ||
      typeof outcomeRecord.approximate !== 'boolean' ||
      !Array.isArray(outcomeRecord.hits)
    ) {
      return undefined
    }
  } else if (outcomeRecord.status === 'error') {
    if (!isNonEmptyString(outcomeRecord.code) || typeof outcomeRecord.message !== 'string') {
      return undefined
    }
  } else {
    return undefined
  }

  return value as SearchRecord
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && value.length <= 4096
}

function isSafeInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

/**
 * Parse a JSONL history file.
 *
 * Exported and pure so the recovery rules are testable without touching a real
 * filesystem. Blank lines are ignored; every other unusable line is counted.
 */
export function parseHistoryFile(contents: string): {
  readonly records: readonly SearchRecord[]
  readonly skipped: number
} {
  const records: SearchRecord[] = []
  let skipped = 0
  for (const line of contents.split('\n')) {
    const trimmed = line.trim()
    if (trimmed.length === 0) continue
    let parsed: unknown
    try {
      parsed = JSON.parse(trimmed)
    } catch {
      // A truncated tail from a crash lands here; it must not be fatal.
      skipped += 1
      continue
    }
    const record = decodeStoredRecord(parsed)
    if (record === undefined) {
      skipped += 1
      continue
    }
    records.push(record)
  }
  return { records, skipped }
}

/** JSONL file persistence with atomic rewrites. */
export class FileHistoryPersistence implements HistoryPersistence {
  readonly #path: string
  #lastReport: LoadReport = { recovered: 0, skipped: 0, repairFailed: false }

  constructor(path: string) {
    this.#path = resolve(path)
    mkdirSync(dirname(this.#path), { recursive: true })
  }

  get path(): string {
    return this.#path
  }

  /** Diagnostics from the most recent `load()`, for the boot banner. */
  get lastReport(): LoadReport {
    return this.#lastReport
  }

  load(): readonly SearchRecord[] {
    if (!existsSync(this.#path)) {
      this.#lastReport = { recovered: 0, skipped: 0, repairFailed: false }
      return []
    }
    const contents = readFileSync(this.#path, 'utf8')
    const { records, skipped } = parseHistoryFile(contents)
    this.#lastReport = { recovered: records.length, skipped, repairFailed: false }
    // A file containing damage is rewritten clean, so the damage is repaired
    // once at boot instead of being re-parsed on every subsequent start. The
    // repair is best-effort: a read-only or full filesystem must not turn one
    // corrupt line into a boot failure.
    if (skipped > 0) {
      try {
        this.rewrite(records)
      } catch (error) {
        if (!(error instanceof Error)) throw error
        this.#lastReport = { recovered: records.length, skipped, repairFailed: true }
      }
    }
    return records
  }

  append(record: SearchRecord): void {
    appendFileSync(this.#path, `${JSON.stringify(record)}\n`, 'utf8')
  }

  /**
   * Atomic replace: write a sibling temp file, fsync it, rename over the
   * target. `rename(2)` within one directory is atomic, so a crash at any
   * point leaves either the previous file or the complete new one.
   */
  rewrite(records: readonly SearchRecord[]): void {
    const temp = `${this.#path}.tmp-${process.pid}`
    const body = records.map(record => `${JSON.stringify(record)}\n`).join('')
    try {
      writeFileSync(temp, body, 'utf8')
      // Flush the temp file's contents before publishing it, otherwise the
      // rename can be durable while the data behind it is not.
      const handle = openSync(temp, 'r')
      try {
        fsyncSync(handle)
      } finally {
        closeSync(handle)
      }
      renameSync(temp, this.#path)
    } catch (error) {
      // Never leave a stray temp file behind on failure.
      try {
        if (existsSync(temp)) unlinkSync(temp)
      } catch {
        /* the original error is the one worth reporting */
      }
      throw error
    }
  }
}
