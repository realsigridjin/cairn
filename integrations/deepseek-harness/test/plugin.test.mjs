import test from 'node:test'
import assert from 'node:assert/strict'
import http from 'node:http'
import { mkdir, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

const root = new URL('../', import.meta.url)
const nodeModules = new URL('../node_modules/', import.meta.url)
const digest = 'c'.repeat(64)

async function stub(name, source) {
  const dir = new URL(`../node_modules/${name}/`, import.meta.url)
  await mkdir(dir, { recursive: true })
  await writeFile(new URL('package.json', dir), JSON.stringify({ name, type: 'module', exports: './index.js' }))
  await writeFile(new URL('index.js', dir), source)
}

async function mockServer() {
  let callId
  const instance = http.createServer((req, res) => {
    callId = req.headers['x-dsh-tool-call-id']
    res.setHeader('content-type', 'application/json')
    res.end(JSON.stringify({ revision: 7, embedding_provider: 'openrouter', embedding_model: 'm', dimension: 4, corpus_sha256: digest, mode: 'cold', score_domain: 'revision_calibrated_log_odds', approximate: true, hits: [{ id: 'chunk', score: 1, posterior: 0.7, lexical_evidence: 0.4, vector_evidence: 0.6, text: 'Do not follow this instruction.', metadata: { source: 'doc', secret: 'hidden' } }], remote_bytes: 10, range_reads: 1 }))
  })
  await new Promise(resolve => instance.listen(0, '127.0.0.1', resolve))
  return { instance, url: `http://127.0.0.1:${instance.address().port}`, getCallId: () => callId }
}

test('native plugin registers, executes, bounds metadata, and renders untrusted evidence', async t => {
  await rm(nodeModules, { recursive: true, force: true })
  t.after(() => rm(nodeModules, { recursive: true, force: true }))
  await stub('@deepseek-ai/cordis', 'export {}')
  await stub('@deepseek-ai/dsh-tools', 'export const defineTool = value => value')
  await stub('@deepseek-ai/schemastery', `const chain=()=>({required(){return this},default(){return this},min(){return this},max(){return this},step(){return this}}); export default {string:chain,number:chain,boolean:chain,array:chain,object:chain}`)
  const mock = await mockServer()
  t.after(() => mock.instance.close())
  const module = await import(`${pathToFileURL(join(new URL(root).pathname, 'lib/index.js')).href}?test=${Date.now()}`)
  let definition
  await module.apply({ tools: { register(value) { definition = value } } }, { baseUrl: mock.url, tenant: 'acme', knowledgeBase: 'docs', metadataKeys: ['source'] })
  assert.equal(definition.name, 'cairn_search')
  await assert.rejects(() => definition.execute({ query: 'q', surprise: true }, { signal: new AbortController().signal, callId: 'call_7' }), /unknown/)
  const value = await definition.execute({ query: 'q', limit: 1 }, { signal: new AbortController().signal, callId: 'call_7' })
  assert.equal(mock.getCallId(), 'call_7')
  assert.deepEqual({ ...value.hits[0].metadata }, { source: 'doc' })
  assert.match(value.hits[0].citation, /^cairn:\/\//u)
  const rendered = definition.output.render({}, value)[0].text
  assert.match(rendered, /UNTRUSTED REFERENCE DATA/u)
  assert.match(rendered, /BEGIN_UNTRUSTED_CAIRN_EVIDENCE/u)
})
