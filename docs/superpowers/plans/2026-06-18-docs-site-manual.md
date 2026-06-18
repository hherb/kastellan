# docs.kastellan.dev Styled Manual — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render the 14-file Markdown developer manual into a styled static site that matches kastellan.dev, deployed to GitHub Pages at `docs.kastellan.dev`.

**Architecture:** A single Python converter (`scripts/site/build_manual.py`) renders `docs/devel/manual/*.md` → HTML, reusing the main site's `style.css` plus a new docs-only `manual.css`. A GitHub Actions workflow builds it with `uv` and deploys to GitHub Pages. The Markdown stays in place — it still doubles as in-repo dev docs on GitHub.

**Tech Stack:** Python 3.11+ run via `uv` (PEP 723 inline deps), `markdown` (BSD), `Pygments` (BSD), GitHub Actions (`astral-sh/setup-uv`, `actions/upload-pages-artifact`, `actions/deploy-pages`).

## Global Constraints

- **Python tooling is `uv` only — never bare `pip`.** Converter is a PEP 723 script run via `uv run`; tests via `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest …`.
- **Pinned deps:** `markdown==3.7`, `pygments==2.18.0`. Both BSD → AGPL-compatible. No CDDL/BUSL/SSPL/Elastic/source-available deps.
- **`site/style.css` is treated as locked** — `scripts/site/check-site.sh` greps it for palette tokens (it does not checksum the file). Do NOT modify it. All docs-only CSS goes in the new `site/manual.css`.
- **Docs nav links are absolute** to `https://kastellan.dev` (separate host from the Cloudflare main site).
- **Canonical domain:** `docs.kastellan.dev`. Repo: `hherb/kastellan`. GitHub blob base: `https://github.com/hherb/kastellan/blob/main/`.
- **Converter filename uses an underscore** (`build_manual.py`, not `build-manual.py`) so its functions are importable by the test module.
- Generated HTML is **not committed** — built fresh in CI; `dist/` is gitignored.
- Follow the existing `scripts/site/check-site.sh` style for the new check script (loud `[SKIP]` for the Apple-2006 `tidy`, `FAIL:`/`OK:` lines, exit non-zero on failure).

## File structure

| File | Responsibility |
|---|---|
| `scripts/site/build_manual.py` | Converter: pure helpers (link rewrite, title, manifest) + render + build orchestration. PEP 723. |
| `scripts/site/test_build_manual.py` | Pytest unit + build tests for the converter. |
| `site/manual.css` | Docs-only styling (sidebar, `.prose`, code container) layered on `style.css`. |
| `scripts/site/check-manual.sh` | Verification suite mirroring `check-site.sh`. |
| `.github/workflows/docs.yml` | CI: build with uv + deploy to GitHub Pages. |
| `.gitignore` | Add `dist/`. |
| `site/contributing.html` | Manual link → `https://docs.kastellan.dev`. |
| `site/{index,roadmap,security,contributing}.html` | Add a "Manual" nav link. |
| `site/README.md` | Document one-time DNS + Pages operator steps. |

---

### Task 1: Converter core — link rewriting, titles, manifest validation

**Files:**
- Create: `scripts/site/build_manual.py`
- Test: `scripts/site/test_build_manual.py`

**Interfaces:**
- Consumes: nothing (first task).
- Produces:
  - `MANIFEST: list[tuple[str, list[str]]]` — ordered sidebar groups of chapter stems (no `.md`), excluding `index`.
  - `INDEX_STEM = "index"`.
  - `manifest_stems() -> list[str]` — every stem in the manifest plus `index`.
  - `rewrite_link(target: str, manual_rel: str = "docs/devel/manual") -> str`.
  - `chapter_title(md_text: str) -> str` — text of the first `# ` heading, `—`/whitespace-trimmed.
  - `validate_manifest(src: Path) -> None` — raises `ValueError` if the set of `*.md` stems in `src` ≠ `set(manifest_stems())`.

- [ ] **Step 1: Write the failing test**

Create `scripts/site/test_build_manual.py`:

