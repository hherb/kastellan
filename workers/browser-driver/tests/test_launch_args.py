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


def test_disable_dev_shm_usage_is_pinned():
    """`--disable-dev-shm-usage` must stay in the defaults — it is load-bearing.

    The Firecracker micro-VM guest has NO /dev/shm: the kernel auto-mounts
    devtmpfs on /dev (CONFIG_DEVTMPFS_MOUNT=y), but devtmpfs provides device
    nodes only, and microvm-init mounts just /proc, /sys and /tmp. Without this
    flag Chromium aborts at startup there with "Creating shared memory in
    /dev/shm/... failed: No such file or directory" (measured both ways — see
    the micro-VM rootfs design spec S10.1).

    The other tests in this file compare build_launch_args() output AGAINST
    DEFAULT_LAUNCH_ARGS, so they stay green if the flag is deleted from that
    list. This asserts on the flag itself, so removing it fails loudly here
    rather than silently at VM boot.
    """
    assert "--disable-dev-shm-usage" in DEFAULT_LAUNCH_ARGS
    # And it must survive into the force-routed arg set, which is the
    # configuration the micro-VM actually runs.
    assert "--disable-dev-shm-usage" in build_launch_args(54321)
