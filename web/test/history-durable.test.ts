/**
 * Durable history: restart recovery, malformed-line tolerance, bounds and
 * atomicity.
 *
 * These use a real temp directory rather than a mocked `fs`, because the
 * behaviour under test *is* the filesystem interaction — an fs mock that
 * accepted a half-written line would prove nothing about a real crash. Every
 * test is still deterministic: each one gets its own directory, and "restart"
 * is modelled by constructing a second `HistoryStore` over the same path, which
 * is exactly what a process restart does.
 */

import assert from 'node:assert/strict'
import { chmodSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { test } from 'node:test'
import { HistoryStore } from '../src/history.ts'
import {
  FileHistoryPersistence,
  decodeStoredRecord,
  parseHistoryFile,
} from '../src/history-store.ts'
import { loadConfig } from '../src/config.ts'
import { DIGEST, record } from './helpers.ts'

/** Fresh directory per test; `t.after` removes it, so runs never interfere. */
function workspace(t: { after: (fn: () => void) => void }): string {
  const dir = mkdtempSync(join(tmpdir(), 'cairn-history-'))
  t.after(() => rmSync(dir, { force: true, recursive: true }))
  return dir
}

function storeAt(path: string, limit = 100): HistoryStore {
  return new HistoryStore(limit, new FileHistoryPersistence(path))
}

/* ---------- Restart recovery ---------- */

test('history survives a restart', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const first = storeAt(path)
  first.append(record({ queryId: 'q-1', createdAtUnixMs: 1000 }))
  first.append(record({ queryId: 'q-2', createdAtUnixMs: 2000 }))

  // A new store over the same path is what a process restart produces.
  const second = storeAt(path)
  assert.equal(second.size, 2)
  assert.deepEqual(
    second.list().records.map(entry => entry.queryId),
    ['q-2', 'q-1'],
  )
  assert.equal(second.get('q-1')?.request.query, 'refunds')
})

test('a recovered record keeps every field the UI renders', async t => {
  const path = join(workspace(t), 'history.jsonl')

  storeAt(path).append(record({ queryId: 'q-1', toolCallId: 'call-9' }))

  const recovered = storeAt(path).get('q-1')
  assert.ok(recovered !== undefined)
  assert.equal(recovered.sessionId, 's-1')
  assert.equal(recovered.tenant, 'acme')
  assert.equal(recovered.knowledgeBase, 'handbook')
  assert.equal(recovered.toolCallId, 'call-9')
  assert.equal(recovered.latencyMs, 120)
  assert.deepEqual(recovered.request, {
    query: 'refunds',
    limit: 20,
    candidateLimit: 200,
    filters: {},
  })
  assert.equal(recovered.outcome.status, 'ok')
  assert.equal((recovered.outcome as { corpusSha256: string }).corpusSha256, DIGEST)
})

test('failed searches survive a restart as first-class records', async t => {
  const path = join(workspace(t), 'history.jsonl')

  storeAt(path).append(
    record({
      queryId: 'q-err',
      outcome: { status: 'error', code: 'EMBEDDING_UNAVAILABLE', message: 'no key', retryable: false },
    }),
  )

  const restarted = storeAt(path)
  assert.equal(restarted.get('q-err')?.outcome.status, 'error')
  assert.equal(restarted.sessions()[0]?.errorCount, 1)
})

test('an absent history file is a clean first boot, not an error', async t => {
  const path = join(workspace(t), 'nested', 'deeper', 'history.jsonl')
  // The parent directories do not exist yet.
  const store = storeAt(path)
  assert.equal(store.size, 0)
  store.append(record({ queryId: 'q-1' }))
  assert.equal(storeAt(path).size, 1)
})

test('an empty file recovers as an empty history', async t => {
  const path = join(workspace(t), 'history.jsonl')
  writeFileSync(path, '', 'utf8')
  assert.equal(storeAt(path).size, 0)
})

/* ---------- Malformed lines ---------- */

test('a truncated final line from a crash is skipped, not fatal', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const store = storeAt(path)
  store.append(record({ queryId: 'q-1', createdAtUnixMs: 1000 }))
  store.append(record({ queryId: 'q-2', createdAtUnixMs: 2000 }))

  // Simulate a crash mid-append: a partial JSON object with no newline.
  const contents = readFileSync(path, 'utf8')
  writeFileSync(path, `${contents}{"queryId":"q-3","sessionId":"s-1","ten`, 'utf8')

  const restarted = storeAt(path)
  assert.equal(restarted.size, 2, 'the two complete records must survive')
  assert.deepEqual(
    restarted.list().records.map(entry => entry.queryId),
    ['q-2', 'q-1'],
  )
})

