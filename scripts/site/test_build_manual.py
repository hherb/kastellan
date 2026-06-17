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
