import assert from 'node:assert/strict'
import { test } from 'node:test'
import { buildSessionPacket, type SessionPacket } from '../src/session-packet.ts'
import { citation, type CairnSearchHit, type CairnSearchResponse } from '../src/protocol.ts'
import { generateFinalAnswer, type FinalAnswerProvider } from '../src/final-answer.ts'

const SESSION_UID = 'senpi:01a0060a'
const CORPUS = 'a'.repeat(64)

function hit(id: string, seqStart: number, text: string, score = 1): CairnSearchHit {
  return {
    id,
    score,
    posterior: 0.5,
    lexicalEvidence: 1,
    vectorEvidence: 1,
    text,
    metadata: { doc_type: 'session_window', session_uid: SESSION_UID, seq_start: seqStart },
  }
}

function packet(hits: readonly CairnSearchHit[]): SessionPacket {
  const response: CairnSearchResponse = {
    revision: 7,
    embeddingProvider: 'openrouter',
    embeddingModel: 'qwen/qwen3-embedding-8b',
    dimension: 1024,
    corpusSha256: CORPUS,
    mode: 'cold',
    scoreDomain: 'revision_calibrated_log_odds',
    approximate: false,
    hits,
    remoteBytes: 1,
    rangeReads: 1,
  }
  return buildSessionPacket({
    tenant: 'local',
    knowledgeBase: 'sessions',
    sessionUid: SESSION_UID,
    response,
    head: {
      tenant: 'local',
      knowledgeBase: 'sessions',
      revision: 7,
      createdAtUnixMs: 1_700_000_000_000,
      embeddingProvider: 'openrouter',
      embeddingModel: 'qwen/qwen3-embedding-8b',
      dimension: 1024,
      shardCount: 1,
      hasUqaBundle: false,
    },
  })
}

const provider: FinalAnswerProvider = {
  complete: async () => ({
    answer: 'The earlier decision was to keep the bounded evidence path.',
    citations: [
      citation('local', 'sessions', 7, `s:${SESSION_UID}:m:1-2`),
      citation('local', 'sessions', 7, `s:${SESSION_UID}:m:1-2`),
    ],
    usage: { promptTokens: 23, completionTokens: 11, totalTokens: 34 },
  }),
}

test('builds bounded chronological deduplicated evidence and returns audited answer metadata', async () => {
  const first = hit(`s:${SESSION_UID}:m:1-2`, 1, 'first evidence')
  const duplicate = hit(`s:${SESSION_UID}:m:1-2`, 1, 'first evidence')
  const packetValue = packet([hit(`s:${SESSION_UID}:m:3-4`, 3, 'later evidence'), first, duplicate])
  const result = await generateFinalAnswer(packetValue, provider)

  assert.ok(result.evidenceText.length <= 5_000)
  assert.equal(result.evidenceText, 'first evidence\nlater evidence')
  assert.deepEqual(result.citations, [citation('local', 'sessions', 7, `s:${SESSION_UID}:m:1-2`)])
  assert.notEqual(result.answer, '')
  assert.deepEqual(result.usage, { promptTokens: 23, completionTokens: 11, totalTokens: 34 })
  assert.equal(typeof result.estimatedTokens, 'number')
  assert.notEqual(result.estimatedTokens, result.usage.totalTokens)
  assert.ok(Number.isFinite(result.elapsedMs) && result.elapsedMs >= 0)
})

test('does not cite a segment whose text does not fit in the evidence packet', async () => {
  const firstCitation = citation('local', 'sessions', 7, `s:${SESSION_UID}:m:1-2`)
  const laterCitation = citation('local', 'sessions', 7, `s:${SESSION_UID}:m:3-4`)
  let receivedEvidence = ''
  let receivedCitations: readonly string[] = []
  const packetValue = packet([
    hit(`s:${SESSION_UID}:m:1-2`, 1, 'a'.repeat(5_000)),
    hit(`s:${SESSION_UID}:m:3-4`, 3, 'later evidence'),
  ])
  const oversizedPacket: SessionPacket = {
    ...packetValue,
    segments: packetValue.segments.map((segment, index) => index === 0
      ? { ...segment, text: 'a'.repeat(5_000), truncated: false }
      : segment),
  }
  const result = await generateFinalAnswer(oversizedPacket, {
    complete: async request => {
      receivedEvidence = request.evidenceText
      receivedCitations = request.citations
      return { answer: 'answer', citations: [firstCitation] }
    },
  })

  assert.equal(receivedEvidence, 'a'.repeat(5_000))
  assert.deepEqual(receivedCitations, [firstCitation])
  assert.deepEqual(result.citations, [firstCitation])
  assert.equal(receivedCitations.includes(laterCitation), false)
})

test('rejects malformed, cross-scope, cross-revision, and unknown provider citations', async () => {
  const packetValue = packet([hit(`s:${SESSION_UID}:m:1-2`, 1, 'evidence')])
  for (const badCitation of [
    'not-a-citation',
    'cairn://other/sessions/revision/7/chunk/id',
    'cairn://local/sessions/revision/8/chunk/id',
    citation('local', 'sessions', 7, 'unknown'),
  ]) {
    await assert.rejects(
      generateFinalAnswer(packetValue, {
        complete: async () => ({ answer: 'answer', citations: [badCitation], usage: { totalTokens: 1 } }),
      }),
    )
  }
})

test('forwards the question and accepts provider answers without usage', async () => {
  const packetValue = packet([hit(`s:${SESSION_UID}:m:1-2`, 1, 'fixed evidence')])
  const question = 'What was the earlier decision?'
  let receivedQuestion: unknown
  const providerWithoutUsage = {
    complete: async (request: { question?: unknown; evidenceText: string; citations: readonly string[] }) => {
      receivedQuestion = request.question
      return {
        answer: 'The earlier decision was to keep the bounded evidence path.',
        citations: [citation('local', 'sessions', 7, `s:${SESSION_UID}:m:1-2`)],
      }
    },
  } as FinalAnswerProvider
  const generate = generateFinalAnswer as unknown as (
    packet: SessionPacket,
    provider: FinalAnswerProvider,
    question: string,
  ) => Promise<Awaited<ReturnType<typeof generateFinalAnswer>>>

  const result = await generate(packetValue, providerWithoutUsage, question)

  assert.equal(receivedQuestion, question)
  assert.equal(result.usage, undefined)
  assert.equal(typeof result.estimatedTokens, 'number')
  assert.ok(result.estimatedTokens > 0)
})

test('explicitly abstains with zero citations when evidence is empty or negative', async () => {
  const negative = packet([hit(`s:${SESSION_UID}:m:1-2`, 1, 'not relevant', -1)])
  for (const packetValue of [packet([]), negative]) {
    const result = await generateFinalAnswer(packetValue, provider)
    assert.equal(result.answer, 'ABSTAIN')
    assert.deepEqual(result.citations, [])
  }
})
