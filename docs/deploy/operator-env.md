# Operator environment settings

`kastellan-cli install` **regenerates** `~/.config/kastellan/kastellan.env` from
its CLI flags every time it runs. Any key you add to that file is dropped on the
next deploy, and any value you tune there reverts to the flag default.

Put operator settings in `~/.config/kastellan/kastellan.env.local` instead.

- The installer **never writes** that file.
- Its values **win**: both backends apply environment files in order, later
  overriding earlier, and the overlay is listed second.
- It is optional — if it does not exist, nothing changes.
- Give it `0600` permissions, like the file it sits beside.

## Example

```sh
cat > ~/.config/kastellan/kastellan.env.local <<'EOF'
KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0-ctx64k
KASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443
KASTELLAN_MAIL_TOKEN_FILE=/home/you/.config/kastellan/mail-token
KASTELLAN_MATRIX_PEERS=@you:your-homeserver
EOF
chmod 600 ~/.config/kastellan/kastellan.env.local
```

Every key above is one the installer has no flag for, or one it would reset —
which is exactly why they belong here rather than in `kastellan.env`.

## What the file may contain

The grammar is systemd's `EnvironmentFile=`, and the macOS backend implements
the same subset, so one overlay behaves identically on both platforms:

| you write | the daemon gets |
| --- | --- |
| `A=plain` | `plain` |
| `A="a b"` or `A='a b'` | `a b` — surrounding quotes are stripped |
| `A='  x  '` | `  x  ` — whitespace *inside* quotes is kept |
| `A=  c  ` | `c` — whitespace outside them is trimmed |
| `A=f"g` | `f"g` — a quote that is not the first character is literal |
| `A="a` | `a` — a leading quote is stripped even with no closing one |
| `# A=x`, `; A=x` | ignored, both are comments |
| `export A=x` | **ignored** — write `A=x`, not shell syntax |
| `A="a b" # note` | `a b# note` — see the warning below |
| `A="a b"x y` | `a bx y` — a value continues past its closing quote |
| `A="a" "b" c` | `abc` — adjacent quoted sections concatenate, no separator |

