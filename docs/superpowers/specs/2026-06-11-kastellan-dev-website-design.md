# kastellan.dev website — design

**Date:** 2026-06-11
**Status:** approved (brainstorming session with operator)

## Goal

A public website at **kastellan.dev** that informs potential users and
contributors about what Kastellan is, the current state of the project, and
the roadmap — in an engaging way. The domain is already held at Cloudflare;
hosting is the free Cloudflare Pages tier, building from the GitHub repo.

## Locked-in decisions

| Decision | Choice |
| --- | --- |
| Scope | Landing page + 3 subpages (roadmap, security, contributing) |
| Source location | `site/` directory in the existing `hherb/kastellan` repo |
| Stack | Hand-rolled static HTML/CSS, no framework, no build step; minimal vanilla JS (mobile nav toggle at most) |
| Hosting | Cloudflare Pages: build command *none*, output directory `site/`, production branch `main`; custom domain `kastellan.dev` attached in the CF dashboard |
| Content freshness | Manually curated narrative; one new line in the HANDOVER.md session-end checklist: "if a milestone shipped, check whether `site/roadmap.html` needs a one-line update" |
| Visual style | **B1 "Pure Clean"** — light (`#fafbfd` background), modern sans-serif (system-ui stack), indigo accent (`#4f46e5`), white cards with `#e2e8f0` borders, one dark navy band (`#0f172a`) on the landing page for the security story |

Rejected alternatives (for the record): single-page site (operator wants a
few pages); separate `kastellan.dev` repo (drift risk); generated-from-ROADMAP.md
content (dev wording leaking into public site); Zola/Eleventy/Astro (build
machinery not warranted at 4 pages); dark "Stronghold"/"Terminal" visual
directions (B1 chosen; Stronghold was a close second — keep its tagline as
copy, not as palette).

## Site structure

```
site/
├── index.html          # Landing
├── roadmap.html        # Roadmap & status
├── security.html       # Security architecture
├── contributing.html   # Contributing / get involved
├── style.css           # One shared stylesheet
└── assets/             # logo + diagrams copied from /assets (optimized), favicon, OG image
```

Every page shares the same nav (logo · Roadmap · Security · Contributing ·
GitHub button) and footer (AGPL-3.0 · © Horst Herb · GitHub / crates.io /
page links). Header/footer markup is duplicated per page — accepted cost at
this scale. All pages get proper `<meta>` description + OpenGraph/Twitter
tags so links unfurl on social/GitHub; the OG image derives from the robot
logo.

## Page designs (wireframes approved in visual companion)

### Landing (`index.html`), top to bottom

1. **Hero** — headline "Your personal AI agent. *Trustworthy by
   construction.*" (second line indigo); subtext (reads mail, searches web,
   runs code, remembers — can't reach anything not explicitly allowed);
   the castellan quote in italics ("full authority within the walls, none
   to act beyond them"); CTAs: "View on GitHub" (primary), "How it's
   secured →" (text link); a `cargo install` one-liner chip noting v0.1.0
   on crates.io; robot logo on the right.
2. **Trust strip** — pill badges: Rust core · AGPL-3.0 · Linux + macOS
   first-class · No vendor lock-in · Runs on your hardware.
3. **Security story** — dark navy band, "Two kinds of walls: mechanical and
   semantic": one card for kernel sandboxes (per-tool jail; bwrap + Landlock
   + seccomp / Seatbelt), one for CASSANDRA (plan review against five
   constitutional constraints no user/admin/config can override). Link to
   /security.
4. **What it does** — six capability cards, each phrased with its
   containment: Talks to you (Telegram/Signal/email) · Works the web
   (host-allowlisted) · Runs code (no-network scratch jail) · Remembers
   (Postgres 3-lane recall) · Learns skills (operator-approved L3 arc) ·
   Accountable (append-only audit log).
5. **Status snapshot** — stat row (v0.1.0 on crates.io · ~1,500 tests green ·
   13 workspace crates · 2 OS sandboxes first-class) + phase dots
   (Phase 0 done · Phase 1 done · Phase 3 in progress · rest planned).
   Link to /roadmap.
6. **Get involved** — "Built in the open. Help hold the walls." + CTA to
   /contributing; footer.

### Roadmap & status (`roadmap.html`)

- Title "Where Kastellan stands", subtitle noting it's curated and showing a
  **"Last updated: <month year>"** stamp.
