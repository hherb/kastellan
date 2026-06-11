# kastellan.dev Website Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the public kastellan.dev website — a landing page plus roadmap, security, and contributing pages — as a hand-rolled static site in `site/`, ready for Cloudflare Pages.

**Architecture:** Four static HTML pages sharing one stylesheet and duplicated nav/footer markup, no build step, no framework. Cloudflare Pages serves the `site/` directory directly from the GitHub repo; the CF-side setup is a documented operator action.

**Tech Stack:** Plain HTML5 + CSS (system-ui font stack, B1 "Pure Clean" palette), minimal vanilla JS (mobile nav toggle only), `sips` for image resizing, `tidy`/`curl` for verification.

**Spec:** `docs/superpowers/specs/2026-06-11-kastellan-dev-website-design.md` — read it before starting. The wireframes it references were approved by the operator; structure and copy below are final.

**Design quality note:** When authoring HTML/CSS (Tasks 2–6), the implementer SHOULD invoke the `frontend-design` skill first and follow its guidance for polish, within the locked constraints: light `#fafbfd` background, indigo `#4f46e5` accent, system-ui sans stack, white cards with `#e2e8f0` borders, one dark `#0f172a` band on the landing page. The copy given below is final — do not rewrite it; layout/visual craft is where the skill adds value.

**Verification model:** This is a static site — there is no unit-test framework. Every task still follows check-first discipline: each page task starts by adding its URL to the link/content checklist script (the "failing test"), then builds the page until the script passes.

---

### Task 0: Scaffold, assets, and the verification script

**Files:**
- Create: `site/assets/` (directory, populated below)
- Create: `scripts/site/check-site.sh`
- Source assets: `assets/kastellan_logo_transparent.png`, `assets/security-architecture.png`, `assets/security-request-flow.png`

- [ ] **Step 1: Create directories and copy assets**

```bash
mkdir -p site/assets scripts/site
cp assets/kastellan_logo_transparent.png site/assets/logo.png
cp assets/security-architecture.png site/assets/security-architecture.png
cp assets/security-request-flow.png site/assets/security-request-flow.png
ls -la site/assets/
```

- [ ] **Step 2: Downscale any image over 1 MB and generate the favicon + OG image**

```bash
# Downscale oversized diagrams to max 1600 px wide (only if > 1 MB)
for f in site/assets/security-architecture.png site/assets/security-request-flow.png; do
  size=$(stat -f%z "$f")
  if [ "$size" -gt 1048576 ]; then sips --resampleWidth 1600 "$f"; fi
done
# Favicon (64x64) and OG image (1200px-wide canvas not required; the logo itself is fine)
sips --resampleWidth 64 site/assets/logo.png --out site/assets/favicon.png
cp site/assets/logo.png site/assets/og-image.png
ls -la site/assets/
```

Expected: five files in `site/assets/` (`logo.png`, `favicon.png`, `og-image.png`, two diagram PNGs), none over ~1 MB.

