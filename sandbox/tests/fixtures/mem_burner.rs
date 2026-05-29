//! Tiny memory-allocation probe used by the cgroup OOM-kill smoke test
//! on Linux. Built as a workspace bin; the test invokes
//! `target/debug/mem_burner --mb N` under a sandbox policy with a small
//! `MemoryMax` and asserts the kernel killed it.
//!
//! What it does:
//!   1. Parse `--mb N` from argv (no clap, no deps — keep this minimal so
//!      it links against nothing the cgroup test can blame for an
//!      allocation failure).
//!   2. Allocate a `Vec<u8>` of N MiB and **touch every page** (write 1
//!      byte every 4 KiB). Without the touch, the pages stay
//!      copy-on-write zero pages and never count against `memory.max`.
//!   3. Sleep briefly, exit 0.
//!
//! The test runs this with N significantly larger than the `MemoryMax`
//! it set on the cgroup; the kernel should OOM-kill the process during
//! step 2.

use std::time::Duration;

const PAGE_SIZE: usize = 4096;
const MB: usize = 1024 * 1024;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut mb: Option<usize> = None;
    while let Some(a) = args.next() {
        if a == "--mb" {
            mb = args
                .next()
                .and_then(|v| v.parse::<usize>().ok());
        }
    }
    let mb = mb.unwrap_or_else(|| {
        eprintln!("mem_burner: missing --mb N");
        std::process::exit(2);
    });

    let bytes = mb * MB;
    // Allocate N MiB. `vec![0u8; bytes]` may hand back demand-zero pages
    // that don't count against `memory.max` until they're written, so the
    // loop below still touches one byte per page to force every page
    // resident. We deliberately avoid `Vec::set_len` over uninitialised
    // capacity here — `clippy::uninit_vec` is deny-by-default and would
    // break `cargo clippy --all-targets` (issue #150).
    let mut buf: Vec<u8> = vec![0u8; bytes];

    let mut i = 0usize;
    while i < bytes {
        // Tag with the page index, modulo 256 — keeps the optimiser
        // from eliding the write.
        buf[i] = (i / PAGE_SIZE) as u8;
        i += PAGE_SIZE;
    }

    // If we got here without being killed, the cgroup didn't enforce.
    // Sleep briefly so the parent sees a clean exit (which the test
    // interprets as a containment failure).
    std::thread::sleep(Duration::from_millis(100));
    std::process::exit(0);
}
