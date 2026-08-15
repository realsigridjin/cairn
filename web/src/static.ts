/**
 * Static asset serving from `web/static`. Allowlisted by extension and
 * resolved against the static root, so a traversal attempt cannot escape.
 */

import { readFile } from 'node:fs/promises'
import { dirname, join, normalize, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import type { ServerResponse } from 'node:http'

const STATIC_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'static')

const CONTENT_TYPES: Readonly<Record<string, string>> = {
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.woff2': 'font/woff2',
  '.png': 'image/png',
  '.json': 'application/json; charset=utf-8',
}

export function resolveStaticPath(relative: string): string | undefined {
  if (relative.length === 0 || relative.includes('\0')) return undefined
  let decoded: string
  try {
    decoded = decodeURIComponent(relative)
  } catch {
    return undefined
  }
  const normalized = normalize(decoded)
  if (normalized.startsWith('..') || normalized.startsWith(sep)) return undefined
  const full = resolve(join(STATIC_ROOT, normalized))
  if (full !== STATIC_ROOT && !full.startsWith(STATIC_ROOT + sep)) return undefined
  const extension = full.slice(full.lastIndexOf('.'))
  if (!Object.hasOwn(CONTENT_TYPES, extension)) return undefined
  return full
}

export function contentTypeFor(path: string): string {
  const extension = path.slice(path.lastIndexOf('.'))
  return CONTENT_TYPES[extension] ?? 'application/octet-stream'
}

export async function serveStatic(relative: string, response: ServerResponse): Promise<void> {
  const path = resolveStaticPath(relative)
  if (path === undefined) {
    response.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' })
    response.end('not found')
    return
  }
  try {
    const body = await readFile(path)
    response.writeHead(200, {
      'content-type': contentTypeFor(path),
      'cache-control': 'public, max-age=300',
      'x-content-type-options': 'nosniff',
    })
    response.end(body)
  } catch {
    response.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' })
    response.end('not found')
  }
}
