# Operator env overlay — design

**Issue:** [#458](https://github.com/hherb/kastellan/issues/458)
**Date:** 2026-08-08
**Status:** approved, ready for an implementation plan

---

## The problem, with live evidence from today

`kastellan-cli install` regenerates `~/.config/kastellan/kastellan.env` from CLI
flags only (`install::plan::render_env_file`). `scripts/upgrade_from_git.sh`
re-passes just `--matrix-homeserver-url` / `--matrix-user`. Every other
hand-added key is therefore dropped on every deploy, and every installer-owned
key whose value was tuned by hand silently reverts to the flag default.

This is not theoretical. On 2026-08-08 the deployed bot answered
*"summarize my most recent email"* with:

> I am unable to summarize your most recent email because I do not have
> permission to access your files or directories.

The cause was not a code defect. `kastellan.env` had been regenerated on
2026-08-06 and never repaired, so:

```
KASTELLAN_MAIL_ENDPOINT             ABSENT   -> mail.* tools never register
KASTELLAN_MAIL_TOKEN_FILE           ABSENT
KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA  ABSENT   -> MITM cannot reach self-signed localmail
KASTELLAN_LLM_TIMEOUT_MS            ABSENT
KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0   (reverted from the -ctx64k tag)
```

With no `mail.*` tool in the registry the planner never saw one and fell back to
`shell-exec` filesystem probing, which the sandbox correctly denied. **The system
behaved perfectly on top of a broken config, and the only symptom was a wrong
answer.** The capability was gone for two days.

A key-set diff of the pre-install snapshot against the regenerated file confirmed
the failure is exactly two shapes, and nothing else:

```
LOST:   KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA, KASTELLAN_LLM_TIMEOUT_MS,
        KASTELLAN_MAIL_ENDPOINT, KASTELLAN_MAIL_TOKEN_FILE
DRIFT:  KASTELLAN_LLM_LOCAL_MODEL
```

The handover has carried a manual re-add procedure for weeks. A procedure that
must be remembered on every deploy is not a fix.

## Measured facts (verified on the DGX, not recalled)

The design depends on systemd's env precedence, so it was measured on the
deployment target with a transient unit rather than taken from memory — the same
discipline #521 (`PgListener`) and #525 (`PersistentWorker`) each had to learn
the hard way:

| behaviour | result |
| --- | --- |
| `EnvironmentFile=` vs `Environment=` | **`EnvironmentFile=` wins** |
| two `EnvironmentFile=` lines | **later wins** |
| `EnvironmentFile=-<missing>` | unit starts normally, exit 0 |

Two consequences:

1. A second env file listed *after* the generated one fixes **both** failure
   shapes above — lost keys and reverted values — with one mechanism.
2. The env file the installer regenerates **can already override the unit's own**
   `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1`. The claim at
   `supervisor/src/specs.rs:90` that operators can override force-routing via the
   `EnvironmentFile` is therefore correct and now confirmed. That is exactly why
   the override must never be silent (Part 3).

## Part 1 — `kastellan.env.local`, an overlay the installer never writes

`ServiceSpec.environment_file: Option<PathBuf>` becomes:

```rust
pub struct EnvFileRef {
    pub path: PathBuf,
    /// A missing file is not an error: renders systemd's `-` prefix, and the
    /// launchd backend skips it rather than failing the install.
    pub optional: bool,
}

pub environment_files: Vec<EnvFileRef>,   // applied in order; later wins
```

`install::plan::build_specs` sets
`[{kastellan.env, required}, {kastellan.env.local, optional}]`. `install` never
writes the second file.

**Why a `Vec` and not a second named field.** The precedence rule then lives in
one ordered fold shared by both backends, rather than as a sentence repeated in
two doc comments that can drift apart.

**Why `optional` is a real field and not a convention.** The launchd backend
today does `fs::read_to_string(env_file)?` and hard-errors when the file is
absent. Without a modelled `optional`, every macOS install lacking a `.local`
would fail.

**Backend rendering.**

- **systemd** (`systemd_user/builder.rs`): one `EnvironmentFile=` line per entry,
  in order, `-`-prefixed when `optional`.
- **launchd** (`launchd_agents.rs`): fold each file in order into the plist's
  `EnvironmentVariables`, later winning; skip an absent *optional* file; still
  error on an absent *required* one.

**One env-file grammar, in one `cfg`-free home.** `parse_env_file` and
`merge_env` currently sit in `launchd_agents::builders` as `pub(super)`. The
installer's diff (Part 2) needs the same grammar and `core` already depends on
`kastellan-supervisor`. They lift into a new `cfg`-free
`supervisor/src/env_file.rs`, used by launchd's fold and by the installer.

This follows #511's `atomic_write` precedent for its stated reason: shared code
is compiled and tested on **both** hosts, per-backend code is invisible to CI
(there is no macOS job at all). A second parser for one file format is the drift
shape #479 and #520 each cost a round.

**The lift buys more than symmetry.** `systemd_user` is
`#[cfg(target_os = "linux")]` and `launchd_agents` is
`#[cfg(target_os = "macos")]`, so `parse_env_file` and `merge_env` — sitting
inside the launchd module — were **never compiled on Linux and their tests never
ran there**, including on the DGX and in CI. Moving them to a `cfg`-free module
is what makes the one env-file grammar actually covered on both hosts.

**Consequence for gating, stated so a count mismatch is not misread.** Because
each backend compiles on only one host, changes to `launchd_agents.rs` are
invisible to the DGX and changes to `systemd_user.rs` are invisible to the Mac —
the [[cfg-linux-e2e-deadcode-dgx-clippy]] blindness, in both directions. Every
task touching a backend must be gated on **both** hosts, and the two hosts will
legitimately report **different test counts**.

**Documented platform asymmetry.** On systemd the overlay is re-read at every
service start, so an edit takes effect on `restart`. On launchd the pairs are
baked into the plist at *install* time, so an edit needs a re-install. The
guarantee #458 asks for — survives reinstall — holds identically on both; only
the refresh moment differs, and it is inherent, since launchd has no
`EnvironmentFile=` directive. This goes in the `EnvFileRef` doc and the operator
docs. An unstated platform difference here is the #508 parity-break shape.

## Part 2 — name what is being dropped, and keep a copy

**A pure diff.** New `core/src/install/env_diff.rs`:

```rust
pub struct EnvDiff { pub lost: Vec<String>, pub changed: Vec<String> }  // key names
pub fn diff_env_files(old: &str, new: &str) -> EnvDiff
```

Built on the lifted `env_file` grammar, so commented lines
(`# KASTELLAN_TIMEZONE=…`, which `render_env_file` emits) are ignored rather than
reported. Keys present only in the *new* file are not reported — they are the
installer doing its job.

Two edge cases the grammar decides for us, both correctly, and both worth stating
so they are not later mistaken for bugs:

- **A key the operator uncommented is reported as lost.** `render_env_file`
  writes `KASTELLAN_TIMEZONE` and `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`
  commented out. If the operator uncommented one, the old file has the key and
  the new file does not (a comment is not a key), so it is reported `lost` — which
  is exactly right, because the value *is* being lost.
- **A fresh install has nothing to diff.** When `kastellan.env` does not yet
  exist there is no diff, no warning and no backup. The warning is about
  overwriting an operator's file, not about writing one.

`install/run.rs` reads the file it is about to overwrite, diffs it against the
freshly rendered content, and warns per key, naming `kastellan.env.local` as the
fix. This is the same diff that identified today's failure by hand.

**A backup, written only when the diff is non-empty.** `kastellan.env.bak`
beside the file. It earns its place twice: it makes the transition safe for
installs whose tuned keys are still in `kastellan.env` (every existing install),
and it lets the warning name **key names only, never values** — the operator
reads the values from the backup. That keeps the install transcript free of
anything that might one day be a secret while leaving the warning actionable.
Gating the backup on a non-empty diff stops a later clean install from silently
clobbering the one backup that mattered.

## Part 3 — force-routing off becomes loud

`core/src/main.rs:191` is `if let Some(fr) = force_routing.as_ref()` with **no
`else`**. That is the silence: with force-routing off, host workers fall back to
`--share-net` with only the in-worker allowlist, and nothing says so at any level.

It gains a `warn!` plus an `egress.force_routing_disabled` audit row, so the
condition reaches the oversight record rather than only a plaintext log.

The log phrase is a `const`, asserted through the const. This project has paid
for that lesson three times (#516, #525, #524): the moment any operator-facing
text tells someone to grep for a phrase, a literal typed twice starts drifting.

## Non-goals

- **No change to force-routing defaults.** Still ON in the supervised deployment,
  still fail-closed on a missing proxy binary.
- **No auto-migration of existing keys into `.local`.** The installer cannot
  distinguish "the operator tuned this deliberately" from "this should be
  regenerated", so it names what it is dropping and leaves the decision to the
  operator. Guessing intent here would be a worse failure than the one being
  fixed.
- **No new secret handling.** Values stay out of the install transcript; the
  backup carries them at the file's existing `0600` permissions.

## Testing

Pure first:

- `diff_env_files`: a lost key; a changed value; commented lines ignored; a
  key present only in `new` **not** reported; blank and malformed (`=`-less)
  lines ignored.
- `env_file` grammar: the existing `parse_env_file` / `merge_env` tests move with
  the module and keep running on both hosts.

Backends:

- systemd renders N `EnvironmentFile=` lines in declared order, `-`-prefixed on
  optional entries only.
- launchd folds files in order with later-wins; skips an absent *optional* file;
  still errors on an absent *required* one.

Installer:

- the backup is written iff the diff is non-empty;
- `kastellan.env.local` is never written by `install`;
- `build_specs` emits the two entries in the right order with the right
  optionality.

Force-routing:

- the audit payload builder is pure and unit-tested;
- the phrase const is asserted through the const, not a literal.

## Gate

- **Mac:** `-p kastellan-supervisor` (baseline 85 / 0), `-p kastellan-core --lib
  install::`, `clippy --all-targets -D warnings` on both crates, plus the
  supervisor cross-linted for `aarch64-unknown-linux-gnu` — it is pure-Rust, so
  that type-checks the systemd arm from the Mac.
- **DGX:** full workspace against the **3047 / 0 / 53** baseline — measured at
  `200ce72e`, whose tree is what `5cbab01e` squash-merged, so it carries to
  today's `main` —
  `clippy --workspace --all-targets -D warnings`, run with `--nocapture` so the
  4 `[SKIP]` lines are observed to be the `KASTELLAN_GLINER_RELEX_ENABLE` tier.

## Live acceptance — today's failure, re-run as a test

On the DGX:

1. Move the five tuned settings into `~/.config/kastellan/kastellan.env.local`.
2. Re-run the install.
3. Confirm all five survive in `/proc/<MainPID>/environ` (the file being right is
   not the same as the process having it), that `KASTELLAN_EGRESS_FORCE_ROUTING`
   is still `1`, and that a live `ask` about email still answers correctly.

This leaves the host permanently fixed rather than repaired-until-next-deploy.

## Risks and residuals

- **`ServiceSpec` is a breaking API change.** Confirmed acceptable: the only
  consumers are this workspace and this operator's two hosts. The parked
  "workspace is still 0.2.0 while pub API accumulates" note still applies at the
  next crates.io release.
- **The overlay only protects keys the operator puts in it.** Part 2's warning is
  what covers the rest: a key added to `kastellan.env` later is now named at
  install time instead of vanishing silently.
- **macOS refresh timing** differs as documented above. Not closable without
  launchd gaining a directive it does not have.