> ⚠️ **`#` starts a comment only at the start of a line.** Appending one to a
> value does not comment anything out: systemd keeps the text as part of the
> value and merely drops the whitespace run right after the closing quote, so
> `A="a b" # note` sets `A` to `a b# note` — not `a b`. This is systemd's
> behaviour, matched deliberately (measured, [#552]) rather than a kastellan
> quirk, and it applies on macOS too. **Put comments on their own line.**

> ⚠️ **Only a space or a tab counts as whitespace here.** A non-breaking space
> (U+00A0) does not — it stays in the value, and a key carrying one declares
> **nothing at all**. This bites when a line is pasted out of rendered
> documentation, a web page or a chat window, which is exactly how the block
> above might reach your file. The symptom is a startup line naming a key you
> believe you set:
>
> ```
> operator overlay NOT fully applied: … — 1 of 5 keys did not reach this process: KASTELLAN_MAIL_ENDPOINT
> ```
>
> or an `N lines ignored` count where you expected none. `cat -A` shows it:
> a normal space prints as a space, a non-breaking one as `M-BM-`. Retype the
> line rather than re-pasting it.

[#552]: https://github.com/hherb/kastellan/issues/552

Backslash line-continuations and C-style escapes inside quotes are *not*
supported; a value needing either is out of contract on both platforms.

## When it takes effect

| platform | mechanism | an edit takes effect |
| --- | --- | --- |
| Linux (systemd) | a second `EnvironmentFile=` directive | next `systemctl --user restart kastellan-core` |
| macOS (launchd) | folded into the plist at install time | next `kastellan-cli install` |

launchd has no `EnvironmentFile=` directive, so the values are baked in when the
plist is written. Surviving a reinstall works identically on both platforms; only
the refresh moment differs.

**On macOS, always re-run `install` with the same flags you passed originally.**
A bare `kastellan-cli install` regenerates `kastellan.env` from flag *defaults*,
so it reverts `--llm-model`, `--llm-url`, `--embedding-model`, `--pg-bin-dir`,
and drops the whole `KASTELLAN_MATRIX_*` block (which is emitted only when both
`--matrix-*` flags are passed). On Linux `scripts/upgrade_from_git.sh` scavenges
the Matrix block back before installing; the macOS path has no such guard.

## A supervised guard backend

The guard tier's backend (`llama-server` serving Shieldstral) can run as a
supervised unit, `kastellan-guard`, so it comes back after a reboot along with
the rest of `kastellan.target`. Without it the guard is a hand-started process,
and because the daemon refuses to boot when a configured guard is unreachable,
a reboot leaves the **whole agent** down until someone restarts it by hand
([#609]).

Add the two paths beside the guard settings you already have:

```sh
KASTELLAN_LLM_GUARD_URL=http://127.0.0.1:8081/v1
KASTELLAN_LLM_GUARD_MODEL=shieldstral
KASTELLAN_LLM_GUARD_TAU=0.79552656
KASTELLAN_GUARD_SERVER_BIN=/home/you/.local/bin/llama-server
KASTELLAN_GUARD_SERVER_MODEL=/home/you/models/shieldstral/Shieldstral-1.0-3B-Q8_0.gguf
```

then re-run `kastellan-cli install`. The unit's port and model alias are taken
from `KASTELLAN_LLM_GUARD_URL` and `KASTELLAN_LLM_GUARD_MODEL`, so they always
match what the daemon asks for; the URL must be `http://127.0.0.1:<port>` or
`http://localhost:<port>`. The unit runs
`llama-server -m <model> --alias <model name> --host 127.0.0.1 --port <port> -c 131072 -ngl 99`
and logs to `~/.local/state/kastellan/kastellan-guard.{out,err}`.

- **These two keys are read at install time on both platforms** (they become
  the unit's command line), so a change needs another `install` — not just a
  restart.
- **Stop a hand-started guard first**, or the unit cannot bind the port and
  crash-loops.
- Removing both keys and re-running `install` removes the unit. Setting one
  without the other is refused.

Core starts after the guard, and at boot it waits up to 60 s for a guard that is
still loading its model (connection refused or HTTP 503) before it refuses.

[#609]: https://github.com/hherb/kastellan/issues/609

## Confirming the overlay was actually found

Two places tell you, so a typo in the path cannot pass as "no overlay wanted".

**At install**, one line naming the path that was read:

```
operator overlay: /home/you/.config/kastellan/kastellan.env.local (5 keys) — applied after kastellan.env, so these values win
operator overlay: none at /home/you/.config/kastellan/kastellan.env.local — tuned settings put there survive a reinstall; see docs/deploy/operator-env.md
```

**At daemon startup**, whether the values reached the running process:

```
operator overlay applied: /home/you/.config/kastellan/kastellan.env.local (5 keys, all present in this process)
operator overlay NOT fully applied: /home/you/.config/kastellan/kastellan.env.local — 2 of 5 keys did not reach this process: KASTELLAN_MAIL_ENDPOINT, KASTELLAN_LLM_TIMEOUT_MS
```

The daemon log is JSON, so these arrive as the `"message"` field of a log record
rather than as bare lines — `grep 'operator overlay'` still finds them.

The startup line compares each key you declared against the daemon's own
environment, so it is a real end-to-end check rather than a statement that a file
exists. `NOT fully applied` means one of:

- the overlay is listed *before* the generated file, so `kastellan.env` wins;
- a unit-level `Environment=` directive, or another `EnvironmentFile=` listed
  after the overlay, already set the key at exec time;
- on Linux, you edited the overlay but have not restarted the unit
  (`systemctl --user restart kastellan-core`) — systemd reads `EnvironmentFile=`
  at start, not on change;
- on Linux, the `EnvironmentFile=` directive was dropped by systemd (check
  `journalctl --user -u kastellan-core` for `path is not absolute, ignoring`);
- on macOS, you edited the overlay but have not re-run `install` since — launchd
  bakes the values in at install time, so an edit alone changes nothing.

Two more shapes are reported, both of which used to pass as healthy:

```
operator overlay: /home/you/.config/kastellan/kastellan.env.local declares NO keys — nothing in it can apply; note that `export KEY=value` and lines without `=` are ignored
operator overlay applied: /home/you/.config/kastellan/kastellan.env.local (5 keys, all present in this process, 1 line ignored (line 6))
```

`export KEY=value` is shell syntax; systemd's env-file grammar has no `export`,
so such a line declares nothing. A file made entirely of them is not an empty
overlay, it is a **broken** one, and the count alone would have read `0 keys`
without saying why. `N lines ignored` names the line numbers — never their
contents — for the same reason a key count is not enough on its own: without it,
one mistyped line in a six-key file just reads as a clean five.

An overlay that exists but cannot be read is reported separately and never
treated as empty:

```
operator overlay: /home/you/.config/kastellan/kastellan.env.local UNREADABLE (Permission denied (os error 13)) — kastellan cannot confirm any of its values applied
```

No line ever prints a **value** — only key names, line numbers and counts. The
daemon log is a plaintext file without `audit_log`'s role gating.

If you see `none at …` when you believe you wrote the file, compare the path in
the message with where your file actually landed; a heredoc written to
`~/.config/kastellan.env.local` (missing the `kastellan/` directory component) is
the common miss. Daemon logs are at `~/.local/state/kastellan/*.out`.

## If an install reports dropped keys

```
warning: install is regenerating /home/you/.config/kastellan/kastellan.env
  dropped: KASTELLAN_MAIL_ENDPOINT
  changed: KASTELLAN_LLM_LOCAL_MODEL
  previous file saved to /home/you/.config/kastellan/kastellan.env.bak
  to keep these across future installs, move them into /home/you/.config/kastellan/kastellan.env.local —
  the installer never writes that file, and its values override this one;
  then `systemctl --user restart kastellan-core` to pick them up.
```

(The last line differs on macOS, where it tells you to re-run `install` instead.)

Those keys were living in the generated file rather than in the overlay, which is
why the regeneration reached them. Copy them out of the `.bak` into
`kastellan.env.local`, then apply it as the table above describes.

**Move them now, not later.** The backup is written only when something is
actually being lost, so an ordinary clean install will not touch it — but a
*second* install that also drops or changes keys writes its own backup, and by
then the live file is the already-stripped one. To keep the copy that matters,
the installer never overwrites an existing backup: the first one stays at
`kastellan.env.bak` and later ones land beside it as `kastellan.env.bak.1`,
`.bak.2`, and so on. The warning always names the file it actually wrote. Prune
them yourself once the values are safely in the overlay.

If the installer cannot *read* `kastellan.env` at all — a stray non-UTF-8 byte,
wrong permissions — it refuses to continue rather than overwriting a file it
could neither diff nor back up. Move the file aside and re-run.

## Before you `uninstall --purge`

`--purge` deletes `~/.config/kastellan/` outright — that is the directory
`kastellan.env.local` and every `kastellan.env.bak*` live in, so they all go with
it. The installer's own health check points operators at `uninstall --purge`
then reinstall as the fix for a stale-cluster role mismatch; the purge summary
names both files, but by then it is too late. Copy anything you need out of
`kastellan.env.local` (and the backups, if you have not already moved their
contents over) before running `--purge`.
