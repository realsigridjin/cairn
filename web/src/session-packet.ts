/**
 * Imported-session continuation packets.
 *
 * CAIRN stores imported coding-agent sessions as ordinary revisioned chunks in
 * a dedicated knowledge base (see SESSION_CONTINUITY.md). Retrieval returns
 * them by *score*, which is the wrong order for continuation: a session is a
 * sequence. This module re-derives that sequence.
 *
 * The console and the harness tool share the load-bearing contract: ordering is
 * `seq_start`, never retrieval score; foreign or malformed chunks are dropped
 * and counted; and retrieved text remains untrusted evidence. Each surface owns
 * its presentation bounds, so segment and character caps may differ while the
 * evidence order and rejection semantics stay identical.
 *
 * Three properties are load-bearing and are pinned by `test/session-packet.test.ts`:
 *
 *   1. **Order is `seq_start`, never score.** Ties break on chunk id so the
 *      packet is a pure function of the corpus, not of retrieval jitter.
 *   2. **Server-side filters are post-retrieval over a bounded, approximate
 *      candidate pool.** A hit carrying a different `session_uid` is therefore
 *      possible and is *dropped and counted*, never rendered. Same for chunks
 *      whose continuation metadata is malformed.
 *   3. **Bounds are enforced here, not by the renderer.** Segment count and
 *      total text are capped so one pathological session cannot produce an
 *      unbounded page.
 *
 * Retrieved session text stays untrusted evidence. This module never
 * interprets it; it only measures and truncates it.
 */

import type { CairnHeadResponse, CairnSearchHit, CairnSearchResponse, JsonValue } from './protocol.ts'
import { citation } from './protocol.ts'

/** Harness-compatible shape, stricter on Unicode control characters: 1..=256 chars, no Cc. */
const SESSION_UID = /^[^\p{Cc}]{1,256}$/u

/**
 * Metadata keys the packet surfaces, mirroring the sessions-mount allowlist in
 * `examples/deepseek-harness.sessions.patch.yml`. Everything else in the chunk
 * metadata is ignored rather than rendered: an imported session can carry
 * arbitrary keys and this view is not a metadata dump.
 */
export const SESSION_METADATA_KEYS: readonly string[] = [
  'doc_type',
  'source',
  'session_uid',
  'native_id',
  'root_session_id',
  'parent_id',
  'depth',
  'agent_role',
  'cwd',
  'repo_path',
  'git_branch',
  'provider',
  'model',
  'created_at_ms',
  'updated_at_ms',
  'message_count',
  'usage_total',
  'usage_cost',
  'seq_start',
  'seq_end',
]

export const PACKET_BOUNDS = {
  maxSegments: 24,
  segmentsDefault: 12,
  maxTextCharsPerSegment: 4_000,
  maxTotalTextChars: 16_000,
  /** Retrieval pool: wide enough to cover a long session, still bounded. */
  candidateLimit: 2_000,
} as const

export interface SessionSegmentMetadata {
  readonly sessionUid: string
  readonly seqStart: number
  readonly seqEnd?: number
}

export interface SessionSegment {
  readonly id: string
  readonly citation: string
  readonly seqStart: number
  readonly seqEnd?: number
  readonly score: number
  readonly posterior: number
  readonly text: string
  readonly truncated: boolean
  readonly metadata: Readonly<Record<string, JsonValue>>
}

/** Lineage as recorded by the importer, read off the `session_meta` chunk. */
export interface SessionLineage {
  readonly source?: string
  readonly nativeId?: string
  readonly rootSessionId?: string
  readonly parentId?: string
  readonly depth?: number
  readonly cwd?: string
  readonly repoPath?: string
  readonly gitBranch?: string
  readonly model?: string
  readonly provider?: string
  readonly createdAtUnixMs?: number
  readonly updatedAtUnixMs?: number
  readonly messageCount?: number
  readonly usageTotal?: number
  readonly usageCost?: number
}

/**
 * Drift between the revision the packet was read from and current HEAD.
 * `exact` and `advanced` are both usable; they differ in whether the console
 * may be showing a session view that no longer matches the live corpus.
 */
