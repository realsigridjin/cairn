/**
 * Mobile-safe layout guards.
 *
 * The 390px overflow was never a viewport-width bug — `.shell` already collapses
 * to one `minmax(0, 1fr)` column at <=899px. It was **unbreakable inline
 * content**: chunk ids, `cairn://` citations, 64-hex corpus digests, request ids
 * and endpoint URLs are single tokens far wider than 390px, and the chips that
 * carry them pin `white-space: nowrap`. A nowrap token inside a grid/flex track
 * raises that track's min-content floor, the track outgrows the viewport, and
 * the whole document scrolls sideways.
 *
 * These tests assert the two halves of the real fix and, critically, that the
 * fix is not `overflow-x: clip` — clipping hides the overflow while leaving the
 * content unreachable, which is how a masked bug survives a screenshot review.
 *
 * They are static analyses of the CSS and the rendered markup, so they are
 * deterministic and need no browser.
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'
import { DIGEST, headPayload, healthRoute, searchPayload, startApp, versionRoute } from './helpers.ts'

const STATIC_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'static')
const APP_CSS = readFileSync(resolve(STATIC_DIR, 'app.css'), 'utf8')

function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//gu, '')
}

/**
 * Concatenated bodies of EVERY media query whose prelude matches `pattern`.
 *
 * A breakpoint is legitimately declared in more than one place (component
 * styles live next to their component), so taking only the first match would
 * assert against whichever block happens to appear earliest in the file.
 */
function mediaBlock(pattern: RegExp): string {
  const css = stripComments(APP_CSS)
  const global = new RegExp(pattern.source, 'gu')
  const bodies: string[] = []
  for (const match of css.matchAll(global)) {
    const open = css.indexOf('{', match.index)
    let depth = 0
    for (let index = open; index < css.length; index += 1) {
      if (css[index] === '{') depth += 1
      else if (css[index] === '}') {
        depth -= 1
        if (depth === 0) {
          bodies.push(css.slice(open + 1, index))
          break
        }
      }
    }
  }
  assert.ok(bodies.length > 0, `no media query matching ${String(pattern)}`)
  return bodies.join('\n')
}

/**
 * Declarations of one selector inside a block.
 *
 * Parsed by splitting on braces rather than by regex: a selector list like
 * `.chip-mono,\n.num { ... }` must match on any of its members, and a regex
 * anchored to the selector text alone silently returns nothing instead of
 * failing loudly.
 */
function ruleBody(block: string, selector: string): string {
  const bodies: string[] = []
  let cursor = 0
  for (;;) {
    const open = block.indexOf('{', cursor)
    if (open < 0) break
    const close = block.indexOf('}', open)
    if (close < 0) break
    const prelude = block.slice(cursor, open)
    const selectors = prelude.split(',').map(part => part.trim()).filter(part => part.length > 0)
    if (selectors.includes(selector)) bodies.push(block.slice(open + 1, close))
    cursor = close + 1
  }
  return bodies.join('\n')
}

const XS = /@media \(max-width: 639px\)/u
const MD = /@media \(max-width: 899px\)/u

/* ---------- The real fix ---------- */

test('the small-screen breakpoint lets unbreakable tokens wrap instead of widening the track', () => {
  const block = mediaBlock(XS)
  const chip = ruleBody(block, '.chip')
  assert.match(chip, /white-space:\s*normal/u, 'chips must stop forcing nowrap at 390px')
  assert.match(chip, /overflow-wrap:\s*anywhere/u, 'digests and ids must be able to break')
  assert.match(chip, /max-width:\s*100%/u)

  // Monospace values are the widest single tokens on the page.
  assert.match(block, /overflow-wrap:\s*anywhere/u)
  const mono = ruleBody(block, '.chip-mono')
  assert.match(mono, /overflow-wrap:\s*anywhere/u, 'mono values must break at 390px')
})

test('multi-column grids collapse to one column before they can overflow', () => {
  const block = mediaBlock(XS)
  assert.match(
    ruleBody(block, '.budget-grid'),
    /grid-template-columns:\s*minmax\(0,\s*1fr\)/u,
    'the budget grid must be single-column at 390px',
  )
  assert.match(
    ruleBody(block, '.timeline-item'),
    /grid-template-columns:\s*minmax\(0,\s*1fr\)/u,
    'the timeline holds raw query text and must stack at 390px',
  )
  assert.match(
    ruleBody(block, '.lineage-row'),
    /grid-template-columns:\s*minmax\(0,\s*1fr\)/u,
    'lineage values are long paths and must stack at 390px',
  )
})

test('the shell collapses to a single column so the fixed rail cannot overflow', () => {
  const block = mediaBlock(MD)
  assert.match(
    ruleBody(block, '.shell'),
    /grid-template-columns:\s*minmax\(0,\s*1fr\)/u,
    'a fixed --rail-width track would exceed a 390px viewport',
  )
})

test('every flexible track is min-width:0 so its content cannot set a floor', () => {
  // `min-width: auto` on a flex/grid item is the default and is exactly what
  // makes a long token push the layout wider than the viewport.
  for (const selector of ['.main', '.input,\n.textarea']) {
    const match = new RegExp(`${selector.replace(/[.*+?^${}()|[\]\\/]/gu, '\\$&')}\\s*\\{([^}]*)\\}`, 'u').exec(
      stripComments(APP_CSS),
    )
    assert.ok(match !== null, `missing rule for ${selector}`)
    assert.match(match[1] as string, /min-width:\s*0/u, `${selector} must not float on min-content`)
  }
})

