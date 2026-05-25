# 2 — Dev environment: Linux (Ubuntu 24.04+)

This guide sets up a working dev environment on Ubuntu 24.04 or later. Other
Debian-based distributions work; Fedora/Arch users will need to adapt the
`apt` commands.

---

## Step 1 — Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version    # should print 1.77 or later
```

Cargo (the build tool) is installed alongside Rust. The `source` line is
needed in every new shell session, or you can add it to your `~/.bashrc` /
`~/.zshrc`.

---

## Step 2 — Install system dependencies

```sh
sudo apt update
sudo apt install -y \
    build-essential pkg-config git \
    bubblewrap \
    libssl-dev
```

`bubblewrap` (`bwrap`) is the Linux container tool that isolates each worker
process. It is required to run the sandbox tests.

---

## Step 3 — Fix the AppArmor restriction (Ubuntu 24.04 only)

Ubuntu 24.04 restricts unprivileged user namespaces by default. Without a
workaround, bwrap cannot create its own jail and all sandbox tests silently
skip. Install the AppArmor profile once:

```sh
sudo scripts/linux/install-bwrap-apparmor-profile.sh
```

This is the same profile pattern used by Flatpak. You only need to do this
once per machine.

To confirm it worked:

```sh
cargo test -p hhagent-sandbox -- --nocapture 2>&1 | grep -E 'SKIP|ok|FAILED'
```

You should see `ok` lines, not `[SKIP]` lines. If you still see `[SKIP]`, the
AppArmor profile did not install correctly.

---

## Step 4 — Install and configure Postgres

The project ships a helper script:

```sh
sudo scripts/linux/install-postgres.sh
```

This installs PostgreSQL 18 from the official PGDG repository and stops the
default system-level service so it does not conflict with the per-user
instance the agent manages itself.

Initialise a per-user cluster:

```sh
cargo run -p hhagent-db --bin hhagent-db-init
```

This creates a cluster in `~/.local/share/hhagent/postgres/`, configured for
localhost-only Unix socket connections with peer auth.

---

## Step 5 — First build

```sh
source "$HOME/.cargo/env"
cargo build --workspace
```

The first build downloads and compiles all dependencies. On a typical machine
this takes 2–5 minutes. Subsequent incremental builds are fast.

---

## Step 6 — Run the test suite

```sh
cargo test --workspace -- --nocapture
```

Some integration tests spin up a real Postgres cluster per test. They are
slow (30–60 s total) but reliable. A healthy result looks like:

```
test result: ok. 998 passed; 0 failed; 3 ignored
```

The 3 ignored tests require hardware or services not present in a standard dev
setup (real GLiNER model, Apple `container` CLI).

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `bwrap: creating new session` error in tests | AppArmor restriction | Re-run step 3 |
| `[SKIP]` lines in sandbox tests | Same as above | Re-run step 3 |
| `connection refused` in Postgres tests | DB not running | `cargo run -p hhagent-db --bin hhagent-db-init` |
| `command not found: cargo` | Rust env not sourced | `source "$HOME/.cargo/env"` |
