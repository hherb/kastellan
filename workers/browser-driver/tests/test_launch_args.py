"""build_launch_args wires Chromium's --proxy-server only when force-routed."""
from kastellan_worker_browser_driver.render import build_launch_args, DEFAULT_LAUNCH_ARGS


def test_no_proxy_when_port_none():
    args = build_launch_args(None)
    assert args == DEFAULT_LAUNCH_ARGS
    assert not any(a.startswith("--proxy-server") for a in args)


def test_proxy_server_and_bypass_when_port_given():
    args = build_launch_args(54321)
    assert "--proxy-server=127.0.0.1:54321" in args
    # Force loopback destinations through the proxy too (remove implicit bypass).
    assert "--proxy-bypass-list=<-loopback>" in args
    for base in DEFAULT_LAUNCH_ARGS:
        assert base in args