```python
# /// script
# requires-python = ">=3.11"
# dependencies = ["pytest", "markdown==3.7", "pygments==2.18.0"]
# ///
"""Tests for build_manual.py. Run via:
   uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 \
       pytest scripts/site/test_build_manual.py -v
"""
import importlib.util
from pathlib import Path

import pytest

_SPEC = importlib.util.spec_from_file_location(
    "build_manual", Path(__file__).with_name("build_manual.py")
)
bm = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(bm)


@pytest.mark.parametrize("target,expected", [
    ("./01-what-is-kastellan.md", "01-what-is-kastellan.html"),
    ("./05-build-test-run.md#what-skip-lines-mean",
     "05-build-test-run.html#what-skip-lines-mean"),
    ("index.md", "index.html"),
    ("#the-workers-directory", "#the-workers-directory"),
    ("https://modelcontextprotocol.io", "https://modelcontextprotocol.io"),
    ("mailto:x@y.z", "mailto:x@y.z"),
])
def test_rewrite_link(target, expected):
    assert bm.rewrite_link(target) == expected


def test_rewrite_link_out_of_tree_goes_to_github_blob():
    assert bm.rewrite_link("../architecture.md") == (
        "https://github.com/hherb/kastellan/blob/main/docs/devel/architecture.md"
    )


def test_chapter_title_strips_hash_and_dash():
    assert bm.chapter_title("# 1 — What is kastellan?\n\nbody") == \
        "1 — What is kastellan?"


def test_manifest_stems_includes_index_and_all_chapters():
    stems = bm.manifest_stems()
    assert "index" in stems
    assert "01-what-is-kastellan" in stems
    assert "13-llm-router" in stems
    assert len(stems) == 14


def test_validate_manifest_passes_on_real_manual():
    repo = Path(__file__).resolve().parents[2]
    bm.validate_manifest(repo / "docs" / "devel" / "manual")  # no raise


def test_validate_manifest_raises_on_unlisted_file(tmp_path):
    for stem in bm.manifest_stems():
        (tmp_path / f"{stem}.md").write_text(f"# {stem}\n")
    (tmp_path / "99-orphan.md").write_text("# 99 — Orphan\n")
    with pytest.raises(ValueError, match="99-orphan"):
        bm.validate_manifest(tmp_path)
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest scripts/site/test_build_manual.py -v`
Expected: FAIL — `build_manual.py` does not exist (import error).

- [ ] **Step 3: Write minimal implementation**

Create `scripts/site/build_manual.py`:

```python
# /// script
# requires-python = ">=3.11"
# dependencies = ["markdown==3.7", "pygments==2.18.0"]
# ///
"""Render docs/devel/manual/*.md into a styled static site for docs.kastellan.dev.

Usage:
    uv run scripts/site/build_manual.py --out dist/manual
"""
from __future__ import annotations

import os
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
MANUAL_SRC = REPO_ROOT / "docs" / "devel" / "manual"
SITE_DIR = REPO_ROOT / "site"
GITHUB_BLOB = "https://github.com/hherb/kastellan/blob/main/"
CANONICAL_DOMAIN = "docs.kastellan.dev"

INDEX_STEM = "index"

# Ordered sidebar groups. Stems are filenames without the .md suffix.
# Adding a chapter? Add its stem here — validate_manifest() fails the build
# if a *.md file in the manual dir is missing from this list.
MANIFEST: list[tuple[str, list[str]]] = [
    ("Onboarding", [
        "01-what-is-kastellan", "02-dev-env-linux", "03-dev-env-macos",
        "04-repo-tour", "05-build-test-run", "06-architecture",
        "07-sandboxing", "08-hard-constraints", "09-rust-patterns",
        "10-first-contribution",
    ]),
    ("Subsystem deep dives", [
        "11-cassandra-pipeline", "12-memory-and-recall", "13-llm-router",
    ]),
]

_SCHEMES = ("#", "http://", "https://", "mailto:", "tel:", "//")


def manifest_stems() -> list[str]:
    stems = [INDEX_STEM]
    for _group, chapter_stems in MANIFEST:
        stems.extend(chapter_stems)
    return stems


def rewrite_link(target: str, manual_rel: str = "docs/devel/manual") -> str:
    """Rewrite a Markdown link target for the rendered site.

    - anchors / external schemes → unchanged
    - sibling chapter `./X.md` or `X.md` → `X.html` (anchor preserved)
    - any other relative path → GitHub blob URL at its repo path
    """
    if not target or target.startswith(_SCHEMES):
        return target
    path, _, frag = target.partition("#")
    frag = f"#{frag}" if frag else ""
    p = path[2:] if path.startswith("./") else path
    if p.endswith(".md") and "/" not in p and ".." not in p:
        return f"{p[:-3]}.html{frag}"
    repo_path = os.path.normpath(os.path.join(manual_rel, path))
    return f"{GITHUB_BLOB}{repo_path}{frag}"


def chapter_title(md_text: str) -> str:
    for line in md_text.splitlines():
        if line.startswith("# "):
            return line[2:].strip()
    raise ValueError("no level-1 heading found")


def validate_manifest(src: Path) -> None:
    on_disk = {p.stem for p in src.glob("*.md")}
    declared = set(manifest_stems())
    missing_file = declared - on_disk
    unlisted = on_disk - declared
    if missing_file or unlisted:
        raise ValueError(
            f"manifest/manual mismatch — unlisted files: {sorted(unlisted)}; "
            f"declared-but-missing: {sorted(missing_file)}"
        )


if __name__ == "__main__":  # pragma: no cover — filled in Task 2
    raise SystemExit("build() not implemented yet")
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest scripts/site/test_build_manual.py -v`
Expected: PASS — all 10 test cases green.