- [ ] **Step 3: Write the verification script (the site's "test suite")**

Create `scripts/site/check-site.sh`:

```bash
#!/usr/bin/env bash
# Verification suite for the static site in site/.
# Checks: every expected page exists, is valid-enough HTML (tidy: no errors),
# every local href/src it references resolves to a real file, and every page
# carries the required meta/OG tags and shared nav.
set -u
SITE_DIR="$(cd "$(dirname "$0")/../../site" && pwd)"
[ -n "$SITE_DIR" ] && [ -d "$SITE_DIR" ] || { echo "FAIL: site/ directory not found"; exit 1; }
PAGES="index.html roadmap.html security.html contributing.html"
fail=0

# tidy: hard-fail if absent; loud-skip if it's the pre-HTML5 Apple 2006 build
# (macOS /usr/bin/tidy rejects <nav>/<main> etc.). brew install tidy-html5 for
# full validation; CI should install a modern tidy so this never skips there.
#
# Apple 2006 build prints:
#   HTML Tidy for Mac OS X released on 31 October 2006 - Apple Inc. build 13462
# Modern HTML Tidy 5.x prints:
#   HTML Tidy for Apple macOS version 5.8.0   (or Linux version 5.x.y etc.)
#
# Regex 'HTML Tidy.*[ .]5\.' matches a space or literal dot before "5."
# — matches "version 5.8.0" but not the 2006 date-based string (no "5." present).
TIDY_OK=1
if ! command -v tidy >/dev/null 2>&1; then
  echo "FAIL: tidy not installed (brew install tidy-html5 / apt install tidy)"; exit 1
elif ! tidy --version 2>/dev/null | grep -qE 'HTML Tidy.*[ .]5\.'; then
  echo "[SKIP] tidy is pre-HTML5 (Apple 2006 build) — HTML validity check skipped; brew install tidy-html5 for full validation"
  TIDY_OK=0
fi

for page in $PAGES; do
  p="$SITE_DIR/$page"
  if [ ! -f "$p" ]; then echo "FAIL: missing page $page"; fail=1; continue; fi

  # 1. HTML validity: tidy reports no Errors (warnings tolerated)
  if [ "$TIDY_OK" -eq 1 ]; then
    tidy_out=$(tidy -qe "$p" 2>&1)
    errs=$(printf '%s\n' "$tidy_out" | grep -c "Error:")
    if [ "$errs" -ne 0 ]; then echo "FAIL: $page has $errs tidy error(s)"; printf '%s\n' "$tidy_out" | grep "Error:"; fail=1; fi
  fi

  # 2. Required head tags
  for needle in '<meta name="description"' 'property="og:title"' 'property="og:image"' '<title>'; do
    if ! grep -q "$needle" "$p"; then echo "FAIL: $page missing $needle"; fail=1; fi
  done

  # 3. Shared nav links present on every page
  for link in 'href="roadmap.html"' 'href="security.html"' 'href="contributing.html"' 'github.com/hherb/kastellan'; do
    if ! grep -q "$link" "$p"; then echo "FAIL: $page missing nav link $link"; fail=1; fi
  done

  # 4. Every local href/src resolves to a file.
  # Filter skips absolute URLs (https?:, mailto:, tel:, data:, protocol-relative //)
  # and fragment-only refs (#). Note: word-splitting here is intentional and safe —
  # hrefs in this controlled static site contain no spaces.
  for ref in $(grep -oE '(href|src)="[^"]+"' "$p" | sed -E 's/^(href|src)="//; s/"$//' \
               | grep -vE '^(https?:|mailto:|tel:|data:|//|#)'); do
    clean="${ref%%#*}"; clean="${clean%%\?*}"
    if [ -n "$clean" ] && [ ! -e "$SITE_DIR/$clean" ]; then
      echo "FAIL: $page references missing local file: $clean"; fail=1
    fi
  done
done

# 5. Stylesheet exists and defines the locked design tokens
css="$SITE_DIR/style.css"
if [ ! -f "$css" ]; then echo "FAIL: missing style.css"; fail=1; else
  for token in '#4f46e5' '#fafbfd' '#0f172a'; do
    if ! grep -qi -- "$token" "$css"; then echo "FAIL: style.css missing palette token $token"; fail=1; fi
  done
fi

if [ "$fail" -eq 0 ]; then echo "OK: all site checks passed"; else exit 1; fi
```

```bash
chmod +x scripts/site/check-site.sh
```

- [ ] **Step 4: Run the script — verify it fails (no pages exist yet)**

Run: `scripts/site/check-site.sh`
Expected: `FAIL: missing page index.html` (and the other three) + `FAIL: missing style.css`, exit code 1.

- [ ] **Step 5: Commit**

```bash
git add site/assets scripts/site/check-site.sh
git commit -m "feat(site): scaffold site/ assets + verification script"
```

---

### Task 1: Shared stylesheet (`site/style.css`)

**Files:**
- Create: `site/style.css`

- [ ] **Step 1: Invoke the `frontend-design` skill, then write `site/style.css`**

The stylesheet must implement the B1 "Pure Clean" system. Required contract
(class names are referenced by Tasks 2–6 — keep them exactly):

```css
/* Design tokens — locked by the spec */
:root {
  --bg: #fafbfd;          /* page background */
  --surface: #ffffff;     /* cards, nav */
  --border: #e2e8f0;
  --ink: #0f172a;         /* headings */
  --body: #475569;        /* body text */
  --muted: #64748b;
  --faint: #94a3b8;
  --accent: #4f46e5;      /* indigo */
  --dark: #0f172a;        /* landing security band */
  --dark-surface: #1e293b;
  --dark-accent: #818cf8;
  --ok: #16a34a;          /* shipped */
  --warn: #d97706;        /* in progress */
}
```

Components the pages use (style them; exact visual treatment is the
implementer's craft under the frontend-design skill):

- `body` — system-ui stack, `--bg`, `--body` text, comfortable line-height
- `.nav` (sticky white header bar, bottom border) with `.nav-logo`,
  `.nav-links`, `.nav-cta` (bordered GitHub button), `.nav-toggle`
  (hamburger, hidden ≥ 720 px)
- `.hero` two-column (text + logo image), stacks under 720 px
- `.btn` (solid indigo) and `.btn-ghost` (text link with →)
- `.chip` — monospace one-liner box (the `cargo install` line)
- `.pills` / `.pill` — trust-strip badges
- `.band` — full-width dark section (`--dark`) with `.band-card` children
- `.grid-3` / `.grid-2` — responsive card grids (collapse to 1 column < 720 px)
- `.card` with `.card-title`, `.card-text`, and `.badge-done` / `.badge-wip`
  / `.badge-plan` status badges (green / amber / gray pill, small caps)
- `.stats` / `.stat` — big-number stat row (number + caption)
- `.timeline` — left-bordered vertical phase list with colored dot markers
  (`.dot-done`, `.dot-wip`, `.dot-plan`)
- `.callout` — dark rounded box (security invariant) and `.note` — light
  gray info box
- `.page-head` — subpage title + subtitle block
- `.footer` — bordered top, two-side flex, faint small text
- `figure.diagram img` — responsive (`max-width:100%; height:auto`), bordered
- Section spacing utility `.section` and a max-width container `.wrap`
  (~1080 px, centered)

Responsive rule: one breakpoint at 720 px is sufficient (grids collapse,
hero stacks, `.nav-links` hides behind the toggle). Pages must be usable at
360 px wide.

- [ ] **Step 2: Verify the token check passes (pages still missing)**

Run: `scripts/site/check-site.sh 2>&1 | grep style.css`
Expected: no `FAIL: style.css` lines (the missing-page FAILs remain).

- [ ] **Step 3: Commit**

```bash
git add site/style.css
git commit -m "feat(site): shared stylesheet — B1 Pure Clean design system"
```

---

### Task 2: Landing page (`site/index.html`)

**Files:**
- Create: `site/index.html`

- [ ] **Step 1: Write the page**

Head block (same pattern on every page — adjust title/description/og:url per page):

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Kastellan — the trustworthy personal AI agent</title>
  <meta name="description" content="A personal, always-on AI agent built so that security is its foundational property. Every tool in its own kernel sandbox; every plan reviewed before it runs. Open source, AGPL, runs on your hardware.">
  <meta property="og:title" content="Kastellan — the trustworthy personal AI agent">
  <meta property="og:description" content="Full authority within the walls. None to act beyond them. A security-first personal AI agent in Rust.">
  <meta property="og:image" content="https://kastellan.dev/assets/og-image.png">
  <meta property="og:url" content="https://kastellan.dev/">
  <meta property="og:type" content="website">
  <meta name="twitter:card" content="summary">
  <link rel="icon" type="image/png" href="assets/favicon.png">
  <link rel="stylesheet" href="style.css">
</head>
```

Shared nav (identical markup on all four pages; `aria-current="page"` moves):

```html
<header class="nav">
  <div class="wrap">
    <a class="nav-logo" href="index.html"><img src="assets/favicon.png" alt="" width="28" height="28"> kastellan</a>
    <button class="nav-toggle" type="button" aria-label="Menu" aria-expanded="false">☰</button>
    <nav class="nav-links" aria-label="Main">
      <a href="roadmap.html">Roadmap</a>
      <a href="security.html">Security</a>
      <a href="contributing.html">Contributing</a>
      <a class="nav-cta" href="https://github.com/hherb/kastellan">GitHub</a>
    </nav>
  </div>
</header>
```

Body sections in order, with this copy verbatim:

1. **Hero** (`.hero`):
   - H1: `Your personal AI agent.` then accent-colored line `Trustworthy by construction.`
   - Paragraph: `Kastellan reads your mail, searches the web, runs code, and remembers what matters — and it cannot reach anything you didn’t explicitly allow. Every tool runs in its own kernel sandbox. Every plan is reviewed before a single tool fires.`
   - Italic quote: `A castellan is the officer a lord entrusts to hold a stronghold: full authority within the walls, none to act beyond them.`
   - Buttons: `View on GitHub` → `https://github.com/hherb/kastellan` (`.btn`); `How it’s secured →` → `security.html` (`.btn-ghost`)
   - Chip: `$ cargo install kastellan-core` with faint suffix `# v0.1.0 on crates.io`
   - Right column: `<img src="assets/logo.png" alt="The Kastellan robot mascot holding mail, calendar, chat, and code icons" width="280" height="280">`

2. **Trust strip** (`.pills`): `🦀 Rust core` · `⚖️ AGPL-3.0` · `🖥️ Linux + macOS first-class` · `🔌 No vendor lock-in` · `🏠 Runs on your hardware`

3. **Security band** (`.band`):
   - Label: `WHY IT’S DIFFERENT` / Heading: `Two kinds of walls: mechanical and semantic`
   - Card 1 `Kernel sandboxes`: `One OS process and one kernel jail per tool — bubblewrap, Landlock, and seccomp on Linux; Seatbelt on macOS. A compromised tool reaches its own short allowlist. Never the next tool’s. Never the core.`
   - Card 2 `CASSANDRA oversight`: `Every plan the agent forms is reviewed before any tool runs — against five constitutional constraints that no user, admin, or configuration change can override.`
   - Link: `Read the full security architecture →` → `security.html`

4. **Capabilities** (`.grid-3` of `.card`, heading `What it does` with muted suffix `— each capability inside its own walls`). Badges: `.badge-done` = "today", `.badge-plan` = "planned".
   - `💬 Talks to you` (planned) — `Telegram, Signal, and its own email account.`
   - `🌐 Works the web` (today) — `Web search and page fetch, host-allowlisted; a sandboxed browser is next.`
   - `🐍 Runs code` (planned) — `Python in a no-network scratch jail.`
   - `🧠 Remembers` (today) — `Postgres memory with semantic, lexical, and graph recall.`
   - `📈 Learns skills` (today) — `Distils successful runs into reusable skills — gated on your approval.`
   - `📜 Accountable` (today) — `An append-only audit log of every action, enforced by the database itself.`

5. **Status snapshot** (`.stats` + phase dots, heading `Where it stands today`, side-link `Full roadmap →` → `roadmap.html`):
   - Stats: `v0.1.0 / on crates.io` · `~1,500 / tests green` · `13 / workspace crates` · `2 / OS sandboxes, first-class`
   - Dots line: `● Phase 0 — Sandboxed core (done)` `● Phase 1 — Memory & agent loop (done)` `● Phase 3 — Web egress (in progress)` `● Channels · python-exec · frontier gate (planned)` using `.dot-done`/`.dot-wip`/`.dot-plan` colors.

6. **Get involved** (centered): Heading `Built in the open. Help hold the walls.`; text `Rust, security review, docs, red-teaming — contributions welcome.`; button `Start contributing` → `contributing.html`.

7. **Footer** (`.footer`, identical on all pages): (footer `<p>` margins are handled by `.footer p` in style.css — no inline styles)
   - Left: `AGPL-3.0-only · © 2026 Horst Herb`
   - Right links: `GitHub` → `https://github.com/hherb/kastellan` · `crates.io` → `https://crates.io/crates/kastellan-core` · `Roadmap` · `Security` · `Contributing` (relative hrefs)

Before `</body>`, the only JS on the site:

```html
<script>
  const t = document.querySelector('.nav-toggle');
  t?.addEventListener('click', () => {
    const links = document.querySelector('.nav-links');
    const open = links.classList.toggle('open');
    t.setAttribute('aria-expanded', open);
  });
</script>
```

- [ ] **Step 2: Verify**

Run: `scripts/site/check-site.sh 2>&1 | grep -v 'missing page'`
Expected: no `FAIL` lines mentioning `index.html`.

Run: `python3 -m http.server 8043 -d site & sleep 1 && curl -s http://localhost:8043/ | grep -c "Trustworthy by construction" ; kill %1`
Expected: `1`

Then look at it: open `http://localhost:8043/` in a browser (or screenshot via the playwright/preview tooling if available) at desktop and ~390 px width. Hero, band, grids, and nav toggle must all render correctly.

- [ ] **Step 3: Commit**

```bash
git add site/index.html site/style.css
git commit -m "feat(site): landing page"
```

(`style.css` included because page work may add component styles.)

---

### Task 3: Roadmap & status page (`site/roadmap.html`)

**Files:**
- Create: `site/roadmap.html`

- [ ] **Step 1: Write the page**

Same head pattern (title `Roadmap & status — Kastellan`, description `Where Kastellan stands today: what's shipped, what's in progress, what's planned — honestly marked.`, og:url `https://kastellan.dev/roadmap.html`), same nav (aria-current on Roadmap) and footer.

Content, with this copy verbatim:

1. **Page head** (`.page-head`): H1 `Where Kastellan stands`; subtitle `Curated by hand, updated as milestones ship. Last updated: June 2026.` (the date is maintained via the HANDOVER checklist — Task 6).

2. **Stat cards** (`.grid-3` of `.card`): `v0.1.0 / published on crates.io` · `~1,500 / tests green on Linux + macOS` · `Phase 3 / web egress, in progress`

3. **Timeline** (`.timeline`), entries in build order:
   - `.dot-done` **Phase 0 — A core that can jail things** `.badge-done` SHIPPED — `Cross-platform kernel sandboxing (bubblewrap + Landlock + seccomp on Linux, Seatbelt on macOS) with negative tests proving that denials deny. JSON-RPC workers, service supervision, Postgres with an append-only audit log.`
   - `.dot-done` **Phase 1 — Memory & the agent loop** `.badge-done` SHIPPED — `Three-lane memory recall (semantic, lexical, graph), the task scheduler, CASSANDRA plan review, a prompt-injection guard on worker output, encrypted secrets, and an operator-approved skill system.`
   - `.dot-wip` **Phase 3 — Web egress** `.badge-wip` IN PROGRESS — `Web search and web fetch shipped behind host allowlists. The egress proxy — the single chokepoint all worker traffic will be forced through — has its first slice shipped and force-routing in design. ← currently being built`
   - `.dot-plan` **Phase 2 — Channels** `.badge-plan` PLANNED — `Telegram, Signal, and email — how you'll actually talk to it. Inbound first (read-only), outbound after the egress proxy hardens.`
   - `.dot-plan` **Phase 4 — python-exec & agent-authored skills** `.badge-plan` PLANNED — `Python in a no-network scratch jail, and a catalog of named skills with trust tiers and human approval gates.`
   - `.dot-plan` **Phase 5 — Frontier escalation & hardening** `.badge-plan` PLANNED — `A policy gate deciding when a frontier LLM may be consulted, TLS-pinned egress, and a 7-day adversarial soak test.`

4. **Note box** (`.note`): `ℹ️ The development-grade roadmap — every item, every commit hash — lives in the repo:` link `docs/devel/ROADMAP.md` → `https://github.com/hherb/kastellan/blob/main/docs/devel/ROADMAP.md`

- [ ] **Step 2: Verify**

Run: `scripts/site/check-site.sh 2>&1 | grep roadmap`
Expected: no output (no FAILs for roadmap.html).

Visual check at `http://localhost:8043/roadmap.html` (desktop + narrow).

- [ ] **Step 3: Commit**

```bash
git add site/roadmap.html site/style.css
git commit -m "feat(site): roadmap & status page"
```

---

### Task 4: Security architecture page (`site/security.html`)

**Files:**
- Create: `site/security.html`

- [ ] **Step 1: Write the page**

Head (title `Security architecture — Kastellan`, description `The threat-model invariant and every mechanism that enforces it: kernel sandboxes, double containment, the dispatcher chokepoint, CASSANDRA, the egress boundary, and an append-only audit log.`, og:url `https://kastellan.dev/security.html`), shared nav/footer.

Content, copy verbatim:

1. **Page head**: H1 `The walls, layer by layer`; subtitle `The threat-model invariant first — then every mechanism that enforces it.`

2. **Invariant callout** (`.callout`, label `THE INVARIANT`): `Worst-case compromise — of the LLM, a tool, a dependency, or agent-authored code — reaches at most the agent's own OS user, its own Postgres role, its own scratch filesystem, and the explicitly allowlisted endpoints of the one tool that was compromised. Nothing else.`

3. **Diagram**: `<figure class="diagram"><img src="assets/security-architecture.png" alt="Kastellan security architecture: the core, CASSANDRA, and per-worker sandboxes" loading="lazy"><figcaption>The architecture at a glance — mechanical walls below, semantic oversight alongside.</figcaption></figure>` (add real width/height attributes from the actual file via `sips -g pixelWidth -g pixelHeight`).

4. **Defence in depth** (ordered list, heading `Defence in depth`, each item bold-lead + sentence):
   1. `One process, one sandbox per tool.` `Every tool invocation gets its own OS process inside its own kernel jail — bubblewrap on Linux, Seatbelt on macOS, optionally an Apple container micro-VM. Workers never share a sandbox with each other or with the core.`
   2. `Double containment.` `The parent installs the OS sandbox at spawn; the worker then locks itself down again with Landlock and seccomp before serving a single request. A kernel bug in either layer alone does not breach the worker.`
   3. `The dispatcher chokepoint.` `One function authors every worker command, consults policy, and writes the audit row. Channels and schedulers call it — they can never spawn workers themselves.`
   4. `CASSANDRA.` `Semantic oversight on top of mechanical sandboxing: every plan is reviewed before any tool runs, against five constitutional constraints — no physical harm, no fraud or impersonation, no irreversible action without a verified human in the loop, no power concentration, no oversight suppression — that no user, admin, or configuration change can override.`
   5. `The egress boundary.` `Outbound traffic goes through a per-worker proxy that enforces host allowlists, resolves DNS itself, and rejects private and link-local addresses — with every allow and block decision audited.`
   6. `An append-only audit log.` `Postgres role grants make audit rows append-only at the database layer, mirrored to disk. The agent cannot rewrite its own history.`

5. **Second diagram**: `<figure class="diagram"><img src="assets/security-request-flow.png" alt="A single request traced through every security gate, from channel ingress to sandboxed execution" loading="lazy"><figcaption>One instruction traced through every gate — blocks and escalations drawn explicitly.</figcaption></figure>`

6. **Honesty note** (`.note`, heading `What we don't claim`): `macOS Seatbelt is weaker than the Linux stack — a documented asymmetry, not a footnote. The egress proxy does not force-route workers yet (that work is in design). CASSANDRA's LLM review stages are still deterministic stubs. The full, current threat model lives in the repo:` link `docs/threat-model.md` → `https://github.com/hherb/kastellan/blob/main/docs/threat-model.md`

- [ ] **Step 2: Verify**

Run: `scripts/site/check-site.sh 2>&1 | grep security`
Expected: no output.

Visual check at `http://localhost:8043/security.html` — both diagrams render, no layout shift (width/height set).

- [ ] **Step 3: Commit**

```bash
git add site/security.html site/style.css
git commit -m "feat(site): security architecture page"
```

---

### Task 5: Contributing page (`site/contributing.html`)

**Files:**
- Create: `site/contributing.html`

- [ ] **Step 1: Write the page**

Head (title `Contributing — Kastellan`, description `A security-first project needs adversarial eyes as much as feature hands. How to build Kastellan, where help is wanted, and the house rules.`, og:url `https://kastellan.dev/contributing.html`), shared nav/footer.

Content, copy verbatim:

1. **Page head**: H1 `Help hold the walls`; subtitle `A security-first project needs adversarial eyes as much as feature hands.`

2. **Contribution areas** (`.grid-2` of `.card`):
   - `🦀 Rust development` — `Workers, channel adapters, and the macOS sandbox-parity work. The codebase is a 13-crate workspace with a strict no-unsandboxed-spawn rule.`
   - `🔍 Security review` — `Red-team the sandbox policies, the threat model, the egress proxy. Finding a hole is a contribution, not a nuisance.`
   - `📝 Docs & testing` — `The onboarding manual, cross-distro testing, macOS coverage. If a setup step surprised you, that's a bug in the docs.`
   - `💬 Ideas & issues` — `Design discussions and issue triage happen in the open on GitHub.` link `Issues →` → `https://github.com/hherb/kastellan/issues`

3. **Build section** (heading `Build it in three commands`):

```html
<pre class="chip"><code>git clone https://github.com/hherb/kastellan
cd kastellan && cargo build --workspace
cargo test --workspace</code></pre>
```

   Below it: `Ubuntu 24.04+ needs one extra step — an AppArmor profile so bubblewrap can create user namespaces (`sudo scripts/linux/install-bwrap-apparmor-profile.sh`). macOS works out of the box.`

4. **House rules** (heading `House rules`, unordered list):
   - `AGPL-3.0, and AGPL-compatible dependencies only — license hygiene is part of the security boundary.`
   - `Linux and macOS are both first-class: no platform-specific feature lands without a counterpart of equivalent guarantee.`
   - `Every worker is sandboxed before it runs. There is no unsandboxed escape hatch, and reviewers refuse PRs that try to add one.`
   - `The repo's handover convention (docs/devel/handovers/) means any contributor — or any AI session — can pick up exactly where the last one left off.`

5. **Closing CTA** (centered): `Start with the repo:` button `github.com/hherb/kastellan` → `https://github.com/hherb/kastellan`

- [ ] **Step 2: Verify — full suite now passes**

Run: `scripts/site/check-site.sh`
Expected: `OK: all site checks passed`, exit 0.

Visual check at `http://localhost:8043/contributing.html`.

- [ ] **Step 3: Commit**

```bash
git add site/contributing.html site/style.css
git commit -m "feat(site): contributing page"
```

---

### Task 6: Deployment docs + HANDOVER checklist line

**Files:**
- Create: `site/README.md`
- Modify: `docs/devel/handovers/HANDOVER.md` (the session-end checklist at the bottom)

- [ ] **Step 1: Write `site/README.md`**

```markdown
# kastellan.dev — site source

Static site, no build step. Served by Cloudflare Pages from this directory.

## Local preview

    python3 -m http.server 8043 -d site     # from the repo root
    scripts/site/check-site.sh              # validity + link + meta checks

## Cloudflare Pages setup (operator action, one-time)

1. Cloudflare dashboard → **Workers & Pages → Create → Pages →
   Connect to Git** → select `hherb/kastellan`.
2. Build settings: Framework preset **None**, build command **(leave
   empty)**, build output directory **`site`**. Production branch
   **`main`**.
3. After the first deploy: project → **Custom domains** → add
   `kastellan.dev` (and `www.kastellan.dev`). The domain is already in
   this Cloudflare account, so DNS + TLS are provisioned automatically.

Notes: every push to `main` triggers a deploy (it's a file copy — harmless);
PRs that touch the repo get free preview URLs.

## Updating content

Content is curated by hand (see
`docs/superpowers/specs/2026-06-11-kastellan-dev-website-design.md`).
When a milestone ships, update `roadmap.html` (timeline + "Last updated"
stamp) and, if the numbers moved, the landing-page status snapshot.
```

- [ ] **Step 2: Add the checklist line to HANDOVER.md**

Read the session-end checklist section at the bottom of
`docs/devel/handovers/HANDOVER.md` and append one item, matching the list's
existing style:

```markdown
- [ ] If a milestone shipped: does `site/roadmap.html` (timeline + "Last
  updated" stamp, and the landing-page status numbers) need a one-line
  update? See `site/README.md`.
```

- [ ] **Step 3: Verify**

Run: `scripts/site/check-site.sh`
Expected: `OK: all site checks passed` (README.md is not a page; nothing regresses).

Run: `grep -n "site/roadmap.html" docs/devel/handovers/HANDOVER.md`
Expected: one hit inside the checklist section.

- [ ] **Step 4: Commit**

```bash
git add site/README.md docs/devel/handovers/HANDOVER.md
git commit -m "docs(site): deploy instructions + HANDOVER checklist line"
```

---

### Task 7: Final review pass + PR

- [ ] **Step 1: Full verification**

```bash
scripts/site/check-site.sh
```
Expected: `OK: all site checks passed`.

Open all four pages once more at desktop and ~390 px width; click every nav
link on every page; confirm the mobile nav toggle works on all pages.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin claude/relaxed-davinci-ddf2ed
gh pr create --title "feat(site): kastellan.dev website" --body "$(cat <<'EOF'
Public website for kastellan.dev: landing + roadmap + security +
contributing, hand-rolled static HTML/CSS in site/, no build step.
Per the approved design spec
docs/superpowers/specs/2026-06-11-kastellan-dev-website-design.md.

After merge (operator, one-time): connect Cloudflare Pages to this repo
(output dir `site`, no build command) and attach the kastellan.dev domain —
exact steps in site/README.md.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Operator action (post-merge, outside this plan)**

Cloudflare Pages project creation + custom domain attachment per
`site/README.md`. Then verify `https://kastellan.dev` serves the landing
page and a test PR produces a preview URL.
