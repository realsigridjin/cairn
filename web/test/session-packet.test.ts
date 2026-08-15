/**
 * Continuation packet invariants.
 *
 * `buildSessionPacket` is pure, so every ordering, rejection and bounds rule is
 * asserted directly against constructed responses — no server, no clock, no
 * network, no timing luck. The HTTP surface is covered separately in
 * `session-routes.test.ts`; these tests pin the semantics that surface depends
 * on.
 */

import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  PACKET_BOUNDS,
  buildSessionPacket,
  classifyDrift,
  decodeSessionSegmentMetadata,
  extractLineage,
  validateSegmentLimit,
  validateSessionUid,
} from '../src/session-packet.ts'
import type { CairnHeadResponse, CairnSearchHit, CairnSearchResponse, JsonValue } from '../src/protocol.ts'
import { DIGEST } from './helpers.ts'

const UID = 'senpi:01a0060a'

function hit(overrides: Partial<CairnSearchHit> & { readonly id: string }): CairnSearchHit {
  return {
    score: 0,
    posterior: 0.5,
    lexicalEvidence: 1,
    vectorEvidence: 1,
    text: 'segment text',
    metadata: {},
    ...overrides,
  }
}

/** A window chunk as `scripts/cairn_sessions/emit.py` produces it. */
function windowHit(
  seqStart: number,
  options: {
    readonly id?: string
    readonly score?: number
    readonly text?: string
    readonly sessionUid?: string
    readonly seqEnd?: number | JsonValue
    readonly extra?: Record<string, JsonValue>
  } = {},
): CairnSearchHit {
  const seqEnd = options.seqEnd === undefined ? seqStart + 1 : options.seqEnd
  return hit({
    id: options.id ?? `s:${UID}:m:${seqStart}-${seqStart + 1}`,
    score: options.score ?? 0,
    text: options.text ?? `window ${seqStart}`,
    metadata: {
      doc_type: 'session_window',
      session_uid: options.sessionUid ?? UID,
      seq_start: seqStart,
      seq_end: seqEnd,
      ...(options.extra ?? {}),
    },
  })
}

function response(hits: readonly CairnSearchHit[], overrides: Partial<CairnSearchResponse> = {}): CairnSearchResponse {
  return {
    revision: 7,
    embeddingProvider: 'openrouter',
    embeddingModel: 'qwen/qwen3-embedding-8b',
    dimension: 1024,
    corpusSha256: DIGEST,
    mode: 'cold',
    scoreDomain: 'revision_calibrated_log_odds',
    approximate: false,
    hits,
    remoteBytes: 2048,
    rangeReads: 6,
    ...overrides,
  }
}

function head(revision: number, parentRevision?: number): CairnHeadResponse {
  return {
    tenant: 'local',
    knowledgeBase: 'sessions',
    revision,
    ...(parentRevision === undefined ? {} : { parentRevision }),
    createdAtUnixMs: 1_700_000_000_000,
    embeddingProvider: 'openrouter',
    embeddingModel: 'qwen/qwen3-embedding-8b',
    dimension: 1024,
    shardCount: 2,
    hasUqaBundle: false,
  }
}

function build(hits: readonly CairnSearchHit[], options: Record<string, unknown> = {}) {
  return buildSessionPacket({
    tenant: 'local',
    knowledgeBase: 'sessions',
    sessionUid: UID,
    response: response(hits),
    head: head(7),
    ...options,
  })
}

/* ---------- Ordering ---------- */

test('segments are ordered by seq_start, not by retrieval score', () => {
  // Score order is deliberately the reverse of sequence order.
  const packet = build([
    windowHit(40, { score: 9 }),
    windowHit(10, { score: 1 }),
    windowHit(30, { score: 5 }),
    windowHit(20, { score: 7 }),
  ])
  assert.deepEqual(
    packet.segments.map(segment => segment.seqStart),
    [10, 20, 30, 40],
  )
})

test('ties on seq_start break on chunk id so the packet is deterministic', () => {
  const forward = build([
    windowHit(5, { id: 'b', score: 1 }),
    windowHit(5, { id: 'a', score: 9 }),
  ])
  const reversed = build([
    windowHit(5, { id: 'a', score: 9 }),
    windowHit(5, { id: 'b', score: 1 }),
  ])
  assert.deepEqual(forward.segments.map(s => s.id), ['a', 'b'])
  // Input order must not change the output: the packet is a pure function of
  // the corpus, not of retrieval jitter.
  assert.deepEqual(forward.segments.map(s => s.id), reversed.segments.map(s => s.id))
})