- [ ] **Step 5: Commit**

```bash
git add scripts/site/build_manual.py scripts/site/test_build_manual.py
git commit -m "feat(docs-site): converter core — link rewrite, titles, manifest"
```

---

### Task 2: Render + build orchestration

**Files:**
- Modify: `scripts/site/build_manual.py`
- Test: `scripts/site/test_build_manual.py`

**Interfaces:**
- Consumes (from Task 1): `MANIFEST`, `INDEX_STEM`, `manifest_stems`, `rewrite_link`, `chapter_title`, `validate_manifest`, `MANUAL_SRC`, `SITE_DIR`, `CANONICAL_DOMAIN`.
- Produces:
  - `render_body(md_text: str) -> str` — Markdown → HTML body with links rewritten and code highlighted (codehilite, class `.codehilite`).
  - `sidebar_html(titles: dict[str, str], active_stem: str) -> str`.
  - `page_html(title: str, body: str, sidebar: str) -> str`.
  - `build(out: Path, src: Path = MANUAL_SRC) -> None` — full build into `out`.
  - CLI: `uv run scripts/site/build_manual.py --out <dir>`.

- [ ] **Step 1: Write the failing test**

Append to `scripts/site/test_build_manual.py`:

```python
def _build(tmp_path):
    repo = Path(__file__).resolve().parents[2]
    out = tmp_path / "manual"
    bm.build(out, src=repo / "docs" / "devel" / "manual")
    return out


def test_build_emits_every_chapter(tmp_path):
    out = _build(tmp_path)
    for stem in bm.manifest_stems():
        assert (out / f"{stem}.html").is_file(), f"missing {stem}.html"


def test_build_rewrites_intra_manual_links(tmp_path):
    out = _build(tmp_path)
    index = (out / "index.html").read_text()
    assert 'href="01-what-is-kastellan.html"' in index
    assert ".md\"" not in index  # no raw .md link targets leaked through


def test_build_nav_links_are_absolute(tmp_path):
    out = _build(tmp_path)
    page = (out / "06-architecture.html").read_text()
    assert 'href="https://kastellan.dev/security.html"' in page
    assert '<link rel="stylesheet" href="manual.css">' in page
    assert '<meta name="description"' in page


def test_build_copies_assets_and_writes_pages_files(tmp_path):
    out = _build(tmp_path)
    assert (out / "style.css").is_file()
    assert (out / "manual.css").is_file()
    assert (out / "pygments.css").is_file()
    assert (out / "assets" / "favicon.png").is_file()
    assert (out / ".nojekyll").is_file()
    assert (out / "CNAME").read_text().strip() == "docs.kastellan.dev"
```

Note: this task's tests reference `site/manual.css`, which is created in Task 3. Create a placeholder now so the copy step has a source:

```bash
printf '/* docs manual styles — filled in Task 3 */\n' > site/manual.css
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest scripts/site/test_build_manual.py -v`
Expected: FAIL — `bm.build` does not exist (`AttributeError`).

- [ ] **Step 3: Write minimal implementation**

In `scripts/site/build_manual.py`, add imports at the top of the import block:

```python
import argparse
import re
import shutil

import markdown as md
from pygments.formatters import HtmlFormatter
```

Replace the `if __name__ == "__main__":` block at the end with:

