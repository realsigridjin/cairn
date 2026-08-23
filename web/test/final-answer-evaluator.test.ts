import assert from 'node:assert/strict'
import { test } from 'node:test'
import { buildSessionPacket, type SessionPacket } from '../src/session-packet.ts'
import { citation, type CairnSearchHit, type CairnSearchResponse } from '../src/protocol.ts'
import type { FinalAnswerProvider } from '../src/final-answer.ts'
import { evaluateReplayCases, type ReplayCase } from '../src/final-answer-evaluator.ts'

const TENANT = 'local'
const KNOWLEDGE_BASE = 'sessions'
const REVISION = 7
const SESSION_UID = 'senpi:replay-evaluator'
const CORPUS = 'b'.repeat(64)
const EXPECTED_CITATION = citation(TENANT, KNOWLEDGE_BASE, REVISION, `s:${SESSION_UID}:m:1-2`)
const EXPECTED_ANSWER = 'The bounded evidence path was retained.'

function packet(caseNumber: number, relevant: boolean): SessionPacket {
  const id = `s:${SESSION_UID}:m:1-2`
  const hit: CairnSearchHit = {
    id,
    score: relevant ? 1 : -1,
    posterior: 0.5,
    lexicalEvidence: 1,
    vectorEvidence: 1,
    text: relevant ? `fixed evidence for replay case ${caseNumber}` : 'irrelevant fixed evidence',
    metadata: { doc_type: 'session_window', session_uid: SESSION_UID, seq_start: 1, seq_end: 2 },
  }
  const response: CairnSearchResponse = {
    revision: REVISION,
    embeddingProvider: 'openrouter',
    embeddingModel: 'qwen/qwen3-embedding-8b',
    dimension: 1024,
    corpusSha256: CORPUS,
    mode: 'cold',
    scoreDomain: 'revision_calibrated_log_odds',
    approximate: false,
    hits: relevant ? [hit] : [],
    remoteBytes: 1,
    rangeReads: 1,
  }
  return buildSessionPacket({
    tenant: TENANT,
    knowledgeBase: KNOWLEDGE_BASE,
    sessionUid: SESSION_UID,
    response,
    head: {
      tenant: TENANT,
      knowledgeBase: KNOWLEDGE_BASE,
      revision: REVISION,
      createdAtUnixMs: 1_700_000_000_000,
      embeddingProvider: 'openrouter',
      embeddingModel: 'qwen/qwen3-embedding-8b',
      dimension: 1024,
      shardCount: 1,
      hasUqaBundle: false,
    },
  })
}

function provider(answer: string, cited: boolean, withUsage: boolean): FinalAnswerProvider {
  return {
    complete: async () => ({
      answer,
      citations: cited ? [EXPECTED_CITATION] : [],
      ...(withUsage ? { usage: { promptTokens: 19, completionTokens: 8, totalTokens: 27 } } : {}),
    }),
  }
}

const cases: readonly ReplayCase[] = [
  ...Array.from({ length: 6 }, (_, index) => ({
    caseId: `positive-${index + 1}`,
    question: 'What was retained?',
    packet: packet(index + 1, true),
    expectedAnswer: EXPECTED_ANSWER,
    expectedCitations: [EXPECTED_CITATION],
    provider: provider(EXPECTED_ANSWER, true, index % 2 === 0),
  })),
  ...Array.from({ length: 2 }, (_, index) => ({
    caseId: `negative-${index + 1}`,
    question: 'What was retained?',
    packet: packet(index + 7, false),
    expectedAnswer: 'ABSTAIN',
    expectedCitations: [],
    provider: provider('ABSTAIN', false, false),
  })),
]

test('replays fixed answer cases with deterministic audit observations', async () => {
  const result = await evaluateReplayCases(cases, 3)

  assert.equal(result.observations.length, 24)
  assert.equal(result.summary.positiveCases, 6)
  assert.equal(result.summary.positiveGrounded, 6)
  assert.equal(result.summary.negativeCases, 2)
  assert.equal(result.summary.negativeAbstentions, 2)

  assert.deepEqual(
    result.observations.map(({ caseId, repeat }) => [caseId, repeat]),
    cases.flatMap(({ caseId }) => [1, 2, 3].map(repeat => [caseId, repeat])),
  )
  for (const observation of result.observations) {
    assert.equal(typeof observation.answerCorrect, 'boolean')
    assert.equal(typeof observation.citationGrounded, 'boolean')
    assert.equal(typeof observation.abstention, 'boolean')
    assert.ok(observation.evidenceChars >= 0 && observation.evidenceChars <= 5_000)
    assert.ok(Number.isInteger(observation.estimatedInputTokens) && observation.estimatedInputTokens >= 0)
    assert.ok(observation.providerUsage === undefined || observation.providerUsage === null || typeof observation.providerUsage === 'object')
    assert.ok(Number.isFinite(observation.providerElapsedMs) && observation.providerElapsedMs >= 0)
    assert.ok(Number.isFinite(observation.elapsedMs) && observation.elapsedMs >= observation.providerElapsedMs)
  }

  const positives = result.observations.filter(observation => observation.caseId.startsWith('positive-'))
  const negatives = result.observations.filter(observation => observation.caseId.startsWith('negative-'))
  assert.ok(positives.every(observation => observation.answerCorrect && observation.citationGrounded && !observation.abstention))
  assert.ok(negatives.every(observation => observation.abstention && observation.answerCorrect && !observation.citationGrounded))

  const stable = result.observations.map(({ providerElapsedMs: _provider, elapsedMs: _elapsed, ...observation }) => observation)
  const rerun = await evaluateReplayCases(cases, 3)
  assert.deepEqual(rerun.observations.map(({ providerElapsedMs: _provider, elapsedMs: _elapsed, ...observation }) => observation), stable)
})
