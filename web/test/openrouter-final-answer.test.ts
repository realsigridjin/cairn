import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createOpenRouterFinalAnswerProvider } from '../src/openrouter-final-answer.ts'

const MODEL = 'openai/gpt-4o-mini'
const API_KEY = 'fixed-test-secret'
const QUESTION = 'What decision was recorded?'
const EVIDENCE = 'The team recorded the decision to ship on Friday.'
const CITATIONS = ['cairn://local/sessions/revision/7/chunk/s:m:1-2']
const DEFAULT_ENDPOINT = 'https://openrouter.ai/api/v1/chat/completions'

type FetchCall = { url: string; init: RequestInit }

function fakeFetch(body: unknown, status = 200, calls: FetchCall[] = []): typeof fetch {
  return async (input, init = {}) => {
    calls.push({ url: String(input), init })
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    })
  }
}

function requestBody(call: FetchCall): { model: string; stream: boolean; messages: Array<{ role: string; content: string }> } {
  return JSON.parse(String(call.init.body)) as { model: string; stream: boolean; messages: Array<{ role: string; content: string }> }
}

test('sends one explicit non-streaming OpenRouter request and maps the JSON answer envelope and usage', async () => {
  const calls: FetchCall[] = []
  const fetch = fakeFetch({
    choices: [{ message: { content: JSON.stringify({ answer: 'Ship on Friday.', citations: CITATIONS }) } }],
    usage: { prompt_tokens: 23, completion_tokens: 11, total_tokens: 34 },
  }, 200, calls)
  const provider = createOpenRouterFinalAnswerProvider({ model: MODEL, apiKey: API_KEY, fetch })

  const result = await provider.complete({ question: QUESTION, evidenceText: EVIDENCE, citations: CITATIONS })

  assert.equal(calls.length, 1)
  assert.equal(calls[0]?.url, DEFAULT_ENDPOINT)
  assert.equal(calls[0]?.init.method, 'POST')
  assert.deepEqual(calls[0]?.init.headers, { authorization: `Bearer ${API_KEY}`, 'content-type': 'application/json' })
  const body = requestBody(calls[0] as FetchCall)
  assert.equal(body.model, MODEL)
  assert.equal(body.stream, false)
  assert.equal(body.messages.length, 2)
  assert.equal(body.messages[0]?.role, 'system')
  assert.match(body.messages[0]?.content ?? '', /untrusted/i)
  assert.match(body.messages[0]?.content ?? '', /evidence/i)
  assert.equal(body.messages[1]?.role, 'user')
  assert.match(body.messages[1]?.content ?? '', new RegExp(QUESTION))
  assert.match(body.messages[1]?.content ?? '', new RegExp(EVIDENCE))
  assert.match(body.messages[1]?.content ?? '', new RegExp(CITATIONS[0] as string))
  assert.doesNotMatch(calls[0]?.url ?? '', new RegExp(API_KEY))
  assert.doesNotMatch(String(calls[0]?.init.body), new RegExp(API_KEY))
  assert.deepEqual(result, {
    answer: 'Ship on Friday.',
    citations: CITATIONS,
    usage: { promptTokens: 23, completionTokens: 11, totalTokens: 34 },
  })
  assert.doesNotMatch(JSON.stringify(result), new RegExp(API_KEY))
})

test('sends the JSON protocol contract in the provider request', async () => {
  const calls: FetchCall[] = []
  const provider = createOpenRouterFinalAnswerProvider({
    model: MODEL,
    apiKey: API_KEY,
    fetch: fakeFetch({ choices: [{ message: { content: JSON.stringify({ answer: 'ABSTAIN', citations: [] }) } }] }, 200, calls),
  })

  await provider.complete({ question: QUESTION, evidenceText: EVIDENCE, citations: CITATIONS })

  const body = requestBody(calls[0] as FetchCall)
  const system = body.messages[0]?.content ?? ''
  assert.match(system, /FINAL_ANSWER_JSON_START/)
  assert.match(system, /FINAL_ANSWER_JSON_END/)
  assert.match(system, /exactly one JSON object/)
  assert.match(system, /answer.*citations/)
  assert.match(system, /exact string from the supplied Citations list/)
  assert.match(system, /ABSTAIN.*citations.*\[\]/)
  assert.match(system, /sentinel tokens.*must not appear/)
})

test('allows a fixed endpoint and missing usage', async () => {
  const calls: FetchCall[] = []
  const provider = createOpenRouterFinalAnswerProvider({
    model: MODEL,
    apiKey: API_KEY,
    endpoint: 'https://router.test/chat',
    fetch: fakeFetch({ choices: [{ message: { content: JSON.stringify({ answer: 'Answer', citations: [] }) } }] }, 200, calls),
  })

  const result = await provider.complete({ question: QUESTION, evidenceText: EVIDENCE, citations: [] })

  assert.equal(calls[0]?.url, 'https://router.test/chat')
  assert.equal(result.usage, undefined)
})

test('rejects insecure or ambiguous endpoints at construction without fetching', () => {
  const calls: FetchCall[] = []
  const invalidEndpoints = [
    'ftp://localhost/chat',
    'http://router.test/chat',
    'https://user:password@router.test/chat',
    'https://router.test/chat?mode=chat',
    'https://router.test/chat#fragment',
  ]

  for (const endpoint of invalidEndpoints) {
    assert.throws(() => createOpenRouterFinalAnswerProvider({
      model: MODEL,
      apiKey: API_KEY,
      endpoint,
      fetch: fakeFetch({}, 200, calls),
    }), `expected endpoint to be rejected: ${endpoint}`)
  }

  assert.equal(calls.length, 0)
})

test('rejects malformed completion envelopes and non-2xx responses', async () => {
  const request = { question: QUESTION, evidenceText: EVIDENCE, citations: CITATIONS }
  await assert.rejects(
    createOpenRouterFinalAnswerProvider({ model: MODEL, apiKey: API_KEY, fetch: fakeFetch({ choices: [] }) }).complete(request),
  )
  await assert.rejects(
    createOpenRouterFinalAnswerProvider({ model: MODEL, apiKey: API_KEY, fetch: fakeFetch({ error: 'bad' }, 401) }).complete(request),
  )
})
