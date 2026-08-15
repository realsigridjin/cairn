import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  citation,
  decodeApiError,
  decodeHeadResponse,
  decodeSearchResponse,
  parseCitation,
} from '../src/protocol.ts'
import { DIGEST, headPayload, searchPayload } from './helpers.ts'

const DECODE_OPTIONS = { maxHits: 20, maxMetadataBytesPerHit: 4096 }

test('decodeSearchResponse maps the wire format to camelCase', () => {
  const decoded = decodeSearchResponse(searchPayload(), DECODE_OPTIONS)
  assert.equal(decoded.revision, 42)
  assert.equal(decoded.corpusSha256, DIGEST)
  assert.equal(decoded.mode, 'cold')
  assert.equal(decoded.scoreDomain, 'revision_calibrated_log_odds')
  assert.equal(decoded.approximate, false)
  assert.equal(decoded.remoteBytes, 1_258_291)
  assert.equal(decoded.rangeReads, 34)
  assert.equal(decoded.hits.length, 2)
  assert.equal(decoded.hits[0]?.lexicalEvidence, 2.1)
  assert.equal(decoded.hits[0]?.vectorEvidence, 0.4)
})

test('decodeSearchResponse rejects hits that are not sorted by descending score', () => {
  const payload = searchPayload({
    hits: [
      { id: 'a', score: 0.1, posterior: 0.5, lexical_evidence: 0, vector_evidence: 0, text: 'a', metadata: {} },
      { id: 'b', score: 0.9, posterior: 0.7, lexical_evidence: 0, vector_evidence: 0, text: 'b', metadata: {} },
    ],
  })
  assert.throws(
    () => decodeSearchResponse(payload, DECODE_OPTIONS),
    /hits must be sorted by descending score/u,
  )
})

test('decodeSearchResponse rejects duplicate hit ids', () => {
  const hit = { id: 'dup', score: 1, posterior: 0.7, lexical_evidence: 0, vector_evidence: 0, text: 'x', metadata: {} }
  assert.throws(
    () => decodeSearchResponse(searchPayload({ hits: [hit, { ...hit, score: 0.5 }] }), DECODE_OPTIONS),
    /duplicate hit id/u,
  )
})

test('decodeSearchResponse rejects posterior outside [0,1]', () => {
  const payload = searchPayload({
    hits: [{ id: 'a', score: 1, posterior: 1.4, lexical_evidence: 0, vector_evidence: 0, text: 'a', metadata: {} }],
  })
  assert.throws(() => decodeSearchResponse(payload, DECODE_OPTIONS), /posterior must be in \[0,1\]/u)
})

test('decodeSearchResponse rejects a non-hex corpus digest', () => {
  assert.throws(
    () => decodeSearchResponse(searchPayload({ corpus_sha256: 'z'.repeat(64) }), DECODE_OPTIONS),
    /invalid corpus_sha256/u,
  )
})

test('decodeSearchResponse rejects an unexpected score domain', () => {
  assert.throws(
    () => decodeSearchResponse(searchPayload({ score_domain: 'cosine' }), DECODE_OPTIONS),
    /unsupported score_domain/u,
  )
})

test('decodeSearchResponse rejects a mode outside cold|warm', () => {
  assert.throws(
    () => decodeSearchResponse(searchPayload({ mode: 'tepid' }), DECODE_OPTIONS),
    /mode must be cold or warm/u,
  )
})

test('decodeSearchResponse accepts negative log-odds scores', () => {
  const decoded = decodeSearchResponse(searchPayload(), DECODE_OPTIONS)
  assert.ok((decoded.hits[1]?.score ?? 0) < 0, 'second hit carries a negative calibrated score')
})

test('decodeHeadResponse treats a missing parent revision as genesis', () => {
  const decoded = decodeHeadResponse(headPayload({ parent_revision: null }))
  assert.equal(decoded.parentRevision, undefined)
  assert.equal(decoded.revision, 42)
  assert.equal(decoded.knowledgeBase, 'handbook')
})

test('decodeHeadResponse carries embedding provenance', () => {
  const decoded = decodeHeadResponse(headPayload({ has_uqa_bundle: true }))
  assert.equal(decoded.embeddingProvider, 'openrouter')
  assert.equal(decoded.dimension, 1024)
  assert.equal(decoded.shardCount, 8)
  assert.equal(decoded.hasUqaBundle, true)
})

test('decodeApiError preserves code, retryability and request id', () => {
  const decoded = decodeApiError(
    { error: { code: 'EMBEDDING_UNAVAILABLE', message: 'no key', retryable: false, request_id: 'r-9' } },
    503,
  )
  assert.equal(decoded.code, 'EMBEDDING_UNAVAILABLE')
  // 503 but explicitly non-retryable: the envelope wins over the status class.
  assert.equal(decoded.retryable, false)
  assert.equal(decoded.requestId, 'r-9')
})

test('decodeApiError falls back to a status-derived code for a malformed envelope', () => {
  const decoded = decodeApiError('not an object', 500)
  assert.equal(decoded.code, 'HTTP_500')
  assert.equal(decoded.retryable, true)
})

test('citation round-trips through parseCitation', () => {
  const raw = citation('acme', 'handbook', 42, 'doc-v3-c7')
  assert.equal(raw, 'cairn://acme/handbook/revision/42/chunk/doc-v3-c7')
  const parsed = parseCitation(raw)
  assert.deepEqual(parsed, {
    tenant: 'acme',
    knowledgeBase: 'handbook',
    revision: 42,
    chunkId: 'doc-v3-c7',
  })
})

test('citation percent-encodes scope and chunk components', () => {
  const raw = citation('ac me', 'hand/book', 7, 'doc c/1')
  assert.equal(raw, 'cairn://ac%20me/hand%2Fbook/revision/7/chunk/doc%20c%2F1')
  assert.deepEqual(parseCitation(raw), {
    tenant: 'ac me',
    knowledgeBase: 'hand/book',
    revision: 7,
    chunkId: 'doc c/1',
  })
})

test('parseCitation rejects malformed identifiers', () => {
  assert.equal(parseCitation('https://acme/handbook/revision/42/chunk/x'), undefined)
  assert.equal(parseCitation('cairn://acme/handbook/revision/0/chunk/x'), undefined)
  assert.equal(parseCitation('cairn://acme/handbook/chunk/x'), undefined)
})
