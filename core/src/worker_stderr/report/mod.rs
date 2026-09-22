//! What the system *says* about a worker that died: the report formatters, the
//! stderr-fallback markers, and the emitters that choose a channel for them.
//!
//! Split out of [`super`] (the capture half) because the two answer different
//! questions. That module keeps a dying worker's bytes; this one turns them
//! into a line a human reads. Every item here is re-exported by the parent, so
//! `worker_stderr::` is still the one public path and no call site names
//! `report` directly.
//!
//! # Two channels, one producer per event, exactly one channel per report
//!
//! A worker's last words have to reach whoever is looking, and *who* that is
//! differs by binary. The daemon installs a `tracing` subscriber as the first
//! statement of `main`; **test binaries and `kastellan-cli` install none**, so
//! before #725 a report logged through `tracing` alone went nowhere in exactly
//! the place a human was reading it. Each emitter here therefore logs through
//! `tracing` **or** `eprintln!`s a marked line — whichever will actually be
//! read, decided per event by [`delivery::warn_and_fall_back`].
//!
//! ⚠️ **"Has a subscriber" is not the question, and the daemon is not exempt**
//! (#734). `main` builds its subscriber from `EnvFilter::try_from_default_env()`,
//! so a target-scoped `RUST_LOG` in the operator overlay leaves a subscriber
//! installed that records nothing from these targets — and the report then
//! needs the stderr line exactly as a test binary does.
//!
//! There are three such events, and they get **distinct markers** so a grep
//! can tell them apart:
//!
//! | Event | Marker | Emitter |
//! | --- | --- | --- |
//! | a tool worker fails a call and is retired | [`WORKER_FAILED_STDERR_MARKER`] | [`emit_worker_failure_report`] |
//! | a persistent worker dies mid-service | [`WORKER_DEATH_STDERR_MARKER`] | [`emit_persistent_death_report`] |
//! | a persistent worker is NOT coming back | [`WORKER_DOWN_STDERR_MARKER`] | [`emit_persistent_down_report`] |
//!
//! ⚠️ **The shared half is shared on purpose.** All three emitters render
//! through the same private `format_stderr_fallback` and emit through the same
//! `warn_and_fall_back!`, so neither the neutralisation nor the delivery check
//! exists in two copies that can drift. That drift is the shape CLAUDE.md's
//! bwrap-argv note names, where the incomplete copy is the one that breaks —
//! and #730 is the worked example: the persistent-worker report was a
//! hand-rolled second copy of the `tracing`-only half and inherited none of
//! #725's fixes.
//!
//! ⚠️ **The emit half is a macro, and that is not a style choice.** See
//! [`delivery::warn_and_fall_back`]: the delivery check carries the
//! `module_path!` of wherever it is *written*, so the same check in a shared
//! function answers for the wrong target and reports "delivered" for an event
//! `EnvFilter` dropped. Measured table in that macro's doc.

mod delivery;
mod persistent;
pub(crate) mod shared;
mod tool_worker;

pub use persistent::{
    emit_persistent_death_report, emit_persistent_down_report, format_death_report,
    format_persistent_death_line, format_persistent_death_stderr_fallback,
    format_persistent_down_line, format_persistent_down_stderr_fallback,
    WORKER_DEATH_STDERR_MARKER, WORKER_DOWN_STDERR_MARKER,
};
pub use shared::STDERR_FALLBACK_MARKERS;
pub use tool_worker::{
    emit_worker_failure_report, format_worker_failure_report,
    format_worker_failure_stderr_fallback, WORKER_FAILED_STDERR_MARKER,
};