test('seq_start 0 is a valid position, not a falsy absence', () => {
  const packet = build([windowHit(1), windowHit(0)])
  assert.deepEqual(packet.segments.map(s => s.seqStart), [0, 1])
  assert.equal(packet.malformedMetadata, 0)
})

test('citations are emitted in segment order and pin the packet revision', () => {
  const packet = build([windowHit(20, { id: 'later' }), windowHit(10, { id: 'earlier' })])
  assert.deepEqual(packet.citations, [
    'cairn://local/sessions/revision/7/chunk/earlier',
    'cairn://local/sessions/revision/7/chunk/later',
  ])
  assert.deepEqual(packet.citations, packet.segments.map(s => s.citation))
})

/* ---------- Malformed metadata ---------- */

test('chunks without a usable seq_start are dropped and counted, never reordered in', () => {
  const packet = build([
    windowHit(10),
    hit({ id: 'no-seq', metadata: { session_uid: UID } }),
    hit({ id: 'string-seq', metadata: { session_uid: UID, seq_start: '3' } }),
    hit({ id: 'negative-seq', metadata: { session_uid: UID, seq_start: -1 } }),
    hit({ id: 'float-seq', metadata: { session_uid: UID, seq_start: 1.5 } }),
    hit({ id: 'no-metadata', metadata: {} }),
  ])
  assert.deepEqual(packet.segments.map(s => s.id), [`s:${UID}:m:10-11`])
  assert.equal(packet.malformedMetadata, 5)
  assert.equal(packet.scopeMismatches, 0)
})

test('a session_meta chunk is lineage, not a malformed segment', () => {
  // The default import tier emits exactly one meta chunk and no windows.
  // Reporting that as "malformed" would describe a correct corpus as broken.
  const packet = build([
    hit({
      id: `s:${UID}:meta`,
      metadata: {
        doc_type: 'session_meta',
        session_uid: UID,
        source: 'dsh',
        cwd: '/repo',
        created_at_ms: 1_700_000_000_000,
        updated_at_ms: 1_700_000_100_000,
        usage_total: 12_345,
        usage_cost: 0.25,
      },
    }),
  ])
  assert.equal(packet.metaChunks, 1)
  assert.equal(packet.malformedMetadata, 0)
  assert.equal(packet.returnedSegments, 0)
  // Lineage still comes through, which is the point of the meta chunk.
  assert.equal(packet.lineage.source, 'dsh')
  assert.equal(packet.lineage.cwd, '/repo')
  assert.equal(packet.lineage.createdAtUnixMs, 1_700_000_000_000)
  assert.equal(packet.lineage.updatedAtUnixMs, 1_700_000_100_000)
  assert.equal(packet.lineage.usageTotal, 12_345)
  assert.equal(packet.lineage.usageCost, 0.25)
})

test('a foreign session_meta chunk is a scope mismatch, not lineage', () => {
  const packet = build([
    windowHit(0),
    hit({
      id: 'other-meta',
      metadata: { doc_type: 'session_meta', session_uid: 'other:session', cwd: '/leak' },
    }),
  ])
  assert.equal(packet.metaChunks, 0)
  assert.equal(packet.scopeMismatches, 1)
  assert.notEqual(packet.lineage.cwd, '/leak')
})

test('a chunk for another session is dropped as a scope mismatch, not rendered', () => {
  // Upstream applies metadata filters after a bounded candidate pool, so a
  // foreign chunk reaching the client is expected, not theoretical.
  const packet = build([windowHit(10), windowHit(20, { sessionUid: 'other:session' })])
  assert.equal(packet.segments.length, 1)
  assert.equal(packet.scopeMismatches, 1)
  assert.equal(packet.malformedMetadata, 0)
  assert.ok(!JSON.stringify(packet).includes('other:session'))
})

test('an inconsistent seq_end degrades to start-only rather than dropping the window', () => {
  const packet = build([windowHit(10, { seqEnd: 4 }), windowHit(20, { seqEnd: 'x' })])
  assert.equal(packet.segments.length, 2)
  assert.equal(packet.segments[0]?.seqEnd, undefined)
  assert.equal(packet.segments[1]?.seqEnd, undefined)
  assert.equal(packet.malformedMetadata, 0)
})