test('structurally invalid records are rejected even when they are valid JSON', async t => {
  const path = join(workspace(t), 'history.jsonl')
  const good = JSON.stringify(record({ queryId: 'good' }))
  writeFileSync(
    path,
    [
      good,
      '{}',
      'null',
      '[]',
      '"a string"',
      '42',
      JSON.stringify({ queryId: 'no-outcome', sessionId: 's', tenant: 't', knowledgeBase: 'k' }),
      // Right shape, wrong types: a hand-edited or older-format file.
      JSON.stringify({ ...record({ queryId: 'bad-latency' }), latencyMs: 'fast' }),
      JSON.stringify({ ...record({ queryId: 'bad-outcome' }), outcome: { status: 'maybe' } }),
      '',
      '   ',
    ].join('\n'),
    'utf8',
  )

  const store = storeAt(path)
  assert.equal(store.size, 1)
  assert.ok(store.get('good'))
  assert.equal(store.get('bad-latency'), undefined)
  assert.equal(store.get('bad-outcome'), undefined)
})

test('a damaged file is repaired once at boot rather than re-parsed forever', async t => {
  const path = join(workspace(t), 'history.jsonl')
  const good = JSON.stringify(record({ queryId: 'good' }))
  writeFileSync(path, `${good}\n{"broken":\n`, 'utf8')

  const persistence = new FileHistoryPersistence(path)
  const recovered = persistence.load()
  assert.equal(recovered.length, 1)
  assert.equal(persistence.lastReport.skipped, 1)

  // The rewrite happened during load, so a second load sees a clean file.
  const second = new FileHistoryPersistence(path)
  second.load()
  assert.equal(second.lastReport.skipped, 0)
  assert.equal(second.lastReport.recovered, 1)
})

test('repair failure does not turn a corrupt line into a boot failure', async t => {
  const dir = workspace(t)
  const path = join(dir, 'history.jsonl')
  const good = JSON.stringify(record({ queryId: 'good' }))
  writeFileSync(path, `${good}\n{"broken":\n`, 'utf8')
  chmodSync(dir, 0o555)

  const persistence = new FileHistoryPersistence(path)
  const recovered = persistence.load()
  chmodSync(dir, 0o755)
  assert.equal(recovered.length, 1)
  assert.equal(persistence.lastReport.skipped, 1)
  assert.equal(persistence.lastReport.repairFailed, true)
})

test('parseHistoryFile reports what it dropped instead of failing silently', () => {
  const good = JSON.stringify(record({ queryId: 'good' }))
  const parsed = parseHistoryFile(`${good}\nnot json\n\n{"also":"invalid"}\n`)
  assert.equal(parsed.records.length, 1)
  assert.equal(parsed.skipped, 2)
})

test('decodeStoredRecord accepts a real record and rejects near-misses', () => {
  const valid = JSON.parse(JSON.stringify(record({ queryId: 'q' }))) as unknown
  assert.ok(decodeStoredRecord(valid) !== undefined)

  assert.equal(decodeStoredRecord(undefined), undefined)
  assert.equal(decodeStoredRecord({ ...(valid as object), queryId: '' }), undefined)
  assert.equal(decodeStoredRecord({ ...(valid as object), createdAtUnixMs: -1 }), undefined)
  assert.equal(decodeStoredRecord({ ...(valid as object), request: null }), undefined)
})

/* ---------- Bounds ---------- */

test('retention is bounded on disk, not only in memory', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const store = storeAt(path, 3)
  for (let index = 0; index < 10; index += 1) {
    store.append(record({ queryId: `q-${index}`, createdAtUnixMs: 1000 + index }))
  }

  assert.equal(store.size, 3)
  // The file must not keep the evicted records: it is a rolling window.
  const lines = readFileSync(path, 'utf8').trim().split('\n')
  assert.equal(lines.length, 3)
  assert.deepEqual(
    storeAt(path, 3).list().records.map(entry => entry.queryId),
    ['q-9', 'q-8', 'q-7'],
  )
})

test('a file larger than the configured limit is trimmed at recovery', async t => {
  const path = join(workspace(t), 'history.jsonl')

  // Written by a server configured with a larger limit.
  const big = storeAt(path, 100)
  for (let index = 0; index < 10; index += 1) {
    big.append(record({ queryId: `q-${index}`, createdAtUnixMs: 1000 + index }))
  }

  // Restarting with a smaller limit must not exceed it.
  const small = storeAt(path, 4)
  assert.equal(small.size, 4)
  assert.deepEqual(
    small.list().records.map(entry => entry.queryId),
    ['q-9', 'q-8', 'q-7', 'q-6'],
  )
  // And the trim is persisted, not just applied in memory.
  assert.equal(readFileSync(path, 'utf8').trim().split('\n').length, 4)
})

/* ---------- Import ---------- */

