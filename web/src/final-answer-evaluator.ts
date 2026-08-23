import { generateFinalAnswer, type FinalAnswerProvider, type FinalAnswerUsage } from './final-answer.ts'
import type { SessionPacket } from './session-packet.ts'

export interface ReplayCase {
  readonly caseId: string
  readonly question: string
  readonly packet: SessionPacket
  readonly expectedAnswer: string
  readonly expectedCitations: readonly string[]
  readonly provider: FinalAnswerProvider
}

export interface ReplayObservation {
  readonly caseId: string
  readonly repeat: number
  readonly answerCorrect: boolean
  readonly citationGrounded: boolean
  readonly abstention: boolean
  readonly evidenceChars: number
  readonly estimatedInputTokens: number
  readonly providerUsage?: FinalAnswerUsage
  readonly providerElapsedMs: number
  readonly elapsedMs: number
}

export interface ReplaySummary {
  readonly positiveCases: number
  readonly positiveGrounded: number
  readonly negativeCases: number
  readonly negativeAbstentions: number
}

export interface ReplayEvaluation {
  readonly observations: readonly ReplayObservation[]
  readonly summary: ReplaySummary
}

function invalid(message: string): never {
  throw new TypeError(message)
}

function validateCase(value: ReplayCase, index: number): void {
  if (typeof value !== 'object' || value === null) invalid(`cases[${index}] must be an object`)
  if (typeof value.caseId !== 'string' || value.caseId.length === 0) invalid(`cases[${index}].caseId must be non-empty`)
  if (typeof value.question !== 'string') invalid(`cases[${index}].question must be a string`)
  if (typeof value.expectedAnswer !== 'string') invalid(`cases[${index}].expectedAnswer must be a string`)
  if (!Array.isArray(value.expectedCitations) || !value.expectedCitations.every(item => typeof item === 'string')) invalid(`cases[${index}].expectedCitations must be strings`)
  if (typeof value.packet !== 'object' || value.packet === null) invalid(`cases[${index}].packet must be an object`)
  if (typeof value.provider !== 'object' || value.provider === null || typeof value.provider.complete !== 'function') invalid(`cases[${index}].provider is invalid`)
}

export async function evaluateReplayCases(cases: readonly ReplayCase[], repeats: number): Promise<ReplayEvaluation> {
  if (!Array.isArray(cases)) invalid('cases must be an array')
  if (!Number.isInteger(repeats) || repeats < 1) invalid('repeats must be a positive integer')
  cases.forEach(validateCase)

  const observations: ReplayObservation[] = []
  const positiveIds = new Set<string>()
  const negativeIds = new Set<string>()
  let positiveGrounded = 0
  let negativeAbstentions = 0

  for (const replayCase of cases) {
    const positive = replayCase.expectedAnswer !== 'ABSTAIN'
    if (positive) positiveIds.add(replayCase.caseId)
    else negativeIds.add(replayCase.caseId)
    for (let repeat = 1; repeat <= repeats; repeat += 1) {
      const started = performance.now()
      const result = await generateFinalAnswer(replayCase.packet, replayCase.provider, replayCase.question)
      const elapsed = performance.now() - started
      const elapsedMs = Number.isFinite(elapsed) && elapsed >= 0 ? Math.max(elapsed, result.elapsedMs) : result.elapsedMs
      const answerCorrect = result.answer === replayCase.expectedAnswer
      const citationGrounded = replayCase.expectedCitations.length > 0 && result.citations.length === replayCase.expectedCitations.length && result.citations.every((citation, index) => citation === replayCase.expectedCitations[index])
      const abstention = result.answer === 'ABSTAIN'
      observations.push({ caseId: replayCase.caseId, repeat, answerCorrect, citationGrounded, abstention, evidenceChars: result.evidenceText.length, estimatedInputTokens: result.estimatedTokens, ...(result.usage === undefined ? {} : { providerUsage: result.usage }), providerElapsedMs: result.elapsedMs, elapsedMs })
      if (repeat === 1) {
        if (positive && citationGrounded) positiveGrounded += 1
        if (!positive && abstention) negativeAbstentions += 1
      }
    }
  }
  return { observations, summary: { positiveCases: positiveIds.size, positiveGrounded, negativeCases: negativeIds.size, negativeAbstentions } }
}
