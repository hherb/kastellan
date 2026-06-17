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
