import { parseCitation, type ParsedCitation } from './protocol.ts'
import type { SessionPacket, SessionSegment } from './session-packet.ts'

export interface FinalAnswerUsage {
  readonly promptTokens?: number
  readonly completionTokens?: number
  readonly totalTokens?: number
}

export interface FinalAnswerRequest {
  readonly question: string
  readonly evidenceText: string
  readonly citations: readonly string[]
}

export interface FinalAnswerProviderResponse {
  readonly answer: string
  readonly citations: readonly string[]
  readonly usage?: FinalAnswerUsage
}

export interface FinalAnswerProvider {
  complete(request: FinalAnswerRequest): Promise<FinalAnswerProviderResponse>
}

export interface FinalAnswerResult {
  readonly answer: string
  readonly citations: readonly string[]
  readonly evidenceText: string
  readonly usage?: FinalAnswerUsage
  readonly estimatedTokens: number
  readonly elapsedMs: number
}

export class FinalAnswerError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'FinalAnswerError'
  }
}

function evidenceSegments(packet: SessionPacket): SessionSegment[] {
  const seen = new Set<string>()
  const selected: SessionSegment[] = []
  for (const segment of [...packet.segments].sort((a, b) => a.seqStart - b.seqStart || a.id.localeCompare(b.id))) {
    if (seen.has(segment.id) || !Number.isFinite(segment.score) || segment.score <= 0) continue
    const end = segment.seqEnd ?? segment.seqStart
    const overlaps = selected.some(existing => {
      const existingEnd = existing.seqEnd ?? existing.seqStart
      return segment.seqStart <= existingEnd && end >= existing.seqStart
    })
    if (overlaps) continue
    seen.add(segment.id)
    selected.push(segment)
  }
  return selected
}

export async function generateFinalAnswer(packet: SessionPacket, provider: FinalAnswerProvider, question = ''): Promise<FinalAnswerResult> {
  const segments = evidenceSegments(packet)
  let evidenceText = ''
  const emitted: string[] = []
  for (const segment of segments) {
    const addition = evidenceText.length === 0 ? segment.text : `\n${segment.text}`
    if (evidenceText.length + addition.length > 5000) break
    evidenceText += addition
    emitted.push(segment.citation)
  }
  const estimatedTokens = Math.ceil(evidenceText.length / 4)
  if (evidenceText.length === 0) return { answer: 'ABSTAIN', citations: [], evidenceText, estimatedTokens, elapsedMs: 0 }

  const started = performance.now()
  const response = await provider.complete({ question, evidenceText, citations: emitted })
  const elapsed = performance.now() - started
  const elapsedMs = Number.isFinite(elapsed) && elapsed >= 0 ? elapsed : 0
  if (typeof response.answer !== 'string' || response.answer.length === 0) throw new FinalAnswerError('provider answer must be non-empty')
  const emittedSet = new Set(emitted)
  const citations: string[] = []
  const cited = new Set<string>()
  for (const raw of response.citations) {
    const parsed: ParsedCitation | undefined = typeof raw === 'string' ? parseCitation(raw) : undefined
    if (parsed === undefined || parsed.tenant !== packet.tenant || parsed.knowledgeBase !== packet.knowledgeBase || parsed.revision !== packet.revision || !emittedSet.has(raw)) {
      throw new FinalAnswerError('provider citation is invalid or outside emitted evidence')
    }
    if (!cited.has(raw)) { cited.add(raw); citations.push(raw) }
  }
  citations.sort((a, b) => emitted.indexOf(a) - emitted.indexOf(b))
  return {
    answer: response.answer,
    citations,
    evidenceText,
    ...(response.usage === undefined ? {} : { usage: response.usage }),
    estimatedTokens,
    elapsedMs,
  }
}
