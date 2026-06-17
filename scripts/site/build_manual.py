# /// script
# requires-python = ">=3.11"
# dependencies = ["markdown==3.7", "pygments==2.18.0"]
# ///
"""Render docs/devel/manual/*.md into a styled static site for docs.kastellan.dev.

Usage:
    uv run scripts/site/build_manual.py --out dist/manual
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
from pathlib import Path

import markdown as md
from pygments.formatters import HtmlFormatter

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