```python
ASSETS = ("favicon.png", "logo.png", "og-image.png")
_PYGMENTS_STYLE = "default"

_PAGE_TEMPLATE = """<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title} — Kastellan manual</title>
  <meta name="description" content="{title} — the kastellan developer manual.">
  <meta property="og:title" content="{title} — Kastellan manual">
  <meta property="og:description" content="The kastellan developer onboarding manual.">
  <meta property="og:image" content="https://kastellan.dev/assets/og-image.png">
  <meta property="og:type" content="article">
  <meta name="twitter:card" content="summary">
  <link rel="icon" type="image/png" href="assets/favicon.png">
  <link rel="stylesheet" href="style.css">
  <link rel="stylesheet" href="manual.css">
  <link rel="stylesheet" href="pygments.css">
</head>
<body>
  <header class="nav">
    <div class="wrap">
      <a class="nav-logo" href="https://kastellan.dev/"><img src="assets/favicon.png" alt="" width="28" height="28"> kastellan</a>
      <button class="nav-toggle" type="button" aria-label="Menu" aria-expanded="false">☰</button>
      <nav class="nav-links" aria-label="Main">
        <a href="https://kastellan.dev/roadmap.html">Roadmap</a>
        <a href="https://kastellan.dev/security.html">Security</a>
        <a href="https://kastellan.dev/contributing.html">Contributing</a>
        <a href="index.html" aria-current="page">Manual</a>
        <a class="nav-cta" href="https://github.com/hherb/kastellan">GitHub</a>
      </nav>
    </div>
  </header>
  <div class="manual-layout wrap">
    <aside class="manual-sidebar" aria-label="Manual chapters">
{sidebar}
    </aside>
    <main class="prose">
{body}
    </main>
  </div>
  <footer class="footer">
    <div class="wrap">
      <p>AGPL-3.0-only · © 2026 Horst Herb</p>
      <nav aria-label="Footer">
        <a href="https://kastellan.dev/">kastellan.dev</a> ·
        <a href="https://github.com/hherb/kastellan">GitHub</a> ·
        <a href="https://crates.io/crates/kastellan-core">crates.io</a>
      </nav>
    </div>
  </footer>
  <script>
    const t = document.querySelector('.nav-toggle');
    t?.addEventListener('click', () => {{
      const links = document.querySelector('.nav-links');
      const open = links.classList.toggle('open');
      t.setAttribute('aria-expanded', open);
    }});
  </script>
</body>
</html>
"""

_HREF_RE = re.compile(r'href="([^"]+)"')


def render_body(md_text: str) -> str:
    engine = md.Markdown(extensions=[
        "fenced_code", "codehilite", "tables", "toc", "attr_list",
    ], extension_configs={"codehilite": {"guess_lang": False}})
    html = engine.convert(md_text)
    return _HREF_RE.sub(lambda m: f'href="{rewrite_link(m.group(1))}"', html)


def sidebar_html(titles: dict[str, str], active_stem: str) -> str:
    home_active = " active" if active_stem == INDEX_STEM else ""
    lines = ['<nav class="manual-toc">']
    lines.append(
        f'<a class="manual-home{home_active}" href="index.html">Overview</a>'
    )
    for group, stems in MANIFEST:
        lines.append(f'<p class="manual-group">{group}</p>')
        lines.append("<ol>")
        for stem in stems:
            active = " active" if stem == active_stem else ""
            lines.append(
                f'<li><a class="manual-link{active}" '
                f'href="{stem}.html">{titles[stem]}</a></li>'
            )
        lines.append("</ol>")
    lines.append("</nav>")
    return "\n".join(lines)


def build(out: Path, src: Path = MANUAL_SRC) -> None:
    validate_manifest(src)
    out.mkdir(parents=True, exist_ok=True)

    sources = {stem: (src / f"{stem}.md").read_text() for stem in manifest_stems()}
    titles = {}
    for stem, text in sources.items():
        titles[stem] = "Overview" if stem == INDEX_STEM else chapter_title(text)

    for stem, text in sources.items():
        body = render_body(text)
        sidebar = sidebar_html(titles, stem)
        page = _PAGE_TEMPLATE.format(title=titles[stem], body=body, sidebar=sidebar)
        (out / f"{stem}.html").write_text(page)

    # Stylesheets + assets (self-contained on the GitHub Pages host).
    shutil.copy2(SITE_DIR / "style.css", out / "style.css")
    shutil.copy2(SITE_DIR / "manual.css", out / "manual.css")
    (out / "pygments.css").write_text(
        HtmlFormatter(style=_PYGMENTS_STYLE).get_style_defs(".codehilite")
    )
    (out / "assets").mkdir(exist_ok=True)
    for name in ASSETS:
        shutil.copy2(SITE_DIR / "assets" / name, out / "assets" / name)

    # GitHub Pages glue.
    (out / "CNAME").write_text(f"{CANONICAL_DOMAIN}\n")
    (out / ".nojekyll").write_text("")


def main() -> None:
    parser = argparse.ArgumentParser(description="Build the kastellan manual site.")
    parser.add_argument("--out", default="dist/manual", type=Path,
                        help="output directory (default: dist/manual)")
    parser.add_argument("--src", default=MANUAL_SRC, type=Path,
                        help="manual Markdown source dir")
    args = parser.parse_args()
    build(args.out, src=args.src)
    print(f"OK: built {len(manifest_stems())} pages → {args.out}")


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest scripts/site/test_build_manual.py -v`
Expected: PASS — all Task 1 + Task 2 tests green.

Also smoke-build locally:
Run: `uv run scripts/site/build_manual.py --out /tmp/manual && ls /tmp/manual`
Expected: `OK: built 14 pages → /tmp/manual`, directory lists 14 `.html` files + `style.css manual.css pygments.css CNAME assets`.

