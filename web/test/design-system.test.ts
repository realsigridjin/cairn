/**
 * Design-system enforcement.
 *
 * A design system that is not mechanically checked decays into ad-hoc values
 * within a week. These tests are the check: tokens.css is the only file
 * allowed to declare raw colors and raw lengths, and every token referenced
 * elsewhere must actually exist.
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'
import { presentError, errorCodes, isKnownErrorCode } from '../src/view/errors.ts'

const STATIC_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'static')
const TOKENS = readFileSync(resolve(STATIC_DIR, 'tokens.css'), 'utf8')
const APP_CSS = readFileSync(resolve(STATIC_DIR, 'app.css'), 'utf8')

function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//gu, '')
}

/** Declaration bodies only: selectors, at-rules and comments are excluded. */
function declarations(css: string): string[] {
  const body = stripComments(css)
  // Drop @media/@container preludes so breakpoint literals are not flagged.
  const withoutPreludes = body.replace(/@(?:media|container|supports)[^{]*\{/gu, '{')
  return withoutPreludes
    .split(/[{}]/u)
    .filter((_, index) => index % 2 === 1)
    .flatMap(block => block.split(';'))
    .map(line => line.trim())
    .filter(line => line.length > 0 && line.includes(':'))
}

test('tokens.css defines every documented token group', () => {
  for (const token of [
    '--space-4',
    '--stone-900',
    '--signal-400',
    '--data-lexical',
    '--data-vector',
    '--ok-500',
    '--error-500',
    '--surface-base',
    '--border-hairline',
    '--focus-ring',
    '--text-primary',
    '--font-display',
    '--font-body',
    '--font-mono',
    '--text-body',
    '--radius-md',
    '--shadow-raised',
    '--glow-head',
    '--ease-out',
    '--dur-base',
    '--stagger-step',
    '--rail-width',
    '--measure',
    '--target-min',
  ]) {
    assert.ok(TOKENS.includes(`${token}:`), `tokens.css is missing ${token}`)
  }
})

test('app.css declares no raw color values', () => {
  const offenders = declarations(APP_CSS).filter(line =>
    /#[0-9a-f]{3,8}\b|\brgba?\(\s*\d|\bhsla?\(\s*\d/iu.test(line),
  )
  assert.deepEqual(offenders, [], 'colors must come from tokens.css')
})

test('app.css declares no ad-hoc spacing or font sizes', () => {
  // 0, 1px hairlines/ticks, and percentages are structural, not design values.
  const allowedLength = /^(?:0|1px|100%|50%|0%|auto)$/u
  const offenders = declarations(APP_CSS).filter(line => {
    const value = line.slice(line.indexOf(':') + 1)
    const lengths = value.match(/(?<![\w-])\d*\.?\d+(?:px|rem|em)\b/gu) ?? []
    return lengths.some(length => !allowedLength.test(length))
  })
  assert.deepEqual(offenders, [], 'lengths must reference the spacing/type scale')
})

test('app.css names no font family outside the token families', () => {
  const offenders = declarations(APP_CSS).filter(
    line => /font-family\s*:/u.test(line) && !line.includes('var(--font-'),
  )
  assert.deepEqual(offenders, [], 'font families must come from --font-* tokens')
})

test('the excluded generic typefaces appear nowhere', () => {
  for (const banned of ['Inter', 'Roboto', 'Arial', 'Space Grotesk']) {
    assert.ok(!TOKENS.includes(banned), `tokens.css must not use ${banned}`)
    assert.ok(!APP_CSS.includes(banned), `app.css must not use ${banned}`)
  }
})

test('every var(--token) referenced in app.css is defined in tokens.css', () => {
  const referenced = new Set(
    [...stripComments(APP_CSS).matchAll(/var\((--[a-z0-9-]+)/gu)].map(match => match[1] as string),
  )
  const defined = new Set(
    [...TOKENS.matchAll(/(--[a-z0-9-]+)\s*:/gu)].map(match => match[1] as string),
  )
  // Data-driven ratios are values supplied per element, not design tokens.
  const runtimeValues = new Set(['--bar-width', '--stagger-index'])
  const missing = [...referenced].filter(token => !defined.has(token) && !runtimeValues.has(token))
  assert.deepEqual(missing, [], 'undefined tokens referenced')
})

test('the light theme overrides semantic names only, never raw scale steps', () => {
  const lightBlock = /\[data-theme="light"\]\s*\{([\s\S]*?)\n\}/u.exec(stripComments(TOKENS))
  assert.ok(lightBlock !== null, 'a light theme block must exist')
  const overridden = [...(lightBlock[1] as string).matchAll(/(--[a-z0-9-]+)\s*:/gu)].map(
    match => match[1] as string,
  )
  assert.ok(overridden.length > 0)
  const rawScale = overridden.filter(token => /^--(?:stone|signal)-\d+$/u.test(token))
  assert.deepEqual(rawScale, [], 'the light theme must not redefine raw scale steps')
})

test('reduced motion collapses every duration token to zero', () => {
  const block = /@media \(prefers-reduced-motion: reduce\)\s*\{([\s\S]*?)\n\}/u.exec(TOKENS)
  assert.ok(block !== null)
  const body = block[1] as string
  for (const token of ['--dur-instant', '--dur-fast', '--dur-base', '--dur-slow', '--stagger-step']) {
    assert.match(body, new RegExp(`${token}\\s*:\\s*0ms`, 'u'), `${token} must collapse to 0ms`)
  }
})

test('evidence bars distinguish sides by shape, not hue alone', () => {
  const lexical = /\.bar-fill\[data-kind="lexical"\]\s*\{([\s\S]*?)\}/u.exec(APP_CSS)?.[1] ?? ''
  const vector = /\.bar-fill\[data-kind="vector"\]\s*\{([\s\S]*?)\}/u.exec(APP_CSS)?.[1] ?? ''
  // Lexical sits left of centre and is solid; vector sits right and is hatched.
  assert.match(lexical, /right:\s*50%/u)
  assert.match(vector, /left:\s*50%/u)
  assert.match(vector, /repeating-linear-gradient/u)
})

test('focus rings are 2px and target sizes meet WCAG 2.2 SC 2.5.8', () => {
  assert.match(APP_CSS, /:focus-visible\s*\{[\s\S]*?outline:\s*var\(--space-1\)/u)
  assert.match(TOKENS, /--target-min:\s*24px/u)
  assert.match(TOKENS, /--target-touch:\s*44px/u)
  assert.match(APP_CSS, /min-height:\s*var\(--target-min\)/u)
})

test('a print stylesheet exists so handoff packets are self-contained', () => {
  assert.match(APP_CSS, /@media print/u)
})

test('container queries let cards adapt inside the drawer as well as the column', () => {
  assert.match(APP_CSS, /container-type:\s*inline-size/u)
  assert.match(APP_CSS, /@container results/u)
})

/* ---------- Error catalogue ---------- */

test('the error catalogue covers every code the CAIRN server can emit', () => {
  for (const code of [
    'UNAUTHORIZED',
    'SCOPE_FORBIDDEN',
    'HISTORICAL_REVISION_DISABLED',
    'INVALID_SCOPE',
    'INVALID_JSON',
    'INVALID_REQUEST',
    'KNOWLEDGE_BASE_NOT_FOUND',
    'QUERY_VECTOR_REQUIRED',
    'EMBEDDING_UNAVAILABLE',
    'SEARCH_FAILED',
    'TRANSPORT_ERROR',
  ]) {
    assert.ok(isKnownErrorCode(code), `missing catalogue entry for ${code}`)
  }
})

test('every catalogue entry names a cause and a concrete remedy', () => {
  for (const code of errorCodes()) {
    const entry = presentError(code)
    assert.ok(entry.headline.length > 0, `${code} needs a headline`)
    assert.ok(entry.remedy.length > 20, `${code} needs an actionable remedy`)
  }
})

test('EMBEDDING_UNAVAILABLE is never retryable despite being a 503', () => {
  assert.equal(presentError('EMBEDDING_UNAVAILABLE', true).retryable, false)
})

test('SEARCH_FAILED stays retryable', () => {
  assert.equal(presentError('SEARCH_FAILED').retryable, true)
})

test('an unknown code falls back without inventing a remedy', () => {
  const unknown = presentError('WAT', true)
  assert.equal(unknown.code, 'WAT')
  assert.equal(unknown.retryable, true)
})

test('security-relevant codes block the app instead of degrading quietly', () => {
  for (const code of ['VERSION_INCOMPATIBLE', 'SCOPE_DRIFT', 'ENDPOINT_IDENTITY']) {
    assert.equal(presentError(code).blocking, true, `${code} must be blocking`)
  }
})
