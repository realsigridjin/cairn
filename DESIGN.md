# CAIRN Web — Design & Implementation

The web console for CAIRN, implementing the **Strata** design direction. This document is the
contract: it records what was built, why the load-bearing decisions were made, and where the
mechanical enforcement lives.

Everything described here is under `web/`. No Rust, README, integration, or script file was
modified.

---

## 1. The decision that shapes everything: same-origin BFF

CAIRN's HTTP server cannot serve a browser directly, and pretending otherwise would be a security
regression rather than a convenience:

1. **No CORS.** `src/server.rs` mounts only `DefaultBodyLimit`, `PropagateRequestIdLayer`,
   `SetRequestIdLayer`, and `TraceLayer`. A browser `fetch` from any other origin fails preflight.
2. **The bearer token is a long-lived secret.** `src/server.rs` compares a SHA-256 digest of
   `CAIRN_SERVER_TOKEN`, and the README states plainly that tokens must not be placed in config
   files or command-line arguments. Shipping that token to a browser is strictly worse: it lands
   in devtools, in memory, and in the blast radius of any XSS.
3. **The default bind is loopback with no auth** (`--listen 127.0.0.1:8080`).

So the browser talks **only** to this app's own origin. The BFF holds the token server-side and
calls CAIRN machine-to-machine. This removes all three blockers at once without touching CAIRN's
security model.

**This is not negotiable in the current architecture.** "Just add CORS and call it from the
browser" would require handing every devtools user a credential that grants full KB read access.

Enforcement, in `web/test/server.test.ts`:

- `/api/session` reports `tokenConfigured: boolean` and never the token itself.
- Every rendered HTML page is scanned for the token string and for any `authorization` text.
- No `Access-Control-Allow-*` header is emitted anywhere; cross-origin browser access is
  unsupported by design, not by oversight.

---

## 2. Architecture

```
browser ──same-origin──> CAIRN Web BFF ──Bearer token──> CAIRN server
         (no token)      (holds token)                   (/health /version /head /search)
```

| File | Responsibility |
|---|---|
| `web/src/main.ts` | Production entrypoint. Boots config, client, history, HTTP server. |
| `web/src/server.ts` | Routing, request validation, error mapping, HTML + JSON responses. |
| `web/src/cairn-client.ts` | Server-only CAIRN client. Sole holder of the token. |
| `web/src/protocol.ts` | Wire decoders and the `cairn://` citation primitive. |
| `web/src/config.ts` | Environment parsing, scope list, `SearchRequest` bounds. |
| `web/src/history.ts` | Server-owned session history + imported-session seam. |
| `web/src/static.ts` | Allowlisted static asset serving with traversal defence. |
| `web/src/view/*` | Tokens-bound rendering: HTML escaping, formatting, components, pages. |
| `web/static/tokens.css` | **The design system.** Single source of truth for visual values. |
| `web/static/app.css` | Components. Zero raw colors or lengths. |
| `web/static/app.js` | Progressive enhancement only. |

**Zero npm dependencies.** Node 22.19+/24+ runs TypeScript directly via type stripping, so
`node src/main.ts` is the production entrypoint — no bundler, no build step, no registry access.
`cairn-uqa-dsh` is not installed in this workspace, so `protocol.ts` reproduces its decoders
field-for-field rather than importing them; `web/test/protocol.test.ts` pins the invariants so the
two cannot silently diverge.

### API contract fidelity

These were verified against the Rust source, not assumed:

- **`deny_unknown_fields`**: the search body carries exactly `query`, `limit`, `candidate_limit`,
  `filters`. Any extra key is a 400. Asserted by test.
- **Correlation headers**: `x-request-id` and `x-dsh-tool-call-id` are sanitised to
  `[A-Za-z0-9._:-]{1,256}` before forwarding, because `safe_correlation_header` silently drops
  anything else. A dropped header means an unjoinable server log.
- **Bounds**: `limit ∈ 1..=1000`, `candidate_limit >= limit` and `<= 100_000`, query `<= 16 KiB`,
  `filters <= 64 keys`. Validated in the BFF so an out-of-range value is reported at the composer
  field instead of costing a round trip.
- **Decoder invariants**: descending score order, unique hit ids, `posterior ∈ [0,1]`, 64-hex
  digest, `score_domain` pinned. A decode failure renders an error state with the validator
  message verbatim — **never partial results**.

---

## 3. Design system (built first, per the Phase-2 mandate)

