import test from 'node:test'
import assert from 'node:assert/strict'
import { decodeSearchResponse, decodeHeadResponse, selectMetadata } from '../lib/protocol.js'

const digest = 'a'.repeat(64)
const response = {
  revision: 2, embedding_provider: 'openrouter', embedding_model: 'qwen/qwen3-embedding-8b', dimension: 1024,
  corpus_sha256: digest, mode: 'cold', score_domain: 'revision_calibrated_log_odds', approximate: true,
  hits: [{ id: 'a', score: 2, posterior: 0.8, lexical_evidence: 1, vector_evidence: 1, text: 'hello', metadata: { source: 'x', secret: 'y' } }],
  remote_bytes: 100, range_reads: 2,
}

test('decodes strict search provenance and rejects duplicates', () => {
  const decoded = decodeSearchResponse(response, { maxHits: 2, maxMetadataBytesPerHit: 4096 })
  assert.equal(decoded.embeddingModel, 'qwen/qwen3-embedding-8b')
  assert.throws(() => decodeSearchResponse({ ...response, hits: [response.hits[0], response.hits[0]] }, { maxHits: 2, maxMetadataBytesPerHit: 4096 }), /duplicate/)
})

test('decodes head and metadata allowlist', () => {
  const head = decodeHeadResponse({ tenant: 'acme', knowledge_base: 'docs', revision: 2, parent_revision: null, created_at_unix_ms: 1, embedding_provider: 'openrouter', embedding_model: 'm', dimension: 4, shard_count: 1, has_uqa_bundle: false })
  assert.equal(head.knowledgeBase, 'docs')
  assert.deepEqual({ ...selectMetadata(response.hits[0].metadata, ['source'], 1024) }, { source: 'x' })
})