test('imported sessions persist and stay idempotent across a restart', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const incoming = [
    record({ queryId: 'i-1', sessionId: 'agent-a', createdAtUnixMs: 500 }),
    record({ queryId: 'i-2', sessionId: 'agent-a', createdAtUnixMs: 600 }),
  ]
  const first = storeAt(path)
  assert.deepEqual(first.importSessions(incoming), { imported: 2, skipped: 0 })

  const restarted = storeAt(path)
  assert.equal(restarted.size, 2)
  assert.equal(restarted.sessions().find(entry => entry.sessionId === 'agent-a')?.imported, true)
  assert.ok(
    readFileSync(path, 'utf8')
      .trim()
      .split('\n')
      .every(line => (JSON.parse(line) as { imported?: boolean }).imported === true),
  )
  // Re-importing after a restart must still deduplicate on queryId.
  assert.deepEqual(restarted.importSessions(incoming), { imported: 0, skipped: 2 })
  assert.equal(restarted.size, 2)
  assert.equal(readFileSync(path, 'utf8').trim().split('\n').length, 2)
})

test('an out-of-band import is persisted in chronological order', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const store = storeAt(path)
  store.append(record({ queryId: 'live-1', createdAtUnixMs: 3000 }))
  store.importSessions([record({ queryId: 'old-1', sessionId: 'agent-a', createdAtUnixMs: 1000 })])

  const lines = readFileSync(path, 'utf8').trim().split('\n')
  const ids = lines.map(line => (JSON.parse(line) as { queryId: string }).queryId)
  assert.deepEqual(ids, ['old-1', 'live-1'], 'the file must hold chronological order')
  assert.deepEqual(
    storeAt(path).list().records.map(entry => entry.queryId),
    ['live-1', 'old-1'],
  )
})

/* ---------- Atomicity ---------- */

test('a rewrite leaves no temp file behind', async t => {
  const dir = workspace(t)
  const path = join(dir, 'history.jsonl')

  const store = storeAt(path, 2)
  for (let index = 0; index < 5; index += 1) {
    store.append(record({ queryId: `q-${index}`, createdAtUnixMs: 1000 + index }))
  }

  // Eviction forces rewrites; none of them may leak a partial sibling file.
  assert.deepEqual(readdirSync(dir), ['history.jsonl'])
})

test('the published file is always complete, never a partial line', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const store = storeAt(path, 3)
  for (let index = 0; index < 8; index += 1) {
    store.append(record({ queryId: `q-${index}`, createdAtUnixMs: 1000 + index }))
    // After every write the file must parse cleanly with nothing dropped.
    const parsed = parseHistoryFile(readFileSync(path, 'utf8'))
    assert.equal(parsed.skipped, 0, `file was unparseable after append ${index}`)
    assert.ok(parsed.records.length <= 3)
  }
})

/* ---------- In-memory default and token containment ---------- */

test('history is in memory only when no path is configured', () => {
  const config = loadConfig({
    CAIRN_BASE_URL: 'http://127.0.0.1:8080',
    CAIRN_SERVER_TOKEN: 'test-token-1234567890',
  } as NodeJS.ProcessEnv)
  assert.equal(config.historyPath, undefined)

  // Constructing without persistence must not touch the filesystem at all.
  const store = new HistoryStore(config.historyLimit)
  store.append(record({ queryId: 'q-1' }))
  assert.equal(store.size, 1)
})

test('a blank history path is treated as unset, not as a file named ""', () => {
  for (const raw of ['', '   ']) {
    const config = loadConfig({
      CAIRN_BASE_URL: 'http://127.0.0.1:8080',
      CAIRN_WEB_HISTORY_PATH: raw,
    } as NodeJS.ProcessEnv)
    assert.equal(config.historyPath, undefined)
  }
})

test('a configured path is read back verbatim', () => {
  const config = loadConfig({
    CAIRN_BASE_URL: 'http://127.0.0.1:8080',
    CAIRN_WEB_HISTORY_PATH: '  /tmp/cairn/history.jsonl  ',
  } as NodeJS.ProcessEnv)
  assert.equal(config.historyPath, '/tmp/cairn/history.jsonl')
})

test('the persisted file never contains the bearer token', async t => {
  const path = join(workspace(t), 'history.jsonl')

  const store = storeAt(path)
  store.append(record({ queryId: 'q-1' }))
  store.append(
    record({
      queryId: 'q-2',
      outcome: { status: 'error', code: 'UNAUTHORIZED', message: 'a valid bearer token is required', retryable: false },
    }),
  )

  // The upstream error message mentions tokens; the token value must not be
  // anywhere in the file, and neither must an auth header.
  const contents = readFileSync(path, 'utf8')
  assert.ok(!contents.includes('test-token-1234567890'))
  assert.ok(!/authorization/iu.test(contents))
})
