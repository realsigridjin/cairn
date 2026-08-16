/**
 * Minimal server-side HTML rendering.
 *
 * Retrieved chunk text is UNTRUSTED DATA (see README.md
 * "Prompt-injection boundary"). It is rendered as plain text only: no
 * markdown, no HTML, no link auto-detection. `escapeHtml` is the single
 * choke point and every interpolation of server data goes through `html`.
 */

const ESCAPES: Readonly<Record<string, string>> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
}

export function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/gu, char => ESCAPES[char] ?? char)
}

/** Marks a string as already-safe HTML so `html` will not re-escape it. */
export class SafeHtml {
  readonly value: string

  constructor(value: string) {
    this.value = value
  }

  toString(): string {
    return this.value
  }
}

export function raw(value: string): SafeHtml {
  return new SafeHtml(value)
}

export type Renderable = SafeHtml | string | number | boolean | null | undefined | Renderable[]

function render(value: Renderable): string {
  if (value === null || value === undefined || value === false || value === true) return ''
  if (value instanceof SafeHtml) return value.value
  if (Array.isArray(value)) return value.map(render).join('')
  return escapeHtml(String(value))
}

export function html(strings: TemplateStringsArray, ...values: Renderable[]): SafeHtml {
  let out = ''
  strings.forEach((chunk, index) => {
    out += chunk
    if (index < values.length) out += render(values[index])
  })
  return new SafeHtml(out)
}

export function join(parts: readonly Renderable[], separator = ''): SafeHtml {
  return new SafeHtml(parts.map(render).join(separator))
}

/** Attribute value for `class`/`data-*`; always escaped by `html`. */
export function classNames(...parts: (string | false | undefined | null)[]): string {
  return parts.filter((part): part is string => typeof part === 'string' && part.length > 0).join(' ')
}
