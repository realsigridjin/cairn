/**
 * Pure presentation logic. No DOM, no I/O — every function here is unit
 * tested, because these are the calculations a reviewer would otherwise have
 * to eyeball in a screenshot.
 */

/** 4x4 deterministic monochrome glyph from the first 8 bytes of the digest. */
export function digestGlyph(corpusSha256: string): readonly boolean[] {
  const normalized = corpusSha256.toLowerCase()
  if (!/^[0-9a-f]{64}$/u.test(normalized)) {
    throw new Error('digestGlyph requires a 64-character hex digest')
  }
  const cells: boolean[] = []
  // 16 hex nibbles = first 8 bytes = 16 cells of a 4x4 grid.
  for (let index = 0; index < 16; index += 1) {
    const nibble = Number.parseInt(normalized[index] as string, 16)
    // Parity of the nibble's set bits: stable, evenly distributed, and
    // independent of the display order.
    const bits = (nibble & 1) + ((nibble >> 1) & 1) + ((nibble >> 2) & 1) + ((nibble >> 3) & 1)
    cells.push(bits % 2 === 1)
  }
  return cells
}

/** Short digest for chrome; the full 64 chars stay available for copy. */
export function digestPrefix(corpusSha256: string, length = 12): string {
  return corpusSha256.slice(0, length)
}

/** Screen readers get the prefix grouped in 4s, never 64 hex characters. */
export function digestAriaLabel(corpusSha256: string, length = 12): string {
  const prefix = digestPrefix(corpusSha256, length)
  const groups = prefix.match(/.{1,4}/gu) ?? [prefix]
  return `corpus digest ${groups.join(' ')}, truncated`
}

/**
 * Evidence bar geometry. Lexical extends left of centre, vector right; each
 * is normalised against the largest magnitude in the result set so bars are
 * comparable within a page. Returned as a percentage of the half-width.
 */
export function evidenceWidths(
  lexical: number,
  vector: number,
  scale: number,
): { readonly lexical: number; readonly vector: number } {
  if (!Number.isFinite(scale) || scale <= 0) return { lexical: 0, vector: 0 }
  const clamp = (value: number): number => {
    if (!Number.isFinite(value) || value <= 0) return 0
    return Math.min(50, (value / scale) * 50)
  }
  return { lexical: clamp(lexical), vector: clamp(vector) }
}

export function evidenceScale(
  hits: readonly { readonly lexicalEvidence: number; readonly vectorEvidence: number }[],
): number {
  let max = 0
  for (const hit of hits) {
    if (Number.isFinite(hit.lexicalEvidence)) max = Math.max(max, hit.lexicalEvidence)
    if (Number.isFinite(hit.vectorEvidence)) max = Math.max(max, hit.vectorEvidence)
  }
  return max > 0 ? max : 1
}

/** Raw numbers are meaningless aloud; this is the card's accessible label. */
export function hitAriaLabel(hit: {
  readonly posterior: number
  readonly score: number
  readonly lexicalEvidence: number
  readonly vectorEvidence: number
}): string {
  return [
    `posterior ${hit.posterior.toFixed(2)}`,
    `log-odds ${hit.score.toFixed(2)}`,
    `lexical evidence ${hit.lexicalEvidence.toFixed(2)}`,
    `vector evidence ${hit.vectorEvidence.toFixed(2)}`,
  ].join(', ')
}

export function formatPosterior(posterior: number): string {
  return posterior.toFixed(3)
}

export function formatScore(score: number): string {
  const fixed = score.toFixed(2)
  return score >= 0 ? `+${fixed}` : fixed
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`
}

export function formatCount(value: number): string {
  return new Intl.NumberFormat('en-US').format(value)
}

/** Absolute + relative; the DOM pairs this with `<time datetime>`. */
export function formatTimestamp(unixMs: number, now: number): string {
  if (!Number.isFinite(unixMs) || unixMs <= 0) return 'unknown'
  const deltaSeconds = Math.round((now - unixMs) / 1000)
  const absolute = Math.abs(deltaSeconds)
  if (absolute < 60) return `${deltaSeconds < 0 ? 'in ' : ''}${absolute}s${deltaSeconds < 0 ? '' : ' ago'}`
  if (absolute < 3600) return `${Math.round(absolute / 60)}m ago`
  if (absolute < 86_400) return `${Math.round(absolute / 3600)}h ago`
  return `${Math.round(absolute / 86_400)}d ago`
}

export function isoTimestamp(unixMs: number): string {
  if (!Number.isFinite(unixMs) || unixMs <= 0) return ''
  return new Date(unixMs).toISOString()
}

/**
 * Truncation mirrors the model-facing limit so both audiences see the same
 * content boundary. Returns whether truncation occurred so the UI can render
 * the inline note rather than silently shortening evidence.
 */
export function truncateText(
  text: string,
  max: number,
): { readonly text: string; readonly truncated: boolean } {
  if (text.length <= max) return { text, truncated: false }
  const cut = text.charCodeAt(max - 1)
  // Do not split a surrogate pair.
  const end = cut >= 0xd800 && cut <= 0xdbff ? max - 1 : max
  return { text: `${text.slice(0, end)}…`, truncated: true }
}

/** `lang` from metadata so screen readers switch voices per result. */
export function hitLang(metadata: Readonly<Record<string, unknown>>): string | undefined {
  const lang = metadata.lang
  if (typeof lang !== 'string') return undefined
  return /^[A-Za-z]{2,3}(?:-[A-Za-z0-9]{1,8})*$/u.test(lang) ? lang : undefined
}

/** Metadata values are untrusted; render scalars, summarise structures. */
export function metadataDisplay(value: unknown): string {
  if (value === null) return 'null'
  if (typeof value === 'string') return value
  if (typeof value === 'number' || typeof value === 'boolean') return String(value)
  if (Array.isArray(value)) return `[${value.length} items]`
  return '{object}'
}