- [ ] **Step 5: Commit**

```bash
git add scripts/site/build_manual.py scripts/site/test_build_manual.py site/manual.css
git commit -m "feat(docs-site): render pipeline, template, sidebar, asset copy"
```

---

### Task 3: `site/manual.css` — docs-only styling

**Files:**
- Modify: `site/manual.css` (replace the placeholder)

**Interfaces:**
- Consumes: the class names emitted by Task 2 (`.manual-layout`, `.manual-sidebar`, `.manual-toc`, `.manual-group`, `.manual-link`, `.manual-home`, `.active`, `.prose`, `.codehilite`) and the design tokens from `style.css` (`--border`, `--surface`, `--ink`, `--body`, `--muted`, `--accent`, `--tint`, `--radius-*`, `--font-mono`).
- Produces: a stylesheet that visually matches the main site.

- [ ] **Step 1: Write the failing test**

Append to `scripts/site/test_build_manual.py`:

```python
def test_manual_css_defines_layout_and_prose(tmp_path):
    out = _build(tmp_path)
    css = (out / "manual.css").read_text()
    for selector in [".manual-layout", ".manual-sidebar", ".prose",
                     ".manual-toc", ".codehilite"]:
        assert selector in css, f"manual.css missing {selector}"
    assert "var(--accent)" in css  # built on the locked palette tokens
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest scripts/site/test_build_manual.py::test_manual_css_defines_layout_and_prose -v`
Expected: FAIL — placeholder `manual.css` has none of those selectors.

- [ ] **Step 3: Write minimal implementation**

Replace the contents of `site/manual.css` with:

```css
/* ==========================================================================
   docs.kastellan.dev — manual-only styles, layered on style.css.
   style.css is locked (check-site.sh); all docs additions live here.
   ========================================================================== */

/* ---- Two-column layout: sidebar + prose ---------------------------------- */
.manual-layout {
  display: grid;
  grid-template-columns: 16rem minmax(0, 1fr);
  gap: clamp(1.5rem, 4vw, 3rem);
  padding-block: clamp(2rem, 5vw, 3.5rem);
  align-items: start;
}

.manual-sidebar {
  position: sticky;
  top: 5rem;
  max-height: calc(100vh - 6rem);
  overflow-y: auto;
  padding-right: 0.5rem;
  font-size: 0.9rem;
}

.manual-toc ol {
  list-style: none;
  margin: 0 0 1.25rem;
  padding: 0;
}

.manual-toc li { margin: 0; }

.manual-group {
  margin: 1.25rem 0 0.5rem;
  font-size: 0.72rem;
  font-weight: 700;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--faint);
}

.manual-home,
.manual-link {
  display: block;
  padding: 0.35rem 0.6rem;
  border-radius: var(--radius-s);
  color: var(--body);
  line-height: 1.35;
}

.manual-home { font-weight: 600; color: var(--ink); }

.manual-home:hover,
.manual-link:hover {
  background: var(--tint);
  text-decoration: none;
  color: var(--ink);
}

.manual-home.active,
.manual-link.active {
  background: rgba(79, 70, 229, 0.09);
  color: var(--accent);
  font-weight: 600;
}

/* ---- Prose (rendered Markdown) ------------------------------------------- */
.prose {
  max-width: 46rem;
  min-width: 0;
}

.prose h1 {
  font-size: clamp(1.8rem, 4vw, 2.4rem);
  letter-spacing: -0.02em;
  margin-bottom: 0.4em;
}

.prose h2 {
  margin-top: 2.25rem;
  padding-top: 0.35rem;
  border-top: 1px solid var(--border);
}

.prose h3 { margin-top: 1.75rem; }

.prose p,
.prose li { color: var(--body); }

.prose ul,
.prose ol { padding-left: 1.4rem; }

.prose li { margin-bottom: 0.4rem; }

.prose blockquote {
  margin: 1.5rem 0;
  padding: 0.25rem 0 0.25rem 1rem;
  border-left: 3px solid var(--border);
  color: var(--muted);
  font-style: italic;
}

.prose a { color: var(--accent); font-weight: 500; }

.prose table {
  width: 100%;
  border-collapse: collapse;
  margin: 1.5rem 0;
  font-size: 0.92rem;
}

.prose th,
.prose td {
  padding: 0.5rem 0.75rem;
  border: 1px solid var(--border);
  text-align: left;
  vertical-align: top;
}

.prose th { background: var(--tint); color: var(--ink); font-weight: 600; }

/* Inline code */
.prose :not(pre) > code {
  padding: 0.12rem 0.38rem;
  background: var(--tint);
  border: 1px solid var(--border);
  border-radius: 6px;
  font-size: 0.85em;
  color: var(--ink);
}

/* ---- Code blocks (Pygments token colours come from pygments.css) --------- */
.codehilite {
  margin: 1.25rem 0;
  padding: 1rem 1.25rem;
  background: var(--tint);
  border: 1px solid var(--border);
  border-radius: var(--radius-s);
  overflow-x: auto;
}

.codehilite pre { margin: 0; }

.codehilite,
.codehilite pre code {
  font-family: var(--font-mono);
  font-size: 0.85rem;
  line-height: 1.7;
}

/* ---- Responsive: sidebar stacks above content --------------------------- */
@media (max-width: 859.98px) {
  .manual-layout { grid-template-columns: 1fr; }

  .manual-sidebar {
    position: static;
    max-height: none;
    border-bottom: 1px solid var(--border);
    padding-bottom: 1rem;
  }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run --with pytest --with markdown==3.7 --with pygments==2.18.0 pytest scripts/site/test_build_manual.py -v`
