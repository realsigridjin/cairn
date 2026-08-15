import test, { after } from 'node:test'
import assert from 'node:assert/strict'
import http from 'node:http'
import { mkdir, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { decodeSessionSegmentMetadata } from '../lib/protocol.js'

const root = new URL('../', import.meta.url)
const nodeModules = new URL('../node_modules/', import.meta.url)
const digest = 'd'.repeat(64)

async function stub(name, source) {
  const dir = new URL(`../node_modules/${name}/`, import.meta.url)
  await mkdir(dir, { recursive: true })
  await writeFile(new URL('package.json', dir), JSON.stringify({ name, type: 'module', exports: './index.js' }))
  await writeFile(new URL('index.js', dir), source)
}

await rm(nodeModules, { recursive: true, force: true })
await stub('@deepseek-ai/cordis', 'export {}')
await stub('@deepseek-ai/dsh-tools', 'export const defineTool = value => value')
await stub('@deepseek-ai/schemastery', `const chain=()=>({required(){return this},default(){return this},min(){return this},max(){return this},step(){return this}}); export default {string:chain,number:chain,boolean:chain,array:chain,object:chain}`)
after(() => rm(nodeModules, { recursive: true, force: true }))

const module = await import(`${pathToFileURL(join(new URL(root).pathname, 'lib/index.js')).href}?test=session-packet`)
const exec = () => ({ signal: new AbortController().signal, callId: 'call_seq' })

// Score order deliberately disagrees with seq_start order so client-side
// reordering is observable. 'other' is a foreign session; 'bad' has a
// non-numeric seq_start.
function sessionHits() {
  return [
    { id: 's2', score: 3, posterior: 0.9, lexical_evidence: 0.5, vector_evidence: 0.5, text: 'second segment text', metadata: { session_uid: 'sess-1', seq_start: 20, seq_end: 24, role: 'assistant' } },
    { id: 's1', score: 2, posterior: 0.8, lexical_evidence: 0.5, vector_evidence: 0.5, text: 'first segment text', metadata: { session_uid: 'sess-1', seq_start: 10, role: 'user' } },
    { id: 'other', score: 1.5, posterior: 0.7, lexical_evidence: 0.5, vector_evidence: 0.5, text: 'other session segment', metadata: { session_uid: 'sess-2', seq_start: 5 } },
    { id: 'bad', score: 1, posterior: 0.6, lexical_evidence: 0.5, vector_evidence: 0.5, text: 'malformed segment', metadata: { session_uid: 'sess-1', seq_start: 'ten' } },
    { id: 'meta', score: 0.5, posterior: 0.5, lexical_evidence: 0.5, vector_evidence: 0.5, text: 'session header', metadata: { doc_type: 'session_meta', session_uid: 'sess-1', cwd: '/repo' } },
  ]
}

async function mockServer({ headRevision = 9 } = {}) {
  const searches = []
  const instance = http.createServer((req, res) => {
    res.setHeader('content-type', 'application/json')
    if (req.method === 'GET' && req.url.endsWith('/head')) {
      res.end(JSON.stringify({ tenant: 'acme', knowledge_base: 'sessions', revision: headRevision, parent_revision: 8, created_at_unix_ms: 1, embedding_provider: 'openrouter', embedding_model: 'm', dimension: 4, shard_count: 1, has_uqa_bundle: false }))
      return
    }
    let body = ''
    req.on('data', chunk => { body += chunk })
    req.on('end', () => {
      const parsed = JSON.parse(body)
      searches.push(parsed)
      res.end(JSON.stringify({ revision: 9, embedding_provider: 'openrouter', embedding_model: 'm', dimension: 4, corpus_sha256: digest, mode: 'warm', score_domain: 'revision_calibrated_log_odds', approximate: true, hits: sessionHits().slice(0, parsed.limit), remote_bytes: 100, range_reads: 2 }))
    })
  })
  await new Promise(resolve => instance.listen(0, '127.0.0.1', resolve))
  return { instance, url: `http://127.0.0.1:${instance.address().port}`, searches }
}

async function mount(url, extra = {}) {
  const definitions = []
  await module.apply({ tools: { register(value) { definitions.push(value) } } }, {
    baseUrl: url,
    tenant: 'acme',
    knowledgeBase: 'sessions',
    sessionPacketEnabled: true,
    metadataKeys: ['session_uid', 'seq_start', 'seq_end', 'role'],
    fixedFiltersJson: '{"visibility":"agent"}',
    ...extra,
  })
  return definitions
}

test('session packet filters by session_uid, reorders by seq_start, and renders provenance, lineage, and citations', async t => {
  const mock = await mockServer()
  t.after(() => mock.instance.close())
  const [searchTool, sessionTool] = await mount(mock.url)
  assert.equal(searchTool.name, 'cairn_search')
  assert.equal(sessionTool.name, 'cairn_session_packet')
  const value = await sessionTool.execute({ session_uid: 'sess-1' }, exec())
  assert.equal(mock.searches.length, 1)
  assert.equal(mock.searches[0].query, 'sess-1')
  assert.equal(mock.searches[0].limit, 64)
  assert.equal(mock.searches[0].candidate_limit, 768)
  assert.deepEqual(mock.searches[0].filters, { session_uid: 'sess-1', visibility: 'agent' })
  assert.deepEqual(value.segments.map(segment => segment.id), ['s1', 's2'])
  assert.deepEqual(value.segments.map(segment => segment.seqStart), [10, 20])
  assert.equal(value.segments[1].seqEnd, 24)
  assert.equal('seqEnd' in value.segments[0], false)
  assert.deepEqual({ ...value.segments[0].metadata }, { session_uid: 'sess-1', seq_start: 10, role: 'user' })
  assert.equal(value.trust, 'untrusted_retrieval_evidence')
  assert.equal(value.packet, 'session_continuation')
  assert.equal(value.revision, 9)
  assert.equal(value.corpusSha256, digest)
  assert.equal(value.mode, 'warm')
  assert.equal(value.headRevision, 9)
  assert.equal(value.parentRevision, 8)
  assert.equal(value.drifted, false)
  assert.deepEqual(value.citations, [
    'cairn://acme/sessions/revision/9/chunk/s1',
    'cairn://acme/sessions/revision/9/chunk/s2',
  ])
  assert.equal(value.returnedSegments, 2)
  assert.equal(value.sourceHits, 5)
  assert.equal(value.scopeMismatches, 1)
  assert.equal(value.malformedMetadata, 1)
  assert.equal(value.metaChunks, 1)
  assert.equal(value.truncated, false)
  const rendered = sessionTool.output.render({}, value)[0].text
  assert.match(rendered, /CAIRN-SESSION sessions@revision-9 session_uid=sess-1/u)
  assert.match(rendered, /lineage: head_revision=9 parent_revision=8 drifted=false/u)
  assert.match(rendered, /UNTRUSTED REFERENCE DATA/u)
  assert.match(rendered, /BEGIN_UNTRUSTED_CAIRN_EVIDENCE 1/u)
  assert.match(rendered, /citation: cairn:\/\/acme\/sessions\/revision\/9\/chunk\/s1/u)
  assert.match(rendered, /seq_start: 20 seq_end: 24/u)
  assert.ok(rendered.indexOf('chunk/s1') < rendered.indexOf('chunk/s2'))
  assert.ok(!rendered.includes('truncated by trusted plugin limits'))
})

test('bounds: model limit and total text budget truncate deterministically', async t => {
  const mock = await mockServer()
  t.after(() => mock.instance.close())
  const [, sessionTool] = await mount(mock.url)
  const limited = await sessionTool.execute({ session_uid: 'sess-1', limit: 1 }, exec())
  assert.deepEqual(limited.segments.map(segment => segment.id), ['s1'])
  assert.equal(limited.returnedSegments, 1)
  assert.equal(limited.truncated, true)
  assert.match(sessionTool.output.render({}, limited)[0].text, /truncated by trusted plugin limits/u)
  const [, budgetTool] = await mount(mock.url, { sessionMaxTotalTextChars: 20 })
  const bounded = await budgetTool.execute({ session_uid: 'sess-1' }, exec())
  assert.equal(bounded.segments.length, 2)
  assert.equal(bounded.segments[0].text, 'first segment text')
  assert.equal(bounded.segments[1].text, 's…')
  assert.equal(bounded.truncated, true)
  const [, narrowTool] = await mount(mock.url, { sessionMaxSegments: 2 })
  const narrow = await narrowTool.execute({ session_uid: 'sess-1' }, exec())
  assert.equal(mock.searches.at(-1).limit, 2)
  assert.deepEqual(narrow.segments.map(segment => segment.id), ['s1', 's2'])
  assert.equal(narrow.scopeMismatches, 0)
  await assert.rejects(() => narrowTool.execute({ session_uid: 'sess-1', limit: 3 }, exec()), /limit must be an integer in 1\.\.=2/u)
})

test('rejects unknown and malformed arguments', async t => {
  const mock = await mockServer()
  t.after(() => mock.instance.close())
  const [, sessionTool] = await mount(mock.url)
  await assert.rejects(() => sessionTool.execute({ session_uid: 'sess-1', surprise: true }, exec()), /unknown cairn_session_packet argument/u)
  await assert.rejects(() => sessionTool.execute({}, exec()), /session_uid must be a string/u)
  await assert.rejects(() => sessionTool.execute({ session_uid: '   ' }, exec()), /session_uid/u)
  await assert.rejects(() => sessionTool.execute({ session_uid: 'bad\nuid' }, exec()), /session_uid/u)
  await assert.rejects(() => sessionTool.execute({ session_uid: 'sess-1', limit: 0 }, exec()), /limit must be an integer/u)
  assert.equal(mock.searches.length, 0)
})

test('disabled by default and session packet tool name is validated', async t => {
  const mock = await mockServer()
  t.after(() => mock.instance.close())
  const defaults = []
  await module.apply({ tools: { register(value) { defaults.push(value) } } }, { baseUrl: mock.url, tenant: 'acme', knowledgeBase: 'sessions' })
  assert.deepEqual(defaults.map(definition => definition.name), ['cairn_search'])
  const disabled = await mount(mock.url, { sessionPacketEnabled: false })
  assert.deepEqual(disabled.map(definition => definition.name), ['cairn_search'])
  await assert.rejects(() => mount(mock.url, { sessionPacketToolName: 'bad name' }), /sessionPacketToolName/u)
})

test('malformed continuation metadata is rejected field by field', () => {
  assert.deepEqual(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: 3 }), { sessionUid: 'a', seqStart: 3 })
  assert.deepEqual(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: 3, seq_end: 5 }), { sessionUid: 'a', seqStart: 3, seqEnd: 5 })
  assert.deepEqual(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: 3, seq_end: 2 }), { sessionUid: 'a', seqStart: 3 })
  assert.equal(decodeSessionSegmentMetadata({ session_uid: 'a' }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: '3' }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: 1.5 }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: 'a', seq_start: -1 }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ seq_start: 3 }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: '', seq_start: 3 }), undefined)
  assert.equal(decodeSessionSegmentMetadata({ session_uid: 7, seq_start: 3 }), undefined)
})

test('surfaces lineage drift between head and search revisions', async t => {
  const mock = await mockServer({ headRevision: 10 })
  t.after(() => mock.instance.close())
  const [, sessionTool] = await mount(mock.url)
  const value = await sessionTool.execute({ session_uid: 'sess-1' }, exec())
  assert.equal(value.headRevision, 10)
  assert.equal(value.revision, 9)
  assert.equal(value.drifted, true)
  assert.match(sessionTool.output.render({}, value)[0].text, /drifted=true/u)
})