test('decodeSessionSegmentMetadata matches the harness decoder exactly', () => {
  assert.deepEqual(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: 2, seq_end: 5 }), {
    sessionUid: 'a',
    seqStart: 2,
    seqEnd: 5,
  })
  assert.deepEqual(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: 2 }), {
    sessionUid: 'a',
    seqStart: 2,
  })
  assert.equal(decodeSessionSegmentMetadata({ seq_start: 2 }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: '', seq_start: 2 }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: 'a'.repeat(257), seq_start: 0 }), undefined)
})

/* ---------- Bounds ---------- */

test('the segment limit caps how many ordered windows are returned', () => {
  const hits = Array.from({ length: 20 }, (_, index) => windowHit(index))
  const packet = build(hits, { limit: 5 })
  assert.equal(packet.returnedSegments, 5)
  assert.deepEqual(packet.segments.map(s => s.seqStart), [0, 1, 2, 3, 4])
  // Dropping later windows is truncation and must be reported as such.
  assert.equal(packet.truncated, true)
})

test('the limit never exceeds the hard segment ceiling', () => {
  const hits = Array.from({ length: PACKET_BOUNDS.maxSegments + 10 }, (_, index) => windowHit(index))
  const packet = build(hits, { limit: 10_000 })
  assert.equal(packet.returnedSegments, PACKET_BOUNDS.maxSegments)
})

test('per-segment text is truncated at the configured cap without splitting a surrogate pair', () => {
  const packet = build([windowHit(0, { text: `${'a'.repeat(40)}😀` })], {
    maxTextCharsPerSegment: 41,
  })
  const segment = packet.segments[0]
  assert.ok(segment !== undefined)
  assert.equal(segment.truncated, true)
  assert.equal(packet.truncated, true)
  // 40 'a' + ellipsis: the surrogate pair was dropped whole, never halved.
  assert.equal(segment.text, `${'a'.repeat(40)}…`)
  assert.ok(!/[\uD800-\uDBFF]$/u.test(segment.text.slice(0, -1)))
})

test('the total text budget stops emission instead of returning an unbounded page', () => {
  const hits = Array.from({ length: 10 }, (_, index) => windowHit(index, { text: 'x'.repeat(500) }))
  const packet = build(hits, { maxTotalTextChars: 1_200, maxTextCharsPerSegment: 500 })
  const total = packet.segments.reduce((sum, segment) => sum + segment.text.length, 0)
  assert.ok(total <= 1_200, `total text ${total} exceeded the budget`)
  assert.ok(packet.returnedSegments < 10)
  assert.equal(packet.truncated, true)
})

test('an exactly-fitting packet is not reported as truncated', () => {
  const packet = build([windowHit(0, { text: 'short' }), windowHit(1, { text: 'also short' })], {
    limit: 2,
  })
  assert.equal(packet.truncated, false)
  assert.equal(packet.returnedSegments, 2)
})

test('validateSegmentLimit enforces the documented range', () => {
  assert.equal(validateSegmentLimit(undefined), PACKET_BOUNDS.segmentsDefault)
  assert.equal(validateSegmentLimit(''), PACKET_BOUNDS.segmentsDefault)
  assert.equal(validateSegmentLimit('3'), 3)
  assert.deepEqual(validateSegmentLimit('0'), {
    error: `limit must be an integer in 1..=${PACKET_BOUNDS.maxSegments}`,
  })
  assert.deepEqual(validateSegmentLimit(String(PACKET_BOUNDS.maxSegments + 1)), {
    error: `limit must be an integer in 1..=${PACKET_BOUNDS.maxSegments}`,
  })
  assert.deepEqual(validateSegmentLimit('1.5'), {
    error: `limit must be a decimal integer in 1..=${PACKET_BOUNDS.maxSegments}`,
  })
  assert.deepEqual(validateSegmentLimit('0x10'), {
    error: `limit must be a decimal integer in 1..=${PACKET_BOUNDS.maxSegments}`,
  })
  assert.deepEqual(validateSegmentLimit('0b11'), {
    error: `limit must be a decimal integer in 1..=${PACKET_BOUNDS.maxSegments}`,
  })
})

