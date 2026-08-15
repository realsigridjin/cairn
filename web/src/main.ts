/**
 * Production entrypoint. `node src/main.ts` — Node 22.19+/24+ runs TypeScript
 * directly via type stripping, so there is no build step and no bundler.
 */

import { CairnClient } from './cairn-client.ts'
import { loadConfig } from './config.ts'
import { HistoryStore } from './history.ts'
import { createApp } from './server.ts'

async function main(): Promise<void> {
  const config = loadConfig()

  const client = new CairnClient({
    baseUrl: config.cairnBaseUrl,
    token: config.token,
    timeoutMs: config.timeoutMs,
    retries: config.retries,
    maxRetryDelayMs: config.maxRetryDelayMs,
    maxResponseBytes: config.maxResponseBytes,
    maxMetadataBytesPerHit: config.maxMetadataBytesPerHit,
  })

  const app = createApp({
    config,
    client,
    history: new HistoryStore(config.historyLimit),
  })

  const server = await app.listen(config.port, config.host)
  const address = server.address()
  const bound =
    address !== null && typeof address === 'object'
      ? `http://${address.family === 'IPv6' ? `[${address.address}]` : address.address}:${address.port}`
      : `http://${config.host}:${config.port}`

  process.stdout.write(
    [
      `CAIRN Web listening on ${bound}`,
      `  upstream : ${config.cairnBaseUrl}`,
      `  token    : ${config.token.length > 0 ? 'configured (server-side, never sent to the browser)' : 'not configured'}`,
      `  scopes   : ${config.scopes.length === 0 ? 'none (set CAIRN_WEB_SCOPES=tenant/kb)' : config.scopes.map(scope => `${scope.tenant}/${scope.knowledgeBase}${scope.fenced ? ' (fenced)' : ''}`).join(', ')}`,
      '',
    ].join('\n'),
  )

  const shutdown = (): void => {
    server.close(() => process.exit(0))
  }
  process.on('SIGINT', shutdown)
  process.on('SIGTERM', shutdown)
}

main().catch((error: unknown) => {
  process.stderr.write(`CAIRN Web failed to start: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