A repo-wide search found no CSS, no component files, no theme config, no tokens — the only "UI"
was CLI text. So the system was specified before any component was written.

`web/static/tokens.css` is the single source of truth. `web/static/app.css` references semantic
tokens only and declares **zero** raw colors and **zero** ad-hoc lengths.

| Axis | Decision |
|---|---|
| Space | 4px base: `--space-1` … `--space-10` |
| Neutrals | Cold slate, `--stone-950` … `--stone-50` |
| Accent | Sodium amber `--signal-*`, reserved **exclusively** for HEAD / live / active |
| Data hues | `--data-lexical` (amber), `--data-vector` (cyan) — charts only, never chrome |
| Semantic | `--ok/warn/error/info-500`, always paired with an icon and text |
| Type | Display Söhne Breit · Body Söhne · Numeric Berkeley Mono, tabular figures mandatory |
| Radii | Small and engineered: 2/4/8px |
| Motion | `--dur-*` + `--stagger-step: 24ms`, all collapsing to `0ms` under reduced motion |

Explicitly excluded typefaces: Inter, Roboto, Arial, Space Grotesk. Pretendard is in the body
stack because the README ships a Korean quick-start and the default warm analyzer is
`standard_cjk` — CJK is a real requirement, not a nicety.

The light theme overrides **semantic names only**. No component knows a theme exists.

### Mechanical enforcement

A design system that is not checked decays within a week. `web/test/design-system.test.ts` fails
the build when:

- `app.css` declares any raw color (`#hex`, `rgb()`, `hsl()`).
- `app.css` declares any length outside the scale (0, `1px` hairlines, and percentages are the
  only structural exceptions).
- Any `font-family` bypasses the `--font-*` tokens.
- Any `var(--token)` reference is undefined in `tokens.css`.
- The light theme redefines a raw scale step instead of a semantic name.
- A duration token fails to collapse to `0ms` under `prefers-reduced-motion`.
- Any excluded typeface appears in either file.

The only inline styles the server emits are `--bar-width` and `--stagger-index`: data-driven
ratios, which are values rather than design decisions.

---

## 4. Strata: the three principles, and where they live in code

**1. Immutability reads as physical.** Committed revisions render as solid bordered strata;
HEAD gets a lit top edge (`--glow-head`); mutable things — the query composer, drafts — are
dashed and translucent (`.stratum[data-state="draft"]`). The invariant "HEAD is a mutable pointer
only" is expressed in the material, not just a label.

**2. Provenance is chrome, not a detail view.** `.provenance` is sticky and always visible:
revision, parent, digest glyph, mode, embedding model, cost. You never click to learn which
revision you are looking at. At every breakpoint it may compact; it may never vanish.

**3. Approximation is visible.** `approximate: true` desaturates card edges, stripes the ranking
axis, and — because color alone is never sufficient — renders the literal word "approximate" plus
an icon.

### Signature elements

- **Evidence Bars** — the most information-dense element in the product. Lexical extends left of
  centre and is **solid**; vector extends right and is **hatched**. Shape, not hue, carries the
  distinction, so the encoding survives grayscale and deuteranopia. Widths are normalised against
  the largest magnitude in the result set (`evidenceScale`) so bars are comparable within a page.
- **Digest Glyph** — a 4×4 deterministic monochrome grid from the first 8 bytes of
  `corpusSha256`, beside a 12-char monospace prefix. Humans compare glyphs far faster than hex,
  which is exactly the cross-session continuity check. The glyph is `aria-hidden`; the accessible
  name is the prefix grouped in 4s, never 64 hex characters.
- **Strata Rail** — HEAD anchored at top with a lit edge, ancestors descending via
  `parentRevision`, each timestamped.
- **Cost Meter** — `remoteBytes` / `rangeReads` in the provenance strip. Cold search is
  range-read-priced; the price stays visible.

---

## 5. Accessibility

Targeting WCAG 2.2 AA. What is implemented and asserted:

- **Never color alone.** Evidence bars differ by position and fill pattern; `cold`/`warm` carry
  distinct glyphs (◇/◆); `approximate` carries an icon and the word.
- **Keyboard.** `/` focuses the query, `Ctrl/Cmd+Enter` runs the search, `j`/`k` traverse results,
  `t` toggles theme. Focus rings are 2px (`--space-1`) with offset. Cards are focusable.
