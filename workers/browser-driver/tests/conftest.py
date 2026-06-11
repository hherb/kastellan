import pytest


class FakeRenderer:
    """Duck-typed stand-in for the Playwright drive.

    `.render(...)` returns a canned result dict and records its call args; set
    `.raise_exc` to simulate a navigation/render failure.
    """

    def __init__(self, result=None, raise_exc=None):
        self._result = result or {
            "final_url": "https://x.test/",
            "status": 200,
            "title": "T",
            "text": "body text",
            "html": "<html></html>",
        }
        self.raise_exc = raise_exc
        self.calls = []

    def render(self, *, url, timeout_ms, wait_until):
        self.calls.append({"url": url, "timeout_ms": timeout_ms, "wait_until": wait_until})
        if self.raise_exc:
            raise self.raise_exc
        return self._result


@pytest.fixture
def fake_renderer():
    return FakeRenderer()


@pytest.fixture
def renderer_factory():
    """Build a FakeRenderer with custom args (e.g. raise_exc) inside a test."""
    return FakeRenderer
