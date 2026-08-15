import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  digestAriaLabel,
  digestGlyph,
  digestPrefix,
  evidenceScale,
  evidenceWidths,
  formatBytes,
  formatPosterior,
  formatScore,
  formatTimestamp,
  hitAriaLabel,
  hitLang,
  metadataDisplay,
  truncateText,
} from '../src/view/format.ts'
import { escapeHtml, html, raw } from '../src/view/html.ts'
import { DIGEST, DIGEST_ALT } from './helpers.ts'

test('digestGlyph is deterministic and 16 cells wide', () => {
  const first = digestGlyph(DIGEST)
  const second = digestGlyph(DIGEST)
  assert.equal(first.length, 16)
  assert.deepEqual(first, second)
})

test('digestGlyph differentiates distinct digests', () => {
  assert.notDeepEqual(digestGlyph(DIGEST), digestGlyph(DIGEST_ALT))
})

test('digestGlyph is case-insensitive and rejects non-digests', () => {
  assert.deepEqual(digestGlyph(DIGEST.toUpperCase()), digestGlyph(DIGEST))
  assert.throws(() => digestGlyph('abc'), /64-character hex digest/u)
})

test('digest labels read a grouped prefix, never 64 hex characters', () => {
  assert.equal(digestPrefix(DIGEST), 'a3f9c1b2a3f9')
  const label = digestAriaLabel(DIGEST)
  assert.equal(label, 'corpus digest a3f9 c1b2 a3f9, truncated')
  assert.ok(!label.includes(DIGEST))
})

test('evidenceWidths keeps each side within its half of the bar', () => {
  const widths = evidenceWidths(2.1, 0.4, 2.1)
  assert.equal(widths.lexical, 50)
  assert.ok(widths.vector > 0 && widths.vector < 50)
})

test('evidenceWidths clamps negatives and non-finite input to zero', () => {
  assert.deepEqual(evidenceWidths(-3, Number.NaN, 2), { lexical: 0, vector: 0 })
  assert.deepEqual(evidenceWidths(1, 1, 0), { lexical: 0, vector: 0 })
})

test('evidenceScale takes the largest magnitude across both sides', () => {
  assert.equal(
    evidenceScale([
      { lexicalEvidence: 2.1, vectorEvidence: 0.4 },
      { lexicalEvidence: 0.3, vectorEvidence: 3.5 },
    ]),
    3.5,
  )
  assert.equal(evidenceScale([]), 1)
})

test('hitAriaLabel spells out every number a sighted user can see', () => {
  const label = hitAriaLabel({ posterior: 0.8293, score: 1.58, lexicalEvidence: 2.1, vectorEvidence: 0.4 })
  assert.equal(label, 'posterior 0.83, log-odds 1.58, lexical evidence 2.10, vector evidence 0.40')
})

test('formatScore keeps the sign visible on log-odds', () => {
  assert.equal(formatScore(1.58), '+1.58')
  assert.equal(formatScore(-0.22), '-0.22')
  assert.equal(formatPosterior(0.8293), '0.829')
})

test('formatBytes and formatTimestamp render human magnitudes', () => {
  assert.equal(formatBytes(512), '512 B')
  assert.equal(formatBytes(1_258_291), '1.2 MB')
  assert.equal(formatBytes(-1), '—')
  const now = 1_700_000_000_000
  assert.equal(formatTimestamp(now - 45_000, now), '45s ago')
  assert.equal(formatTimestamp(now - 7_200_000, now), '2h ago')
})

test('truncateText reports truncation instead of silently shortening', () => {
  assert.deepEqual(truncateText('short', 10), { text: 'short', truncated: false })
  const long = truncateText('abcdefghij', 5)
  assert.equal(long.truncated, true)
  assert.equal(long.text, 'abcde…')
})

test('truncateText never splits a surrogate pair', () => {
  const emoji = `abc${'\u{1F600}'}def`
  const cut = truncateText(emoji, 4)
  assert.equal(cut.truncated, true)
  // The high surrogate at index 3 must be dropped, not orphaned.
  assert.equal(cut.text, 'abc…')
  assert.ok(!/[\uD800-\uDBFF]$/u.test(cut.text.slice(0, -1)))
})

test('hitLang only accepts a well-formed BCP-47-ish tag', () => {
  assert.equal(hitLang({ lang: 'ko' }), 'ko')
  assert.equal(hitLang({ lang: 'zh-Hant' }), 'zh-Hant')
  assert.equal(hitLang({ lang: 'not a lang' }), undefined)
  assert.equal(hitLang({}), undefined)
})

test('metadataDisplay summarises structures instead of dumping them', () => {
  assert.equal(metadataDisplay('handbook.pdf'), 'handbook.pdf')
  assert.equal(metadataDisplay(12), '12')
  assert.equal(metadataDisplay(null), 'null')
  assert.equal(metadataDisplay([1, 2, 3]), '[3 items]')
  assert.equal(metadataDisplay({ a: 1 }), '{object}')
})

test('escapeHtml neutralises every HTML-significant character', () => {
  assert.equal(escapeHtml(`<script>&"'`), '&lt;script&gt;&amp;&quot;&#39;')
})

test('html escapes interpolated values by default', () => {
  const injected = '<img src=x onerror=alert(1)>'
  const rendered = html`<p>${injected}</p>`.value
  assert.ok(!rendered.includes('<img'))
  assert.ok(rendered.includes('&lt;img'))
})

test('html only trusts values explicitly marked safe', () => {
  assert.equal(html`<p>${raw('<b>ok</b>')}</p>`.value, '<p><b>ok</b></p>')
})
