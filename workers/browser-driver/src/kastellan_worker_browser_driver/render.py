"""Render-result extraction (pure) + the Playwright drive (Phase 2).

`extract_render_result` is pure: given the post-JS HTML + navigation metadata it
produces the `browser.render` result dict (readability text + capped final
HTML). It is unit-tested without a browser. The actual Playwright launch
(`render`) is added in the Phase-2 plan behind the same result shape so the seam
stays testable with a fake (see `server.Server(renderer=…)`).
"""
from typing import Any

from readability import Document
from lxml import html as lxml_html

# Wire-contract caps. Bumping any requires updating the Rust side + spec table.
MAX_HTML_BYTES = 5 * 1024 * 1024   # 5 MiB, mirrors web-fetch
MAX_TEXT_CHARS = 200_000           # post-readability text char cap


def _truncate_utf8(s: str, max_bytes: int) -> str:
    """Truncate `s` so its UTF-8 encoding is <= max_bytes, never splitting a char."""
    encoded = s.encode("utf-8")
    if len(encoded) <= max_bytes:
        return s
    # Decode the byte-prefix, dropping any partial trailing char.
    return encoded[:max_bytes].decode("utf-8", errors="ignore")


def _text_from_html(html: str) -> str:
    """Best-effort readable text: readability summary, falling back to raw text."""
    try:
        summary_html = Document(html).summary(html_partial=True)
        if summary_html.strip():
            return lxml_html.fromstring(summary_html).text_content()
    except Exception:
        # Readability can throw on degenerate DOMs; fall through to raw extraction.
        pass
    try:
        return lxml_html.fromstring(html).text_content()
    except Exception:
        return ""


def extract_render_result(*, html: str, final_url: str, status: int, title: str) -> dict[str, Any]:
    """Build the `browser.render` result from post-JS HTML + navigation metadata."""
    text = " ".join(_text_from_html(html).split())[:MAX_TEXT_CHARS]
    return {
        "final_url": final_url,
        "status": status,
        "title": title,
        "text": text,
        "html": _truncate_utf8(html, MAX_HTML_BYTES),
    }
