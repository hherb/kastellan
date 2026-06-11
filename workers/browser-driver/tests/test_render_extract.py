from kastellan_worker_browser_driver.render import (
    extract_render_result,
    MAX_HTML_BYTES,
    MAX_TEXT_CHARS,
)


def test_extracts_title_text_and_html():
    html = (
        "<html><head><title>Hi</title></head>"
        "<body><article><p>Hello world body.</p></article></body></html>"
    )
    out = extract_render_result(html=html, final_url="https://x.test/", status=200, title="Hi")
    assert out["final_url"] == "https://x.test/"
    assert out["status"] == 200
    assert out["title"] == "Hi"
    assert "Hello world body." in out["text"]
    assert out["html"].startswith("<")


def test_html_byte_cap_truncates_on_char_boundary():
    big = "<html><body>" + ("é" * MAX_HTML_BYTES) + "</body></html>"  # 2-byte chars
    out = extract_render_result(html=big, final_url="https://x.test/", status=200, title="t")
    assert len(out["html"].encode("utf-8")) <= MAX_HTML_BYTES
    # Truncation must not split a multibyte char (round-trips cleanly).
    out["html"].encode("utf-8")


def test_text_char_cap():
    html = "<html><body><p>" + ("a" * (MAX_TEXT_CHARS + 50)) + "</p></body></html>"
    out = extract_render_result(html=html, final_url="https://x.test/", status=200, title="t")
    assert len(out["text"]) <= MAX_TEXT_CHARS


def test_degenerate_html_does_not_raise():
    out = extract_render_result(html="", final_url="https://x.test/", status=204, title="")
    assert out["text"] == ""
    assert out["status"] == 204