export type PacketDrift = 'exact' | 'advanced' | 'incompatible' | 'unknown'

export interface SessionPacket {
  readonly trust: 'untrusted_reference_data'
  readonly packet: 'session_continuation'
  readonly tenant: string
  readonly knowledgeBase: string
  readonly sessionUid: string
  readonly revision: number
  readonly corpusSha256: string
  readonly embeddingProvider: string
  readonly embeddingModel: string
  readonly dimension: number
  readonly mode: 'cold' | 'warm'
  readonly approximate: boolean
  readonly headRevision?: number
  readonly parentRevision?: number
  readonly drift: PacketDrift
  readonly lineage: SessionLineage
  readonly segments: readonly SessionSegment[]
  readonly citations: readonly string[]
  readonly returnedSegments: number
  readonly sourceHits: number
  readonly scopeMismatches: number
  readonly malformedMetadata: number
  /**
   * `session_meta` chunks matching this session. They carry lineage but no
   * sequence position, so they are not segments. Counted separately because a
   * metadata-tier import (the default) produces exactly these and nothing
   * else — reporting them as "malformed" would describe a correct corpus as
   * broken.
   */
  readonly metaChunks: number
  readonly truncated: boolean
  readonly remoteBytes: number
  readonly rangeReads: number
}

export function validateSessionUid(value: string | undefined | null): string | { readonly error: string } {
  if (typeof value !== 'string') return { error: 'session_uid must be a string' }
  const sessionUid = value.trim()
  if (!SESSION_UID.test(sessionUid)) {
    return { error: 'session_uid must contain 1..=256 characters and no control characters' }
  }
  return sessionUid
}

export function validateSegmentLimit(raw: string | undefined | null): number | { readonly error: string } {
  if (raw === undefined || raw === null || raw.trim().length === 0) return PACKET_BOUNDS.segmentsDefault
  const trimmed = raw.trim()
  if (!/^[0-9]+$/u.test(trimmed)) {
    return { error: `limit must be a decimal integer in 1..=${PACKET_BOUNDS.maxSegments}` }
  }
  const parsed = Number(trimmed)
  if (!Number.isSafeInteger(parsed) || parsed < 1 || parsed > PACKET_BOUNDS.maxSegments) {
    return { error: `limit must be an integer in 1..=${PACKET_BOUNDS.maxSegments}` }
  }
  return parsed
}

/**
 * Continuation metadata decoder, identical in behaviour to
 * `decodeSessionSegmentMetadata` in the harness protocol module.
 *
 * A chunk without a usable `seq_start` cannot be placed in the sequence, so it
 * is rejected outright rather than sorted to an arbitrary position. A `seq_end`
 * that is absent or inconsistent (`< seq_start`) degrades to "start only"
 * instead of failing the whole segment: the window still has a known position.
 */
export function decodeSessionSegmentMetadata(
  value: Readonly<Record<string, JsonValue>>,
): SessionSegmentMetadata | undefined {
  const sessionUid = value.session_uid
  if (typeof sessionUid !== 'string' || sessionUid.length === 0 || sessionUid.length > 256) {
    return undefined
  }
  const seqStart = value.seq_start
  if (typeof seqStart !== 'number' || !Number.isSafeInteger(seqStart) || seqStart < 0) {
    return undefined
  }
  const seqEnd = value.seq_end
  if (typeof seqEnd !== 'number' || !Number.isSafeInteger(seqEnd) || seqEnd < seqStart) {
    return { sessionUid, seqStart }
  }
  return { sessionUid, seqStart, seqEnd }
}

function stringField(value: JsonValue | undefined, max = 1024): string | undefined {
  return typeof value === 'string' && value.length > 0 && value.length <= max ? value : undefined
}

function integerField(value: JsonValue | undefined, min = 0): number | undefined {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= min ? value : undefined
}

function numberField(value: JsonValue | undefined, min = 0): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) && value >= min ? value : undefined
}

/**
 * Lineage is read from whichever chunk carries it, preferring the `session_meta`
 * chunk the importer emits. Every field is optional and independently
 * validated: a partially malformed metadata object yields partial lineage
 * rather than none, because lineage is context, not a security boundary.
 */
