#!/usr/bin/env node
import process from 'node:process'
import { CairnClient, CairnClientError } from '../lib/client.js'

const help = `cairn-dsh-doctor [options]

  --base-url URL   default http://127.0.0.1:8080
  --tenant NAME    default acme
  --kb NAME        default handbook
  --token-env NAME default CAIRN_SERVER_TOKEN
  --timeout-ms N   default 10000
  --query TEXT     optional paid end-to-end search probe
  --json           machine-readable output
`

function parse(argv) {
  const options = { baseUrl: 'http://127.0.0.1:8080', tenant: 'acme', knowledgeBase: 'handbook', tokenEnv: 'CAIRN_SERVER_TOKEN', timeoutMs: 10_000, query: undefined, json: false }
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i]
    if (arg === '--help' || arg === '-h') { process.stdout.write(help); process.exit(0) }
    if (arg === '--json') { options.json = true; continue }
    const value = argv[i + 1]
    if (value === undefined) throw new Error(`${arg} requires a value`)
    i += 1
    if (arg === '--base-url') options.baseUrl = value
    else if (arg === '--tenant') options.tenant = value
    else if (arg === '--kb') options.knowledgeBase = value
    else if (arg === '--token-env') options.tokenEnv = value
    else if (arg === '--timeout-ms') options.timeoutMs = Number(value)
    else if (arg === '--query') options.query = value
    else throw new Error(`unknown option: ${arg}`)
  }
  if (!Number.isSafeInteger(options.timeoutMs) || options.timeoutMs < 1 || options.timeoutMs > 600_000) throw new Error('--timeout-ms must be in 1..=600000')
  return options
}

async function main() {
  const options = parse(process.argv.slice(2))
  const client = new CairnClient({ ...options, retries: 1, maxRetryDelayMs: 1000, maxResponseBytes: 1024 * 1024, maxHits: 1, maxMetadataBytesPerHit: 1024 })
  const signal = AbortSignal.timeout(options.timeoutMs)
  const health = await client.health(signal)
  const head = await client.head(signal)
  const search = options.query === undefined ? null : await client.search({ query: options.query, limit: 1, candidateLimit: 12, filters: {} }, signal)
  const result = {
    ok: true,
    baseUrl: options.baseUrl,
    version: health.version,
    apiVersion: health.apiVersion,
    tenant: head.tenant,
    knowledgeBase: head.knowledgeBase,
    revision: head.revision,
    embeddingProvider: head.embeddingProvider,
    embeddingModel: head.embeddingModel,
    dimension: head.dimension,
    shardCount: head.shardCount,
    hasUqaBundle: head.hasUqaBundle,
    search: search === null ? null : { revision: search.revision, mode: search.mode, hits: search.hits.length },
    tokenConfigured: options.tokenEnv.length > 0 && Boolean(process.env[options.tokenEnv]?.trim()),
  }
  if (options.json) process.stdout.write(`${JSON.stringify(result)}\n`)
  else process.stdout.write([
    'CAIRN ↔ DeepSeek Harness integration is ready.',
    `  Service:   ${result.baseUrl}`,
    `  Version:   ${result.version} (API ${result.apiVersion})`,
    `  Scope:     ${result.tenant}/${result.knowledgeBase}`,
    `  Revision:  ${result.revision}`,
    `  Embedding: ${result.embeddingProvider}/${result.embeddingModel} (${result.dimension}d)`,
    `  UQA warm:  ${result.hasUqaBundle ? 'available' : 'not materialized'}`,
    ...(result.search === null ? [] : [`  Search:    ${result.search.mode}, ${result.search.hits} hit(s)`]),
  ].join('\n') + '\n')
}

try { await main() }
catch (error) {
  const payload = { ok: false, error: error instanceof Error ? error.message : String(error), ...(error instanceof CairnClientError ? { code: error.code, retryable: error.retryable, status: error.status, requestId: error.requestId } : {}) }
  process.stderr.write(process.argv.includes('--json') ? `${JSON.stringify(payload)}\n` : `cairn-dsh-doctor: ${payload.error}\n`)
  process.exitCode = 1
}
