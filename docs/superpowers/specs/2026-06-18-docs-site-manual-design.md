# docs.kastellan.dev — styled developer manual on GitHub Pages

**Status:** approved design, ready for implementation plan
**Date:** 2026-06-18
**Author:** Horst Herb (with Claude)

## Problem

The main site (`site/`, served by Cloudflare Pages at `kastellan.dev`) links to the
developer onboarding manual as raw GitHub-rendered Markdown. We want that manual
rendered as a polished HTML site that matches the main site's appearance, hosted on
**GitHub Pages** under **`docs.kastellan.dev`**.

The manual is the 14 Markdown files in `docs/devel/manual/`
(`index.md` + `01-*.md` … `13-*.md`). These files must stay in place: they double as
in-repo developer docs that are read directly on GitHub.

## Decisions (locked)

| Decision | Choice | Rationale |
|---|---|---|
| Build approach | Custom minimal converter | Pixel-match the bespoke site; no framework, output is plain static HTML like the rest of `site/`. |
| Converter language | Python in `scripts/site/` | Lightweight text-munging; repo already has Python scripts. Dev/CI-time only — not the agent runtime, so the "Rust core, Python only in sandboxed workers" constraint does not apply. |
| Python tooling | **`uv` only — never bare pip** | PEP 723 inline-dependency script run via `uv run`; reproducible, pinned, no `requirements.txt` drift. |
| Output model | Built in CI, not committed | GitHub Actions renders + deploys on every push to `main`; only Markdown source + converter are tracked. |
| Host | GitHub Pages at `docs.kastellan.dev` | Free, repo-tied; main marketing site stays on Cloudflare. Separate hosts. |

## Architecture

A single Python script renders each Markdown chapter into a styled HTML page that
**reuses the main site's `style.css`** for visual identity, plus a new docs-only
`manual.css` for the additions a multi-page manual needs (sidebar, prose, code theme).
A GitHub Actions workflow builds the site with `uv` and deploys it to GitHub Pages.

```
docs/devel/manual/*.md ──uv run build-manual.py──▶ dist/manual/  ──actions/deploy-pages──▶ docs.kastellan.dev
        (source, stays in repo)                    (gitignored)         (GitHub Pages)
```

### 1. Converter — `scripts/site/build-manual.py`

- **Input:** `docs/devel/manual/*.md`. **Output:** `--out` dir (default `dist/manual/`, gitignored).
- **PEP 723 header** declaring pinned deps, run via `uv run scripts/site/build-manual.py`:

  ```python
  # /// script
  # requires-python = ">=3.11"
  # dependencies = ["markdown==3.7", "pygments==2.18.0"]
  # ///
  ```

  Both `markdown` (BSD) and `Pygments` (BSD) are AGPL-compatible.

- **Markdown extensions:** `fenced_code`, `codehilite` (syntax highlighting for the
  Rust/sh examples), `tables`, `toc` (heading anchors + per-page "On this page" list),
  `attr_list`.
- **Page template:** one HTML shell reused for every chapter. Same sticky nav, footer,
  fonts, and palette as the main site. Nav links point **absolutely** back to
  `https://kastellan.dev` (Home, Roadmap, Security, Contributing) plus GitHub, because
  the docs live on a different host. Body is a `.prose` content column with a
  **left sidebar** listing every chapter.
- **Link rewriting** (operates on Markdown link targets):
  - `./NN-name.md` and `./NN-name.md#anchor` → `NN-name.html` (anchor preserved).
  - `#anchor` (in-page) and `https://…` (external) → left untouched.
  - Any *other* relative path (defensive fallback; none exist today) →
    `https://github.com/hherb/kastellan/blob/main/<path>`.
- **Sidebar source of truth:** a small explicit ordered manifest in the script, two
  groups — **"Onboarding" (1–10)** and **"Subsystem deep dives" (11–13)**. Chapter
  titles are read from each file's first `# H1`. The build **fails if any `*.md` in the
  manual dir is missing from the manifest**, so adding a chapter without listing it
  cannot silently drop it.

### 2. Styling