Expected: PASS — all tests green.

Visual smoke check:
Run: `uv run scripts/site/build_manual.py --out /tmp/manual && python3 -m http.server 8044 -d /tmp/manual`
Then open `http://localhost:8044/` and confirm the sidebar, prose, and code blocks match the kastellan.dev look. Ctrl-C when done.

- [ ] **Step 5: Commit**

```bash
git add site/manual.css scripts/site/test_build_manual.py
git commit -m "feat(docs-site): manual.css — sidebar, prose, code styling on the locked palette"
```

---

### Task 4: `check-manual.sh` verification suite

**Files:**
- Create: `scripts/site/check-manual.sh`

**Interfaces:**
- Consumes: `build_manual.py` via `uv run`.
- Produces: an executable check script that builds into a temp dir and asserts structure, links, glue files, and HTML validity; prints `OK:` / `FAIL:` and exits non-zero on failure.

- [ ] **Step 1: Write the failing test**

There is no unit test for a shell script; the test is running it. First create the file (Step 3), then Step 4 runs it. For Step 2, confirm it does not yet exist:

Run: `test ! -e scripts/site/check-manual.sh && echo "absent (expected)"`
Expected: `absent (expected)`

- [ ] **Step 2: (covered by Step 1 existence check)**

- [ ] **Step 3: Write the implementation**

Create `scripts/site/check-manual.sh`:

```bash
#!/usr/bin/env bash
# Verification suite for the generated manual site (docs.kastellan.dev).
# Builds into a temp dir via uv, then checks: every chapter rendered, every
# local href/src resolves, the Pages glue files exist, the palette tokens are
# present, and tidy reports no HTML errors. Mirrors scripts/site/check-site.sh.
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
fail=0

# Build (uv resolves the PEP 723 deps).
if ! uv run "$ROOT/scripts/site/build_manual.py" --out "$OUT" >/dev/null; then
  echo "FAIL: build_manual.py did not complete"; exit 1
fi

# 1. Every manifest chapter produced an .html file.
for stem in index 01-what-is-kastellan 02-dev-env-linux 03-dev-env-macos \
            04-repo-tour 05-build-test-run 06-architecture 07-sandboxing \
            08-hard-constraints 09-rust-patterns 10-first-contribution \
            11-cassandra-pipeline 12-memory-and-recall 13-llm-router; do
  [ -f "$OUT/$stem.html" ] || { echo "FAIL: missing $stem.html"; fail=1; }
done

# 2. Pages glue + stylesheets + assets exist.
for f in style.css manual.css pygments.css CNAME .nojekyll assets/favicon.png; do
  [ -e "$OUT/$f" ] || { echo "FAIL: missing $f"; fail=1; }
done
[ "$(cat "$OUT/CNAME" 2>/dev/null)" = "docs.kastellan.dev" ] \
  || { echo "FAIL: CNAME is not docs.kastellan.dev"; fail=1; }

# 3. Palette tokens present in the copied stylesheet.
for token in '#4f46e5' '#fafbfd' '#0f172a'; do
  grep -qi -- "$token" "$OUT/style.css" \
    || { echo "FAIL: style.css missing palette token $token"; fail=1; }
done

# 4. No raw .md link targets leaked, and every local href/src resolves.
for p in "$OUT"/*.html; do
  if grep -qE '(href|src)="[^"]+\.md(#[^"]*)?"' "$p"; then
    echo "FAIL: $(basename "$p") contains an unrewritten .md link"; fail=1
  fi
  for ref in $(grep -oE '(href|src)="[^"]+"' "$p" | sed -E 's/^(href|src)="//; s/"$//' \
               | grep -vE '^(https?:|mailto:|tel:|data:|//|#)'); do
    clean="${ref%%#*}"; clean="${clean%%\?*}"
    if [ -n "$clean" ] && [ ! -e "$OUT/$clean" ]; then
      echo "FAIL: $(basename "$p") references missing local file: $clean"; fail=1
    fi
  done
done

# 5. HTML validity (same loud-skip pattern as check-site.sh).
if ! command -v tidy >/dev/null 2>&1; then
  echo "FAIL: tidy not installed (brew install tidy-html5 / apt install tidy)"; fail=1
elif ! tidy --version 2>/dev/null | grep -qE 'HTML Tidy.*[ .]5\.'; then
  echo "[SKIP] tidy is pre-HTML5 (Apple 2006 build) — HTML validity check skipped"
else
  for p in "$OUT"/*.html; do
    errs=$(tidy -qe "$p" 2>&1 | grep -c "Error:")
    [ "$errs" -eq 0 ] || { echo "FAIL: $(basename "$p") has $errs tidy error(s)"; fail=1; }
  done
fi

if [ "$fail" -eq 0 ]; then echo "OK: all manual checks passed"; else exit 1; fi
```

