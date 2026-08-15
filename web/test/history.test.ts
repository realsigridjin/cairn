import assert from 'node:assert/strict'
import { test } from 'node:test'
import { HistoryStore } from '../src/history.ts'
import { DIGEST, record } from './helpers.ts'

test('append stores records and list returns newest first', () => {
  const store = new HistoryStore(10)
  store.append(record({ queryId: 'q-1', createdAtUnixMs: 1000 }))
  store.append(record({ queryId: 'q-2', createdAtUnixMs: 2000 }))
  const page = store.list()
  assert.deepEqual(
    page.records.map(entry => entry.queryId),
    ['q-2', 'q-1'],
  )
  assert.equal(page.total, 2)
})

test('bounded retention drops the oldest record', () => {
  const store = new HistoryStore(2)
  store.append(record({ queryId: 'q-1', createdAtUnixMs: 1 }))
  store.append(record({ queryId: 'q-2', createdAtUnixMs: 2 }))
  store.append(record({ queryId: 'q-3', createdAtUnixMs: 3 }))
  assert.equal(store.size, 2)
  assert.equal(store.get('q-1'), undefined)
  assert.ok(store.get('q-3'))
})

test('list filters by scope and session', () => {
  const store = new HistoryStore(10)
  store.append(record({ queryId: 'q-1', tenant: 'acme', knowledgeBase: 'handbook' }))
  store.append(record({ queryId: 'q-2', tenant: 'other', knowledgeBase: 'handbook' }))
  store.append(record({ queryId: 'q-3', sessionId: 's-2' }))
  assert.equal(store.list({ tenant: 'acme' }).total, 2)
  assert.equal(store.list({ tenant: 'other' }).total, 1)
  assert.equal(store.list({ sessionId: 's-2' }).total, 1)
})

test('cursor paging walks the full set without repeats or gaps', () => {
  const store = new HistoryStore(10)
  for (let index = 0; index < 5; index += 1) {
    store.append(record({ queryId: `q-${index}`, createdAtUnixMs: 1000 + index }))
  }
  const seen: string[] = []
  let cursor: string | undefined
  for (let guard = 0; guard < 10; guard += 1) {
    const page = store.list({ pageSize: 2, ...(cursor === undefined ? {} : { cursor }) })
    seen.push(...page.records.map(entry => entry.queryId))
    if (page.nextCursor === undefined) break
    cursor = page.nextCursor
  }
  assert.deepEqual(seen, ['q-4', 'q-3', 'q-2', 'q-1', 'q-0'])
  assert.equal(new Set(seen).size, seen.length)
})

test('failed searches are first-class history entries', () => {
  const store = new HistoryStore(10)
  store.append(
    record({
      queryId: 'q-err',
      outcome: {
        status: 'error',
        code: 'EMBEDDING_UNAVAILABLE',
        message: 'no key',
        retryable: false,
      },
    }),
  )
  const stored = store.get('q-err')
  assert.equal(stored?.outcome.status, 'error')
  const sessions = store.sessions()
  assert.equal(sessions[0]?.errorCount, 1)
  assert.equal(sessions[0]?.searchCount, 1)
})

test('sessions summarise revisions, digests and error counts', () => {
  const store = new HistoryStore(10)
  store.append(record({ queryId: 'q-1', createdAtUnixMs: 1000 }))
  store.append(record({ queryId: 'q-2', createdAtUnixMs: 2000 }))
  const [session] = store.sessions()
  assert.equal(session?.sessionId, 's-1')
  assert.equal(session?.searchCount, 2)
  assert.equal(session?.errorCount, 0)
  assert.deepEqual(session?.revisions, [42])
  assert.deepEqual(session?.corpusDigests, [DIGEST])
  assert.equal(session?.startedAtUnixMs, 1000)
  assert.equal(session?.updatedAtUnixMs, 2000)
  assert.equal(session?.imported, false)
})

test('importSessions is idempotent and marks imported sessions', () => {
  const store = new HistoryStore(10)
  const incoming = [
    record({ queryId: 'i-1', sessionId: 'agent-a', createdAtUnixMs: 500 }),
    record({ queryId: 'i-2', sessionId: 'agent-a', createdAtUnixMs: 600 }),
  ]
  const first = store.importSessions(incoming)
  assert.deepEqual(first, { imported: 2, skipped: 0 })

  const second = store.importSessions(incoming)
  assert.deepEqual(second, { imported: 0, skipped: 2 })
  assert.equal(store.size, 2)

  const session = store.sessions().find(entry => entry.sessionId === 'agent-a')
  assert.equal(session?.imported, true)
})

test('imported records are merged in chronological order', () => {
  const store = new HistoryStore(10)
  store.append(record({ queryId: 'live-1', createdAtUnixMs: 3000 }))
  store.importSessions([record({ queryId: 'old-1', sessionId: 'agent-a', createdAtUnixMs: 1000 })])
  assert.deepEqual(
    store.list().records.map(entry => entry.queryId),
    ['live-1', 'old-1'],
  )
})

test('history limit must be a positive integer', () => {
  assert.throws(() => new HistoryStore(0), /history limit must be >= 1/u)
})
