import test from 'node:test'
import assert from 'node:assert/strict'
import http from 'node:http'
import { CairnClient, normalizeBaseUrl, assertCompatibleVersion } from '../lib/client.js'

const digest = 'b'.repeat(64)
function searchBody() { return { revision: 1, embedding_provider: 'openrouter', embedding_model: 'm', dimension: 4, corpus_sha256: digest, mode: 'cold', score_domain: 'revision_calibrated_log_odds', approximate: true, hits: [{ id: 'x', score: 1, posterior: 0.7, lexical_evidence: 0.4, vector_evidence: 0.6, text: 'evidence', metadata: {} }], remote_bytes: 10, range_reads: 1 } }
async function server(handler) {
  const instance = http.createServer(handler)
  await new Promise(resolve => instance.listen(0, '127.0.0.1', resolve))
  const address = instance.address()
  return { instance, url: `http://127.0.0.1:${address.port}` }
}

function client(baseUrl) { return new CairnClient({ baseUrl, tenant: 'acme', knowledgeBase: 'docs', tokenEnv: 'TEST_CAIRN_TOKEN', timeoutMs: 3000, retries: 1, maxRetryDelayMs: 5, maxResponseBytes: 64 * 1024, maxHits: 2, maxMetadataBytesPerHit: 1024 }) }

test('URL and version policy rejects unsafe destinations', () => {
  assert.throws(() => normalizeBaseUrl('http://localhost.evil.example'), /HTTPS/)
  assert.throws(() => normalizeBaseUrl('https://user:pass@example.com'), /credentials/)
  assert.doesNotThrow(() => assertCompatibleVersion('2.1.0', 1))
  assert.throws(() => assertCompatibleVersion('3.0.0', 1), /unsupported/)
})

test('search retries retryable response and propagates auth/call id', async t => {
  let calls = 0
  process.env.TEST_CAIRN_TOKEN = '0123456789abcdef'
  const mock = await server((req, res) => {
    calls += 1
    assert.equal(req.headers.authorization, 'Bearer 0123456789abcdef')
    assert.equal(req.headers['x-dsh-tool-call-id'], 'call_1')
    res.setHeader('content-type', 'application/json')
    if (calls === 1) { res.statusCode = 503; res.end(JSON.stringify({ error: { code: 'TEMP', message: 'retry', retryable: true } })); return }
    res.end(JSON.stringify(searchBody()))
  })
  t.after(() => mock.instance.close())
  const result = await client(mock.url).search({ query: 'q', limit: 1, candidateLimit: 12, filters: {}, callId: 'call_1' })
  assert.equal(result.hits[0].id, 'x')
  assert.equal(calls, 2)
})

test('structured retryable=false suppresses retry', async t => {
  let calls = 0
  const mock = await server((_req, res) => { calls += 1; res.statusCode = 503; res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ error: { code: 'CONFIG', message: 'no key', retryable: false } })) })
  t.after(() => mock.instance.close())
  await assert.rejects(() => client(mock.url).search({ query: 'q', limit: 1, candidateLimit: 12, filters: {} }), /no key/)
  assert.equal(calls, 1)
})

test('bounds body and enforces JSON content type', async t => {
  const mock = await server((_req, res) => { res.setHeader('content-type', 'text/plain'); res.end('not json') })
  t.after(() => mock.instance.close())
  await assert.rejects(() => client(mock.url).health(), /non-JSON/)
})