Make it executable:

```bash
chmod +x scripts/site/check-manual.sh
```

- [ ] **Step 4: Run it to verify it passes**

Run: `scripts/site/check-manual.sh`
Expected: `OK: all manual checks passed` (a single `[SKIP] tidy …` line is acceptable if modern tidy isn't installed). Exit code 0.

- [ ] **Step 5: Commit**

```bash
git add scripts/site/check-manual.sh
git commit -m "test(docs-site): check-manual.sh — structure, links, glue, validity"
```

---

### Task 5: GitHub Actions workflow + gitignore

**Files:**
- Create: `.github/workflows/docs.yml`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: `build_manual.py`, `check-manual.sh`.
- Produces: a CI workflow that builds with uv and deploys to GitHub Pages.

- [ ] **Step 1: Add `dist/` to `.gitignore`**

Add under the `# Logs / runtime artefacts` group (or its own group) in `.gitignore`:

```gitignore
# Generated docs site (built fresh in CI, never committed)
/dist/
```

- [ ] **Step 2: Write the workflow**

Create `.github/workflows/docs.yml`:

```yaml
name: docs-site

on:
  push:
    branches: [main]
    paths:
      - "docs/devel/manual/**"
      - "scripts/site/**"
      - "site/style.css"
      - "site/manual.css"
      - "site/assets/**"
      - ".github/workflows/docs.yml"
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: docs-pages
  cancel-in-progress: true

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install uv
        uses: astral-sh/setup-uv@v6
      - name: Install tidy (HTML validity check)
        run: sudo apt-get update && sudo apt-get install -y tidy
      - name: Verify the manual site
        run: scripts/site/check-manual.sh
      - name: Build the manual site
        run: uv run scripts/site/build_manual.py --out dist/manual
      - name: Upload Pages artifact
        uses: actions/upload-pages-artifact@v3
        with:
          path: dist/manual

  deploy:
    needs: build
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - name: Deploy to GitHub Pages
        id: deployment
        uses: actions/deploy-pages@v4
```

- [ ] **Step 3: Validate the workflow YAML locally**

Run: `uv run --with pyyaml python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/docs.yml')); print('YAML OK')"`
Expected: `YAML OK`

If `actionlint` is installed, also run: `actionlint .github/workflows/docs.yml` (expected: no output). If not installed, skip — the YAML parse above is sufficient for the plan.

- [ ] **Step 4: Confirm gitignore takes effect**

Run: `uv run scripts/site/build_manual.py --out dist/manual >/dev/null && git status --porcelain dist/`
Expected: empty output (dist/ is ignored). Then clean up: `rm -rf dist`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/docs.yml .gitignore
git commit -m "ci(docs-site): build with uv + deploy to GitHub Pages"
```

---

### Task 6: Close the loop — main-site links + operator docs

**Files:**
- Modify: `site/index.html`, `site/roadmap.html`, `site/security.html`, `site/contributing.html`
- Modify: `site/README.md`

**Interfaces:**
- Consumes: nothing new.
- Produces: a "Manual" nav link on every main-site page pointing at `https://docs.kastellan.dev`, the contributing-page manual reference updated, and documented operator steps. `scripts/site/check-site.sh` must still pass.

- [ ] **Step 1: Add the "Manual" nav link to all four pages**

In each of `site/index.html`, `site/roadmap.html`, `site/security.html`, `site/contributing.html`, the nav block contains:

```html
        <a href="contributing.html">Contributing</a>
        <a class="nav-cta" href="https://github.com/hherb/kastellan">GitHub</a>
```

Insert a Manual link between them so it reads:

```html
        <a href="contributing.html">Contributing</a>
        <a href="https://docs.kastellan.dev">Manual</a>
        <a class="nav-cta" href="https://github.com/hherb/kastellan">GitHub</a>
```

(On `contributing.html` the `Contributing` anchor carries `aria-current="page"`; keep that attribute as-is and insert the Manual link after it.)

- [ ] **Step 2: Update the contributing-page manual reference**

In `site/contributing.html`, find the card text:

```html
          <p class="card-text">The onboarding manual, cross-distro testing, macOS coverage. If a setup step surprised you, that&#8217;s a bug in the docs.</p>
```

Replace with a linked version pointing at the new docs site:

```html
          <p class="card-text">The <a href="https://docs.kastellan.dev">onboarding manual</a>, cross-distro testing, macOS coverage. If a setup step surprised you, that&#8217;s a bug in the docs.</p>
```

- [ ] **Step 3: Document the operator steps in `site/README.md`**

Append this section to `site/README.md`:

```markdown
## Manual site (docs.kastellan.dev) — operator setup (one-time)

The developer manual (`docs/devel/manual/*.md`) is rendered by
`scripts/site/build_manual.py` and deployed to GitHub Pages by
`.github/workflows/docs.yml` on every push to `main` that touches the manual,
the converter, or the shared styles. Local preview / verify:

    uv run scripts/site/build_manual.py --out /tmp/manual
    python3 -m http.server 8044 -d /tmp/manual
    scripts/site/check-manual.sh

One-time operator actions to bring `docs.kastellan.dev` live:

1. **GitHub** → repo **Settings → Pages** → Source: **GitHub Actions**.
   After the first `docs-site` workflow run, set **Custom domain** to
   `docs.kastellan.dev` and enable **Enforce HTTPS**.
2. **Cloudflare DNS** (the zone already lives here): add
   `CNAME  docs  →  hherb.github.io`, set to **DNS only (grey cloud)** so
   GitHub can issue the Let's Encrypt certificate. Once HTTPS is enforced and
   working, the proxy (orange cloud) may optionally be re-enabled with TLS mode
   **Full (strict)**.

The build emits a `CNAME` file (`docs.kastellan.dev`), so the custom domain
stays bound across deploys.
```

- [ ] **Step 4: Verify the main site still passes its checks**

Run: `scripts/site/check-site.sh`
Expected: `OK: all site checks passed` (a `[SKIP] tidy …` line is acceptable).

Also confirm the new links are present:
Run: `grep -c 'https://docs.kastellan.dev' site/index.html site/roadmap.html site/security.html site/contributing.html`
Expected: each file reports `1` (contributing.html reports `2` — nav link + card link).

- [ ] **Step 5: Commit**

```bash
git add site/index.html site/roadmap.html site/security.html site/contributing.html site/README.md
git commit -m "feat(site): link the main site to docs.kastellan.dev + operator docs"
```

---

## Self-Review

**Spec coverage:**
- §1 converter (Python/uv/PEP 723, markdown+pygments, extensions, template, link rewrite, manifest+drift-guard) → Tasks 1–2. ✓
- §2 styling (style.css untouched, new manual.css, pygments theme) → Tasks 2–3. ✓
- §3 output layout (html, css, pygments.css, assets, CNAME, .nojekyll) → Task 2 `build()`. ✓
- §4 deployment (docs.yml, setup-uv, upload/deploy-pages, triggers, concurrency, permissions) → Task 5. ✓
- §5 DNS + Pages operator steps → Task 6 README. ✓
- §6 close the loop (contributing link + Manual nav) → Task 6. ✓
- §7 verification (check-manual.sh) → Task 4, wired into CI in Task 5. ✓
- Out-of-scope items (search, dark mode, versioning) — correctly omitted. ✓

**Placeholder scan:** The only intentional placeholder is `site/manual.css`, created as a one-line stub in Task 2 (so the copy step has a source) and fully written in Task 3 — flagged at both ends. All code blocks are final, runnable content; no "TBD"/"handle edge cases"/"add validation"/dead-branch scaffolds remain.

**Type consistency:** Function names and signatures are stable across tasks — `rewrite_link`, `chapter_title`, `manifest_stems`, `validate_manifest`, `render_body`, `sidebar_html`, `page_html`/`_PAGE_TEMPLATE`, `build(out, src=…)`. Class names emitted by Task 2 (`.manual-layout`, `.manual-sidebar`, `.manual-toc`, `.manual-group`, `.manual-link`, `.manual-home`, `.active`, `.prose`, `.codehilite`) match exactly the selectors styled in Task 3 and asserted in Tasks 3–4. Stem list in `check-manual.sh` matches `manifest_stems()`.
