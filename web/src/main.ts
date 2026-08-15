/**
 * Production entrypoint. `node src/main.ts` — Node 22.19+/24+ runs TypeScript
 * directly via type stripping, so there is no build step and no bundler.
 */

import { CairnClient } from './cairn-client.ts'
import { loadConfig } from './config.ts'
import { HistoryStore } from './history.ts'
import { FileHistoryPersistence } from './history-store.ts'
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

  // Unset CAIRN_WEB_HISTORY_PATH keeps history purely in memory.
  const persistence =
    config.historyPath === undefined ? undefined : new FileHistoryPersistence(config.historyPath)
  const history = new HistoryStore(config.historyLimit, persistence)

  const app = createApp({ config, client, history })

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
      `  sessions : ${config.sessionsScope === undefined ? 'no session memory KB (set CAIRN_WEB_SESSIONS_SCOPE=tenant/kb)' : `${config.sessionsScope.tenant}/${config.sessionsScope.knowledgeBase}`}`,
      `  history  : ${
        persistence === undefined
          ? `in memory only (max ${config.historyLimit}; set CAIRN_WEB_HISTORY_PATH to persist)`
          : `${persistence.path} (max ${config.historyLimit}; recovered ${persistence.lastReport.recovered}${persistence.lastReport.skipped > 0 ? `, skipped ${persistence.lastReport.skipped} unreadable line(s)` : ''}${persistence.lastReport.repairFailed ? ', repair unavailable' : ''})`
      }`,
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