- **Screen readers.** Results live in `aria-live="polite"`; the summary carries provenance —
  *"2 results, revision 42, cold mode, exact"* — not merely a count. Each card is an `<article>`
  with `aria-labelledby`. Raw numbers are meaningless aloud, so each card carries
  *"posterior 0.83, log-odds 1.58, lexical evidence 2.10, vector evidence 0.40"*.
- **Landmarks.** Skip link, `<nav aria-label>`, `<main id="main" tabindex="-1">`, labelled
  sections.
- **Targets.** `--target-min: 24px` (SC 2.5.8), rising to `--target-touch: 44px` under
  `(pointer: coarse)`.
- **i18n.** Per-result `lang` derived from the metadata allowlist so screen readers switch voices.
  Only well-formed tags are honoured (`hitLang`).
- **Reduced motion.** Every duration token collapses to `0ms`; the staggered card reveal is
  disabled outright.

Not yet done: an automated axe-core pass and a manual screen-reader session. Both need a browser
harness, which would require npm dependencies this task deliberately avoids.

---

## 6. Responsive behavior

Results stay in a single column capped at `--measure` at every size — ranking is ordinal, and
columns break scan order.

| Range | Behavior |
|---|---|
| ≥1200 | Full shell: scope rail + strata rail + workbench |
| 900–1199 | Content padding tightens |
| 640–899 | Rail moves above content; single column |
| <640 | Compact provenance and cards; composer reachable |

`ResultCard` uses a container query, so it adapts inside a drawer as well as in the main column.
A print stylesheet exists so handoff packets are self-contained on paper.

---

## 7. States

**Loading is determinate, never shimmer.** `.phases` renders real request phases
(connect → resolve revision → embed → cold retrieval → rerank) with a `--signal-500` fill.
Honest progress beats fake shimmer when `timeoutMs` defaults to 30s.

**Empty states name the remedy**, including the shell command: no committed revision points at
`cairn ingest`; zero hits names the active filters and offers one-click "retry without filters".

**Every error renders cause → remedy → code → `request_id`.** `web/src/view/errors.ts` is the
catalogue; every entry is asserted to carry a headline and an actionable remedy.

The subtle rule that matters most: **retry is driven by the catalogue, not by HTTP status.**
`EMBEDDING_UNAVAILABLE` is a 503 that `classify_runtime_error` marks non-retryable — so the retry
button is **absent, not disabled**. A button that cannot succeed is worse than no button.
`SEARCH_FAILED` (500, retryable) does get one. Both are asserted.

`VERSION_INCOMPATIBLE`, `SCOPE_DRIFT`, and `ENDPOINT_IDENTITY` are **blocking**: they replace the
page rather than degrading quietly, because each means the data on screen may not be what it
claims to be.

### The approximation moment

`approximate = !query_vector.is_empty() || !filters.is_empty()` (`src/runtime.rs`). **Applying
any filter flips the result set to approximate.** This is one line in Rust that is trivial to
miss, and missing it would make the UI quietly lie about exactness.

So the filter field renders an inline notice the moment a filter is present, at the point of
action rather than in documentation:

> **Filtered results are approximate.** Metadata filtering happens after a bounded candidate
> pool, so a selective filter can hide real matches. Raise `candidate_limit` for highly selective
> filters.

---

## 8. Trust boundary

Retrieved chunk text is **untrusted data**, exactly as the model sees it behind
`BEGIN_UNTRUSTED_CAIRN_EVIDENCE`. Same content, same trust label, two audiences.

- Hit text and metadata render as **plain text only** — no markdown, no HTML, no link
  auto-detection. `escapeHtml` is the single choke point; `html` escapes every interpolation
  unless explicitly marked `raw`.
- Every result card carries `data-untrusted="true"` and a warning-toned left edge.
- A test feeds `<img src=x onerror=alert(1)>` as both hit text and metadata title and asserts
  neither becomes live markup.
- CSP is `default-src 'none'` with `script-src 'self'`, no `unsafe-inline`; plus `X-Frame-Options:
  DENY`, `nosniff`, and `Referrer-Policy: no-referrer`.
- Static serving is extension-allowlisted and path-confined; traversal attempts 404.
- **Fenced scopes are never dispatched.** A scope marked `!fenced` renders as a locked control
  with an explanatory tooltip and its route returns 404, so a user cannot click into a guaranteed
  `SCOPE_FORBIDDEN`.

---

## 9. Session history

CAIRN stores no query log — `src/server.rs` has no history route — so retrieval history is the
web app's own concern, keyed by `(tenant, knowledgeBase, revision, corpusSha256, queryId)`.

