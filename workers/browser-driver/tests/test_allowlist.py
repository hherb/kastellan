"""Tests for the in-worker host:port allowlist (subresource enforcement)."""
import json

import pytest

from kastellan_worker_browser_driver.allowlist import HostAllowlist


def al(*entries):
    return HostAllowlist.from_endpoints(list(entries))


def test_exact_host_any_port_when_bare():
    a = al("example.com")
    assert a.is_allowed_endpoint("example.com", 443)
    assert a.is_allowed_endpoint("example.com", 8443)  # bare host = any port
    assert not a.is_allowed_endpoint("evil.com", 443)


def test_port_scoped_entry():
    a = al("example.com:443")
    assert a.is_allowed_endpoint("example.com", 443)
    assert not a.is_allowed_endpoint("example.com", 8443)  # wrong port


def test_suffix_entry_matches_subdomains():
    a = al(".example.com")
    assert a.is_allowed_endpoint("example.com", 443)
    assert a.is_allowed_endpoint("cdn.example.com", 443)
    assert not a.is_allowed_endpoint("notexample.com", 443)


def test_case_insensitive_host():
    a = al("Example.COM")
    assert a.is_allowed_endpoint("example.com", 443)


def test_bracketed_ipv6_with_port():
    a = al("[::1]:8080")
    assert a.is_allowed_endpoint("::1", 8080)
    assert not a.is_allowed_endpoint("::1", 443)


def test_bare_ipv6_is_not_missplit():
    # A bare IPv6 literal has multiple colons → treated as host-only (any port).
    a = al("::1")
    assert a.is_allowed_endpoint("::1", 443)


def test_bad_port_fails_closed():
    # A non-u16 port makes the whole entry a dead host token (Exact
    # "example.com:99999") that no real host lookup matches — so the typo can
    # only fail to permit, never widen the grant for the real host.
    a = al("example.com:99999")
    assert not a.is_allowed_endpoint("example.com", 443)
    assert not a.is_allowed_endpoint("example.com", 99999)


def test_empty_env_permits_nothing():
    for raw in ["", "   ", "[]"]:
        a = HostAllowlist.from_env_json(raw)
        assert a.is_empty() or not a.is_allowed_endpoint("example.com", 443)
        assert not a.is_allowed_endpoint("example.com", 443)


def test_from_env_json_parses_array():
    a = HostAllowlist.from_env_json(json.dumps(["a.test:443", "b.test"]))
    assert a.is_allowed_endpoint("a.test", 443)
    assert not a.is_allowed_endpoint("a.test", 80)  # port-scoped
    assert a.is_allowed_endpoint("b.test", 80)      # bare = any port


def test_from_env_json_rejects_non_array():
    with pytest.raises((ValueError, json.JSONDecodeError)):
        HostAllowlist.from_env_json('{"not": "an array"}')


def test_blank_and_lone_dot_entries_skipped():
    a = al("", "  ", ".", "good.test")
    assert a.is_allowed_endpoint("good.test", 443)
    assert not a.is_allowed_endpoint("", 443)