export function extractLineage(hits: readonly CairnSearchHit[]): SessionLineage {
  const preferred =
    hits.find(hit => hit.metadata.doc_type === 'session_meta') ?? hits[0]
  if (preferred === undefined) return {}
  const meta = preferred.metadata
  const lineage: SessionLineage = {
    ...optional('source', stringField(meta.source, 128)),
    ...optional('nativeId', stringField(meta.native_id, 256)),
    ...optional('rootSessionId', stringField(meta.root_session_id, 256)),
    ...optional('parentId', stringField(meta.parent_id, 256)),
    ...optional('depth', integerField(meta.depth)),
    ...optional('cwd', stringField(meta.cwd, 4096)),
    ...optional('repoPath', stringField(meta.repo_path, 4096)),
    ...optional('gitBranch', stringField(meta.git_branch, 256)),
    ...optional('model', stringField(meta.model, 256)),
    ...optional('provider', stringField(meta.provider, 128)),
    ...optional('createdAtUnixMs', integerField(meta.created_at_ms, 1)),
    ...optional('updatedAtUnixMs', integerField(meta.updated_at_ms, 1)),
    ...optional('messageCount', integerField(meta.message_count)),
    ...optional('usageTotal', integerField(meta.usage_total)),
    ...optional('usageCost', numberField(meta.usage_cost)),
  }
  return lineage
}

function optional<K extends string, V>(key: K, value: V | undefined): Partial<Record<K, V>> {
  return value === undefined ? {} : ({ [key]: value } as Record<K, V>)
}

function selectMetadata(
  source: Readonly<Record<string, JsonValue>>,
  maxBytes: number,
): Record<string, JsonValue> {
  const output: Record<string, JsonValue> = {}
  let used = 2
  for (const key of SESSION_METADATA_KEYS) {
    if (!Object.hasOwn(source, key)) continue
    const value = source[key]
    if (value === undefined) continue
    const bytes = new TextEncoder().encode(JSON.stringify({ [key]: value })).byteLength
    if (used + bytes > maxBytes) continue
    used += bytes
    output[key] = value
  }
  return output
}

/**
 * Truncate to at most `max` characters *including* the ellipsis, so the caller's
 * budget arithmetic holds exactly. Returning `max + 1` characters here would let
 * a run of truncated segments drift past the total text budget one char at a
 * time.
 */
function truncate(text: string, max: number): { readonly text: string; readonly truncated: boolean } {
  if (text.length <= max) return { text, truncated: false }
  // Reserve one character for the ellipsis.
  const room = max - 1
  if (room <= 0) return { text: '…', truncated: true }
  const cut = text.charCodeAt(room - 1)
  // Never split a surrogate pair.
  const end = cut >= 0xd800 && cut <= 0xdbff ? room - 1 : room
  return { text: `${text.slice(0, end)}…`, truncated: true }
}

/**
 * Drift classification.
 *
 * `incompatible` is reserved for the case where the packet's revision is ahead
 * of HEAD, which cannot happen against a consistent server and therefore means
 * the two answers came from different corpora. That is a correctness signal,
 * not a staleness one, so it is separated from `advanced`.
 */
export function classifyDrift(
  packetRevision: number,
  headRevision: number | undefined,
): PacketDrift {
  if (headRevision === undefined) return 'unknown'
  if (headRevision === packetRevision) return 'exact'
  return headRevision > packetRevision ? 'advanced' : 'incompatible'
}

export interface BuildPacketInput {
  readonly tenant: string
  readonly knowledgeBase: string
  readonly sessionUid: string
  readonly response: CairnSearchResponse
  readonly head?: CairnHeadResponse | undefined
  readonly limit?: number
  readonly maxTextCharsPerSegment?: number
  readonly maxTotalTextChars?: number
  readonly maxMetadataBytesPerHit?: number
}

/**
 * Build the continuation packet from one search response.
 *
 * Pure: no I/O, no clock, no randomness. Given the same response it returns the
 * same packet, which is what makes the ordering and bounds testable without a
 * server.
 */
