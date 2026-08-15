/**
 * Progressive enhancement only.
 *
 * Every page is fully server-rendered and the workbench form submits as a
 * plain GET, so the console works with this file blocked. Everything here is
 * keyboard affordances and a theme toggle — no rendering, no fetching, no
 * secrets.
 */

const KEY_TARGETS = new Set(['INPUT', 'TEXTAREA', 'SELECT'])

function isTyping(element) {
  return (
    element instanceof HTMLElement &&
    (KEY_TARGETS.has(element.tagName) || element.isContentEditable)
  )
}

/* ---------- Theme: respects the OS, remembers an explicit choice ---------- */

const THEME_KEY = 'cairn-web-theme'

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme
}

function initTheme() {
  let stored = null
  try {
    stored = localStorage.getItem(THEME_KEY)
  } catch {
    stored = null
  }
  if (stored === 'light' || stored === 'dark') {
    applyTheme(stored)
    return
  }
  applyTheme(window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark')
}

function toggleTheme() {
  const next = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light'
  applyTheme(next)
  try {
    localStorage.setItem(THEME_KEY, next)
  } catch {
    /* storage disabled: theme stays for this page only */
  }
}

/* ---------- Keyboard model ---------- */

function results() {
  return [...document.querySelectorAll('.card')]
}

function focusResult(delta) {
  const cards = results()
  if (cards.length === 0) return
  const active = document.activeElement
  const index = cards.indexOf(active instanceof HTMLElement ? active.closest('.card') : null)
  const next = index < 0 ? 0 : Math.min(cards.length - 1, Math.max(0, index + delta))
  cards[next].focus()
}

document.addEventListener('keydown', event => {
  if (event.defaultPrevented) return

  // Cmd/Ctrl+Enter runs the search from anywhere in the composer.
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
    const form = document.querySelector('form.composer')
    if (form instanceof HTMLFormElement) {
      event.preventDefault()
      form.requestSubmit()
    }
    return
  }

  if (event.metaKey || event.ctrlKey || event.altKey) return
  if (isTyping(event.target)) return

  switch (event.key) {
    case '/': {
      const query = document.getElementById('q')
      if (query instanceof HTMLTextAreaElement) {
        event.preventDefault()
        query.focus()
        query.setSelectionRange(query.value.length, query.value.length)
      }
      break
    }
    case 'j':
      event.preventDefault()
      focusResult(1)
      break
    case 'k':
      event.preventDefault()
      focusResult(-1)
      break
    case 't':
      event.preventDefault()
      toggleTheme()
      break
    default:
      break
  }
})

/* ---------- Copy affordance for citations ---------- */

document.addEventListener('click', event => {
  const target = event.target
  if (!(target instanceof HTMLElement)) return
  const citation = target.closest('.citation')
  if (citation === null || navigator.clipboard === undefined) return
  const text = citation.querySelector('span:last-child')?.textContent?.trim()
  if (text === undefined || text.length === 0) return
  navigator.clipboard.writeText(text).then(
    () => {
      citation.dataset.copied = 'true'
      setTimeout(() => delete citation.dataset.copied, 1200)
    },
    () => {
      /* clipboard denied: the citation is still selectable text */
    },
  )
})

initTheme()