- Stat cards (v0.1.0 · tests green · current phase).
- Vertical phase timeline with status badges: Phase 0 "A core that can jail
  things" (SHIPPED), Phase 1 "Memory & the agent loop" (SHIPPED), Phase 3
  "Web egress" (IN PROGRESS, with a "currently being built" marker), Phases
  2/4/5 "Channels · python-exec · frontier gate" (PLANNED). Each phase gets
  2–3 lines of plain-language narrative, not dev shorthand.
- Info box linking to the development-grade `docs/devel/ROADMAP.md` in the
  repo for the full item-by-item record.

### Security architecture (`security.html`)

- Title "The walls, layer by layer".
- **The invariant** in a dark callout box (worst-case compromise reaches at
  most the agent's own OS user / its own PG role / its own scratch FS / the
  one compromised tool's allowlisted endpoints — nothing else).
- Existing `assets/security-architecture.png` diagram.
- Numbered defence-in-depth list (1 process + 1 kernel sandbox per tool;
  double containment; dispatcher chokepoint; CASSANDRA; egress boundary;
  append-only audit log).
- Existing `assets/security-request-flow.png` diagram.
- **Honesty section — "what we don't claim":** macOS Seatbelt weaker than
  the Linux stack (documented asymmetry); egress proxy doesn't force-route
  workers yet; CASSANDRA LLM stages still deterministic stubs. Links to
  `docs/threat-model.md` in the repo.

### Contributing (`contributing.html`)

- Title "Help hold the walls", framing that a security-first project needs
  adversarial eyes as much as feature hands.
- Four contribution-area cards: Rust development · Security review /
  red-teaming · Docs & testing · Ideas & issues.
- "Build it in 3 commands" (clone / `cargo build --workspace` /
  `cargo test --workspace`; Ubuntu needs the one AppArmor script, macOS
  works out of the box).
- House rules: AGPL-compatible deps only · cross-platform parity · every
  worker sandboxed, no exceptions · reviewers refuse invariant-breaking PRs ·
  the HANDOVER convention.

## Content principles

- **Never oversell.** Capabilities that aren't built yet (Telegram/Signal,
  browser worker, python-exec, frontier gate) are always visually marked
  planned/in-progress. The status section and roadmap page make the
  done/planned split explicit; the security page carries the "what we don't
  claim" section.
- Public-facing copy is plain language, not handover shorthand; technical
  depth lives behind links into the repo docs.
- Numbers that drift (test count, crate count, version) are stated
  approximately ("~1,500") or as the released version, and refreshed via the
  HANDOVER checklist line.

## Deployment & operations

- Cloudflare Pages project connected to `hherb/kastellan` (GitHub app),
  production branch `main`, no build command, output directory `site/`.
  PRs touching the site get CF preview URLs automatically.
- Custom domain `kastellan.dev` (already in the operator's CF account) +
  `www.kastellan.dev` redirect. CF provisions TLS.
- Known harmless quirk: CF Pages deploys on every push to `main`, even when
  `site/` is unchanged (it's a file copy). A path-filter guard can be added
  later if it bothers anyone.
- The Cloudflare-side setup (creating the Pages project, attaching the
  domain) is an **operator action** in the CF dashboard; the repo work only
  prepares the `site/` directory.

## Error handling / edge cases

- `404.html` is not in scope for v1 (CF Pages serves its default); can be
  added later.
- Images carry width/height attributes (no layout shift) and `alt` text;
  pages are responsive down to ~360 px (capability grids collapse to one
  column, hero stacks).
- No analytics, no cookies, no third-party JS in v1 — fits the project's
  privacy posture; CF's built-in traffic stats suffice.

## Testing

- Local preview: `python3 -m http.server -d site` (or any static server) —
  manual visual check of all four pages, desktop + narrow viewport.
- HTML validity: run each page through a validator (e.g. `tidy -qe` or the
  W3C validator) — no errors.
- Link check: every internal link resolves; every external link (GitHub,
  crates.io, repo docs) is correct.
- After CF Pages setup: verify the production URL, the custom domain, and
  that a PR produces a preview deployment.

## Out of scope (v1)

Blog/news section; generated roadmap numbers; docs-site rendering of
`docs/`; 404 page; analytics; dark-mode toggle (the site is light-only).