test('validateSessionUid rejects empty, oversized and control-character uids', () => {
  assert.equal(validateSessionUid('  senpi:abc  '), 'senpi:abc')
  assert.ok(typeof validateSessionUid('') === 'object')
  assert.ok(typeof validateSessionUid('a'.repeat(257)) === 'object')
  assert.ok(typeof validateSessionUid('bad\nuid') === 'object')
  assert.ok(typeof validateSessionUid(null) === 'object')
})

/* ---------- Drift ---------- */

test('drift compares the pinned revision against HEAD', () => {
  assert.equal(classifyDrift(7, 7), 'exact')
  assert.equal(classifyDrift(7, 9), 'advanced')
  // Packet ahead of HEAD means two different corpora answered.
  assert.equal(classifyDrift(9, 7), 'incompatible')
  assert.equal(classifyDrift(7, undefined), 'unknown')
})

test('an unresolved HEAD yields unknown drift rather than denying the packet', () => {
  const packet = buildSessionPacket({
    tenant: 'local',
    knowledgeBase: 'sessions',
    sessionUid: UID,
    response: response([windowHit(0)]),
    head: undefined,
  })
  assert.equal(packet.drift, 'unknown')
  assert.equal(packet.headRevision, undefined)
  assert.equal(packet.returnedSegments, 1)
})

test('the packet reports lineage and provenance from the pinned revision', () => {
  const packet = buildSessionPacket({
    tenant: 'local',
    knowledgeBase: 'sessions',
    sessionUid: UID,
    response: response([windowHit(0)]),
    head: head(9, 8),
  })
  assert.equal(packet.revision, 7)
  assert.equal(packet.headRevision, 9)
  assert.equal(packet.parentRevision, 8)
  assert.equal(packet.drift, 'advanced')
  assert.equal(packet.corpusSha256, DIGEST)
  assert.equal(packet.trust, 'untrusted_reference_data')
  assert.equal(packet.packet, 'session_continuation')
})

/* ---------- Lineage ---------- */

test('lineage prefers the session_meta chunk over a window chunk', () => {
  const lineage = extractLineage([
    windowHit(5, { extra: { cwd: '/wrong', model: 'window-model' } }),
    hit({
      id: `s:${UID}:meta`,
      metadata: {
        doc_type: 'session_meta',
        session_uid: UID,
        source: 'senpi',
        cwd: '/right',
        model: 'claude',
        root_session_id: 'root-1',
        depth: 1,
        message_count: 42,
      },
    }),
  ])
  assert.equal(lineage.cwd, '/right')
  assert.equal(lineage.model, 'claude')
  assert.equal(lineage.source, 'senpi')
  assert.equal(lineage.rootSessionId, 'root-1')
  assert.equal(lineage.depth, 1)
  assert.equal(lineage.messageCount, 42)
})

test('malformed lineage fields are individually dropped, not fatal', () => {
  const lineage = extractLineage([
    hit({
      id: 'meta',
      metadata: {
        doc_type: 'session_meta',
        session_uid: UID,
        source: 'senpi',
        cwd: 42,
        depth: 'deep',
        message_count: -3,
        model: '',
      },
    }),
  ])
  assert.equal(lineage.source, 'senpi')
  assert.equal(lineage.cwd, undefined)
  assert.equal(lineage.depth, undefined)
  assert.equal(lineage.messageCount, undefined)
  assert.equal(lineage.model, undefined)
})

test('lineage of an empty result set is empty rather than throwing', () => {
  assert.deepEqual(extractLineage([]), {})
})

/* ---------- Empty ---------- */

test('a session with no matching chunks yields an empty, well-formed packet', () => {
  const packet = build([])
  assert.equal(packet.returnedSegments, 0)
  assert.deepEqual(packet.segments, [])
  assert.deepEqual(packet.citations, [])
  assert.equal(packet.truncated, false)
  assert.equal(packet.sourceHits, 0)
})

test('only allowlisted metadata keys reach the packet', () => {
  const packet = build([
    windowHit(0, { extra: { secret_field: 'do-not-render', cwd: '/repo' } }),
  ])
  const metadata = packet.segments[0]?.metadata ?? {}
  assert.equal(metadata.cwd, '/repo')
  assert.ok(!Object.hasOwn(metadata, 'secret_field'))
})