`HistoryStore` is an in-memory implementation shaped like the eventual SQLite table: append,
bounded retention, cursor paging, session grouping, and import. Swapping the backing store is a
constructor change, not a rewrite.

**Failed searches are first-class records.** An agent that hit `EMBEDDING_UNAVAILABLE` mid-session
is exactly what a human needs to see, so errors are stored with code, message, and retryability
alongside successes.

**The imported-session seam is live**, not a stub: `POST /api/history/import` accepts records from
another agent run, deduplicates on `queryId` so re-import is idempotent, merges them in
chronological order, and marks the session so `/sessions` labels its provenance. This is the
placeholder the continuation-handoff feature will build on.

---

## 10. Running and verifying

```bash
cd web

# Typecheck (tsc 7.0.2 / tsgo, --noEmit)
npm run typecheck

# Tests: 104 assertions, node:test, no network, no sleeps
npm test

# Both
npm run check

# Serve
CAIRN_BASE_URL=http://127.0.0.1:8080 \
CAIRN_WEB_SCOPES='acme/handbook' \
CAIRN_SERVER_TOKEN='...' \
npm start
```

| Variable | Default | Purpose |
|---|---|---|
| `CAIRN_BASE_URL` | `http://127.0.0.1:8080` | Upstream CAIRN. HTTPS required except exact loopback. |
| `CAIRN_SERVER_TOKEN` | *(unset)* | Bearer token. **Server-side only.** |
| `CAIRN_WEB_SCOPES` | *(unset)* | `tenant/kb` list; suffix `!fenced` marks a locked scope. |
| `CAIRN_WEB_HOST` / `CAIRN_WEB_PORT` | `127.0.0.1` / `8787` | Bind address. |
| `CAIRN_WEB_TIMEOUT_MS` | `30000` | Upstream deadline. |
| `CAIRN_WEB_MAX_TEXT_CHARS` | `4000` | Per-hit text budget; truncation is surfaced inline. |

### Test discipline

No fixed sleeps, no polling, no timing luck. The fake CAIRN upstream is a `fetch` implementation
rather than a socket, retries are set to 0 in tests, and servers bind to port 0 and report the
assigned port back. The suite is deterministic by construction.

---

## 11. Deliberately not built

Scoped out to keep 1.0 honest rather than half-built:

- **Ingestion and publishing UI.** `ingest`/`publish`/`promote` are effectful and
  stale-parent-prone; they belong in CLI/CI.
- **The continuation handoff builder.** The packet format and the green/amber/red drift check are
  specified in the brief, and the history layer that feeds them (including the import seam) is
  built and tested. The builder UI itself is not, and claiming otherwise would be false.
- **The `cairn://` deep-link resolver route.** `parseCitation` is implemented and tested; the `/c/*`
  route that consumes it is not wired.
- **Vector-query and lexical-only composer modes.** Text query is implemented. Note that
  `lexical_only` is a **CLI flag, not an HTTP field** — inventing that body key would trip
  `deny_unknown_fields` and 400.
- **Persistent storage.** History is in-memory with bounded retention.

---

## 12. Verification performed

```
$ cd web && npm run typecheck
tsc -p tsconfig.json  →  exit 0, no diagnostics

$ npm test
node --test --test-concurrency=1 test/*.test.ts
# pass 104
# fail 0
```

Live end-to-end run against a stub CAIRN upstream on `127.0.0.1:8099`, with the BFF on `:8788`:

| Check | Result |
|---|---|
| `/` | `302 → /kb/acme/handbook` |
| `/api/session` | 200, `tokenConfigured: true`, token absent from payload |
| `/kb/acme/handbook/search?q=refunds` | 200, 12157 bytes, results + provenance rendered |
| Token string in served HTML | **0 occurrences** |
| Filter applied | approximation notice present |
| `/kb/acme/secrets` (fenced) | 404, zero upstream calls for that scope |
| `limit=9999` | 400, rejected before any upstream call |
| `/static/../src/config.ts` | 404 |
| `POST /api/history/import` | `{imported:1, skipped:0}`; re-import `{imported:0, skipped:1}` |
| Imported session on `/sessions` | rendered with `imported` and `1 failed` labels |
| Inline styles in output | only `--bar-width` and `--stagger-index` |

Not verified: rendered pixels in a real browser. Playwright is unavailable in this environment and
installing it would violate the zero-dependency constraint. Structure, semantics, and token
compliance are asserted mechanically; visual polish should get one human look before release.