export function buildSessionPacket(input: BuildPacketInput): SessionPacket {
  const limit = clamp(input.limit ?? PACKET_BOUNDS.segmentsDefault, 1, PACKET_BOUNDS.maxSegments)
  const perSegment = clamp(
    input.maxTextCharsPerSegment ?? PACKET_BOUNDS.maxTextCharsPerSegment,
    1,
    PACKET_BOUNDS.maxTextCharsPerSegment,
  )
  const totalBudget = clamp(
    input.maxTotalTextChars ?? PACKET_BOUNDS.maxTotalTextChars,
    1,
    PACKET_BOUNDS.maxTotalTextChars,
  )
  const maxMetadataBytes = input.maxMetadataBytesPerHit ?? 4 * 1024
  const response = input.response

  const matched: { readonly hit: CairnSearchHit; readonly meta: SessionSegmentMetadata }[] = []
  const lineageHits: CairnSearchHit[] = []
  let scopeMismatches = 0
  let malformedMetadata = 0
  let metaChunks = 0
  for (const hit of response.hits) {
    // A session_meta chunk has no seq_start by design: it is lineage, not a
    // window. Classify it before the segment decoder rejects it as malformed.
    if (hit.metadata.doc_type === 'session_meta') {
      if (hit.metadata.session_uid === input.sessionUid) {
        metaChunks += 1
        lineageHits.push(hit)
      } else {
        // Belongs to another session: same rejection as a foreign window.
        scopeMismatches += 1
      }
      continue
    }
    const meta = decodeSessionSegmentMetadata(hit.metadata)
    if (meta === undefined) {
      malformedMetadata += 1
      continue
    }
    if (meta.sessionUid !== input.sessionUid) {
      scopeMismatches += 1
      continue
    }
    matched.push({ hit, meta })
    lineageHits.push(hit)
  }

  // Sequence order, with a total tie-break so the packet is deterministic.
  matched.sort(
    (left, right) =>
      left.meta.seqStart - right.meta.seqStart ||
      (left.hit.id < right.hit.id ? -1 : left.hit.id > right.hit.id ? 1 : 0),
  )

  let remaining = totalBudget
  let truncated = false
  const segments: SessionSegment[] = []
  for (const { hit, meta } of matched.slice(0, limit)) {
    if (remaining <= 0) {
      truncated = true
      break
    }
    const cut = truncate(hit.text, Math.min(perSegment, remaining))
    if (cut.truncated) truncated = true
    remaining -= cut.text.length
    segments.push({
      id: hit.id,
      citation: citation(input.tenant, input.knowledgeBase, response.revision, hit.id),
      seqStart: meta.seqStart,
      ...(meta.seqEnd === undefined ? {} : { seqEnd: meta.seqEnd }),
      score: hit.score,
      posterior: hit.posterior,
      text: cut.text,
      truncated: cut.truncated,
      metadata: selectMetadata(hit.metadata, maxMetadataBytes),
    })
  }

  const headRevision = input.head?.revision
  return {
    trust: 'untrusted_reference_data',
    packet: 'session_continuation',
    tenant: input.tenant,
    knowledgeBase: input.knowledgeBase,
    sessionUid: input.sessionUid,
    revision: response.revision,
    corpusSha256: response.corpusSha256,
    embeddingProvider: response.embeddingProvider,
    embeddingModel: response.embeddingModel,
    dimension: response.dimension,
    mode: response.mode,
    approximate: response.approximate,
    ...(headRevision === undefined ? {} : { headRevision }),
    ...(input.head?.parentRevision === undefined ? {} : { parentRevision: input.head.parentRevision }),
    drift: classifyDrift(response.revision, headRevision),
    lineage: extractLineage(lineageHits),
    segments,
    citations: segments.map(segment => segment.citation),
    returnedSegments: segments.length,
    sourceHits: response.hits.length,
    scopeMismatches,
    malformedMetadata,
    metaChunks,
    truncated: truncated || segments.length < matched.length,
    remoteBytes: response.remoteBytes,
    rangeReads: response.rangeReads,
  }
}

function clamp(value: number, min: number, max: number): number {
  if (!Number.isSafeInteger(value)) return min
  return Math.min(max, Math.max(min, value))
}