`site/style.css` is contract-checked by `scripts/site/check-site.sh` and stays
**byte-for-byte unchanged**. All docs-only additions go in a **new `site/manual.css`**,
layered on top:

- sidebar layout (reusing existing tokens — `--border`, `--surface`, `--muted`, `--accent`);
- `.prose` rules styling rendered Markdown (`h2`/`h3`/`p`/`ul`/`ol`/`table`/`blockquote`/inline `code`);
- a Pygments **light** theme tuned to the existing palette (code blocks sit on `--tint`).

The converter copies `site/style.css`, `site/manual.css`, and a small subset of
`site/assets/` (favicon, logo, og-image) into the output, so the docs site is fully
self-contained on its separate host.

### 3. Output layout

```
dist/manual/
  index.html  01-*.html … 13-*.html
  style.css   manual.css   assets/{favicon,logo,og-image}.png
  CNAME       → docs.kastellan.dev
  .nojekyll   → skip Jekyll on Pages
```

### 4. Deployment — `.github/workflows/docs.yml`

- **Trigger:** push to `main` touching `docs/devel/manual/**`, `scripts/site/**`,
  `site/style.css`, `site/manual.css`, `site/assets/**`, or the workflow file; plus
  `workflow_dispatch`.
- **Build job:** `astral-sh/setup-uv@v6` (pinned) →
  `uv run scripts/site/build-manual.py --out dist/manual` →
  `actions/upload-pages-artifact` with `path: dist/manual`.
- **Deploy job:** `actions/deploy-pages` with permissions `pages: write` +
  `id-token: write`, in a `concurrency` group so overlapping pushes don't race.

### 5. DNS + custom domain (one-time operator actions)

Documented in `site/README.md` alongside the existing Cloudflare Pages operator notes:

1. Cloudflare DNS: add `CNAME docs → hherb.github.io`, **DNS-only (grey cloud)** so
   GitHub can issue the Let's Encrypt certificate. (Proxy can be enabled later with
   Full-strict TLS.)
2. GitHub repo → Settings → Pages → Source **GitHub Actions**; Custom domain
   `docs.kastellan.dev`; Enforce HTTPS.

The `CNAME` file emitted into the build keeps the custom domain bound across deploys.

### 6. Close the loop on the main site

- `site/contributing.html`: point "the onboarding manual" at `https://docs.kastellan.dev`.
- Add a **"Manual"** link to the main-site nav across all four pages. This is additive —
  it does not break `check-site.sh`'s required-nav-link assertions.

### 7. Verification — `scripts/site/check-manual.sh`

Mirrors `check-site.sh`: builds into a temp dir via `uv run`, then asserts:

- every chapter in the manifest produced an `.html` file;
- no dangling local `href`/`src` (every local reference resolves);
- `CNAME`, `.nojekyll`, `style.css`, `manual.css`, and the palette tokens are present;
- `tidy` reports no errors (same loud-skip pattern as `check-site.sh` for the Apple-2006 tidy).

Runs locally and in CI (added to the build job before deploy).

**Local preview:**

```sh
uv run scripts/site/build-manual.py --out /tmp/manual
python3 -m http.server 8044 -d /tmp/manual
scripts/site/check-manual.sh
```

## Out of scope (YAGNI for v1)

- Client-side search.
- Dark-mode toggle (the manual matches the light marketing site).
- Versioned docs.

These can be added later if wanted; none block v1.

## Files touched

| File | Change |
|---|---|
| `scripts/site/build-manual.py` | new — the converter (PEP 723, `uv run`) |
| `scripts/site/check-manual.sh` | new — verification suite |
| `site/manual.css` | new — docs-only styling layered on `style.css` |
| `.github/workflows/docs.yml` | new — build (uv) + deploy to Pages |
| `site/contributing.html` | edit — manual link → `https://docs.kastellan.dev` |
| `site/index.html`, `roadmap.html`, `security.html`, `contributing.html` | edit — add "Manual" nav link |
| `site/README.md` | edit — document the one-time DNS + Pages operator steps |
| `.gitignore` | edit — ignore `dist/` |
| `docs/devel/manual/*.md` | unchanged — source of truth, also read on GitHub |
| `site/style.css` | unchanged — locked, contract-checked |
