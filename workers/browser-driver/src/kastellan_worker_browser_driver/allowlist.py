"""Host:port allowlist for in-worker subresource enforcement.

A Python port of ``workers/web-common`` ``HostAllowlist::from_endpoints`` /
``is_allowed_endpoint`` semantics, so the browser worker self-enforces
``KASTELLAN_BROWSER_DRIVER_ALLOWLIST`` per navigation **and per subresource**
via Playwright request interception.

This is **defense in depth**: the hard egress boundary is the jail netns (or,
once egress slice #2 lands, the egress proxy). In the dev-only direct-net
posture (issue #263) this in-worker check is the *only* egress control, so it
must be solid — hence the faithful port-of-the-Rust-matcher + its own tests.

Pure; unit-tested without a browser.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Optional


@dataclass(frozen=True)
class _Rule:
    """One allowlist entry: a host matcher + an optional port scope."""

    # Lowercased host token. `suffix=True` means it matches the domain itself
    # and any subdomain (a leading-dot entry like ".example.com"); otherwise an
    # exact host match.
    host: str
    suffix: bool
    # Explicit port (host:port entry) or None for a bare host (matches any port).
    port: Optional[int]

    def matches_host(self, host: str) -> bool:
        if self.suffix:
            return host == self.host or host.endswith("." + self.host)
        return host == self.host


def _parse_host(token: str) -> Optional[tuple[str, bool]]:
    """Parse a bare host token into (host, is_suffix). None for empty/lone-dot."""
    e = token.strip().lower()
    if not e:
        return None
    if e.startswith("."):
        domain = e[1:]
        if not domain:
            return None
        return (domain, True)
    return (e, False)


def _split_host_port(entry: str) -> tuple[str, Optional[int]]:
    """Split ``host[:port]`` into (host, port|None).

    Mirrors web-common: handles bracketed IPv6 (``[::1]:443`` / ``[::1]``),
    ``host:443``, bare IPv6 (``::1`` — no port), and bare hosts. A trailing
    ``:<digits>`` is a port only when the host part has no other colon, so a
    bare IPv6 literal is never mis-split. A non-u16 port is fail-closed: the
    whole string becomes a (dead) host token rather than widening the grant.
    """
    e = entry.strip()
    if e.startswith("["):
        rest = e[1:]
        if "]" in rest:
            host, after = rest.split("]", 1)
            port = None
            if after.startswith(":"):
                port = _parse_port(after[1:])
            return (host, port)
    # rsplit on the last colon; only a port if the host part has no colon.
    if ":" in e:
        host, _, port_str = e.rpartition(":")
        if host and ":" not in host:
            port = _parse_port(port_str)
            if port is not None:
                return (host, port)
            # Bad port → fail closed: treat the whole entry as a dead host.
            return (e, None)
    return (e, None)


def _parse_port(s: str) -> Optional[int]:
    if s.isdigit():
        v = int(s)
        if 0 <= v <= 65535:
            return v
    return None


class HostAllowlist:
    """Port-scoped host allowlist (the ``from_endpoints`` shape)."""

    def __init__(self, rules: list[_Rule]):
        self._rules = rules

    @classmethod
    def from_endpoints(cls, entries: list[str]) -> "HostAllowlist":
        rules: list[_Rule] = []
        for entry in entries:
            host_tok, port = _split_host_port(entry)
            parsed = _parse_host(host_tok)
            if parsed is not None:
                host, suffix = parsed
                rules.append(_Rule(host=host, suffix=suffix, port=port))
        return cls(rules)

    @classmethod
    def from_env_json(cls, raw: str) -> "HostAllowlist":
        """Parse the ``KASTELLAN_BROWSER_DRIVER_ALLOWLIST`` JSON array.

        A blank/empty env yields an **empty** allowlist that permits nothing —
        fail-closed, matching the worker's containment posture.
        """
        if not raw or not raw.strip():
            return cls([])
        entries = json.loads(raw)
        if not isinstance(entries, list):
            raise ValueError("allowlist env must be a JSON array of strings")
        return cls.from_endpoints([str(e) for e in entries])

    def is_allowed_endpoint(self, host: str, port: int) -> bool:
        """True iff some rule permits ``host`` on ``port`` (bare host = any port)."""
        h = host.strip().lower()
        return any(
            r.matches_host(h) and (r.port is None or r.port == port)
            for r in self._rules
        )

    def is_empty(self) -> bool:
        return not self._rules