/* ---------- The fix must not be a mask ---------- */

test('overflow is prevented, never clipped away', () => {
  // `overflow-x: clip`/`hidden` on the content column would hide the symptom
  // and make the clipped content unreachable rather than readable.
  const block = mediaBlock(XS)
  const main = ruleBody(block, '.main')
  assert.ok(
    !/overflow-x:\s*(?:clip|hidden|scroll)/u.test(main),
    '.main must not mask overflow at 390px; fix the content that overflows',
  )
  assert.ok(
    !/overflow-x:\s*(?:clip|hidden)/u.test(ruleBody(stripComments(APP_CSS), 'body')),
    'body must not mask horizontal overflow',
  )
})

test('long-token wrapping is declared wherever unbreakable values are rendered', () => {
  // These selectors carry citations, ids and free text at every breakpoint.
  for (const selector of ['.citation', '.card-title', '.evidence-text', '.citation-item', '.lineage-value']) {
    const match = new RegExp(`\\${selector}\\s*\\{([^}]*)\\}`, 'u').exec(stripComments(APP_CSS))
    assert.ok(match !== null, `missing rule for ${selector}`)
    assert.match(
      match[1] as string,
      /overflow-wrap:\s*anywhere/u,
      `${selector} renders unbreakable tokens and must wrap`,
    )
  }
})

/* ---------- Desktop must remain intact ---------- */

test('the desktop rail and content column are untouched by the mobile fix', () => {
  const css = stripComments(APP_CSS)
  const shell = /\.shell\s*\{([^}]*)\}/u.exec(css)?.[1] ?? ''
  // The two-column desktop layout is the default, overridden only under <=899px.
  assert.match(shell, /grid-template-columns:\s*var\(--rail-width\)\s+minmax\(0,\s*1fr\)/u)

  const main = /\.main\s*\{([^}]*)\}/u.exec(css)?.[1] ?? ''
  assert.match(main, /max-width:\s*var\(--content-max\)/u)

  // Nowrap chips are the desktop default and must survive outside the xs block.
  const chip = /\.chip\s*\{([^}]*)\}/u.exec(css)?.[1] ?? ''
  assert.match(chip, /white-space:\s*nowrap/u, 'desktop chips must stay on one line')
})

test('the mobile overrides are scoped to their media queries, never global', () => {
  const css = stripComments(APP_CSS)
  // Everything before the first @media is the unconditional base layer.
  const base = css.slice(0, css.indexOf('@media'))
  assert.ok(
    !/white-space:\s*normal/u.test(/\.chip\s*\{([^}]*)\}/u.exec(base)?.[1] ?? ''),
    'the mobile chip override must not leak into the desktop base layer',
  )
})

/* ---------- Rendered markup ---------- */

test('the viewport meta allows zoom and matches the device width', async t => {
  const app = await startApp({
    routes: {
      '/version': versionRoute(),
      '/health': healthRoute(),
      '/v1/acme/kb/handbook/head': () => ({ status: 200, body: headPayload() }),
      '/v1/acme/kb/handbook/search': () => ({ status: 200, body: searchPayload() }),
    },
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()
  const meta = /<meta name="viewport" content="([^"]+)"/u.exec(body)?.[1] ?? ''

  assert.match(meta, /width=device-width/u)
  // Blocking zoom fails WCAG 2.2 SC 1.4.4.
  assert.ok(!/user-scalable\s*=\s*no/u.test(meta))
  assert.ok(!/maximum-scale\s*=\s*1/u.test(meta))
})

test('no rendered element carries an inline width that could exceed a 390px viewport', async t => {
  const app = await startApp({
    routes: {
      '/version': versionRoute(),
      '/health': healthRoute(),
      '/v1/acme/kb/handbook/head': () => ({ status: 200, body: headPayload() }),
      '/v1/acme/kb/handbook/search': () => ({ status: 200, body: searchPayload() }),
    },
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()

  // The only inline styles allowed are data-driven ratios, never fixed sizes.
  const inlineStyles = [...body.matchAll(/style="([^"]*)"/gu)].map(match => match[1] as string)
  assert.ok(inlineStyles.length > 0, 'expected data-driven inline custom properties')
  for (const style of inlineStyles) {
    assert.match(
      style,
      /^--(?:bar-width|stagger-index):/u,
      `unexpected inline style "${style}": layout values belong in app.css`,
    )
    // A custom property is a value, not a layout decision. What must never
    // appear is a real sizing declaration with an absolute unit.
    assert.ok(
      !/(?:^|;)\s*(?:min-|max-)?(?:width|height):\s*[\d.]+(?:px|rem|em)/u.test(style),
      `inline absolute size in "${style}"`,
    )
  }
})

test('the long unbreakable values that caused the overflow are still rendered in full', async t => {
  const app = await startApp({
    routes: {
      '/version': versionRoute(),
      '/health': healthRoute(),
      '/v1/acme/kb/handbook/head': () => ({ status: 200, body: headPayload() }),
      '/v1/acme/kb/handbook/search': () => ({ status: 200, body: searchPayload() }),
    },
  })
  t.after(() => app.close())

  const body = await (await fetch(`${app.origin}/kb/acme/handbook/search?q=refunds`)).text()

  // Wrapping, not truncation: the citation and digest must remain complete.
  assert.ok(body.includes('cairn://acme/handbook/revision/42/chunk/doc-v3-c7'))
  assert.ok(body.includes(`title="${DIGEST}"`), 'the full digest stays available')
})
