import assert from 'node:assert/strict'
import { test } from 'node:test'
import { loadConfig, parseScopes } from '../src/config.ts'
import { normalizeBaseUrl, sanitizeCorrelationId } from '../src/cairn-client.ts'

test('parseScopes reads tenant/kb pairs and the fenced marker', () => {
  const scopes = parseScopes('acme/handbook, acme/secrets!fenced')
  assert.deepEqual(scopes, [
    { tenant: 'acme', knowledgeBase: 'handbook', fenced: false },
    { tenant: 'acme', knowledgeBase: 'secrets', fenced: true },
  ])
})

test('parseScopes deduplicates and tolerates empty input', () => {
  assert.deepEqual(parseScopes(undefined), [])
  assert.deepEqual(parseScopes('  '), [])
  assert.equal(parseScopes('a/b,a/b').length, 1)
})

test('parseScopes rejects unsafe scope components', () => {
  assert.throws(() => parseScopes('acme'), /expected tenant\/kb/u)
  assert.throws(() => parseScopes('acme/../etc'), /invalid scope/u)
  assert.throws(() => parseScopes('acme/..'), /unsafe tenant or knowledge base/u)
  assert.throws(() => parseScopes('-bad/kb'), /unsafe tenant or knowledge base/u)
})

test('loadConfig picks the first unfenced scope as the default', () => {
  const config = loadConfig({
    CAIRN_WEB_SCOPES: 'acme/secrets!fenced,acme/handbook',
    CAIRN_SERVER_TOKEN: 'token-1234567890abcd',
  } as NodeJS.ProcessEnv)
  assert.deepEqual(config.defaultScope, {
    tenant: 'acme',
    knowledgeBase: 'handbook',
    fenced: false,
  })
})

test('loadConfig keeps the token in config and never in the scope list', () => {
  const config = loadConfig({
    CAIRN_SERVER_TOKEN: '  token-1234567890abcd  ',
    CAIRN_WEB_SCOPES: 'acme/handbook',
  } as NodeJS.ProcessEnv)
  assert.equal(config.token, 'token-1234567890abcd')
  assert.ok(!JSON.stringify(config.scopes).includes('token'))
})

test('loadConfig rejects a non-loopback plain-HTTP upstream at boot', () => {
  assert.throws(
    () => loadConfig({ CAIRN_BASE_URL: 'http://cairn.example.com' } as NodeJS.ProcessEnv),
    /must use HTTPS/u,
  )
})

test('loadConfig rejects an out-of-range port', () => {
  assert.throws(
    () => loadConfig({ CAIRN_WEB_PORT: '70000' } as NodeJS.ProcessEnv),
    /must be <= 65535/u,
  )
})

test('normalizeBaseUrl allows loopback HTTP and rejects credentials', () => {
  assert.equal(normalizeBaseUrl('http://127.0.0.1:8080').toString(), 'http://127.0.0.1:8080/')
  assert.equal(normalizeBaseUrl('https://cairn.example.com').toString(), 'https://cairn.example.com/')
  assert.throws(() => normalizeBaseUrl('https://user:pw@cairn.example.com'), /credentials/u)
  assert.throws(() => normalizeBaseUrl('https://cairn.example.com?x=1'), /query string/u)
})

test('sanitizeCorrelationId drops values the CAIRN server would silently discard', () => {
  assert.equal(sanitizeCorrelationId('req-1:abc.def_ghi'), 'req-1:abc.def_ghi')
  assert.equal(sanitizeCorrelationId('req with space'), undefined)
  assert.equal(sanitizeCorrelationId('x'.repeat(257)), undefined)
  assert.equal(sanitizeCorrelationId(''), undefined)
  assert.equal(sanitizeCorrelationId(undefined), undefined)
})
