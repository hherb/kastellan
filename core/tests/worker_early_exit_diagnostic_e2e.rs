//! #666, core half: a worker that dies before responding must take its own
//! explanation with it into the daemon log.
//!
//! `Protocol(EarlyExit)` — "worker exited before responding" — is the most
//! content-free failure this system produces. It says the pipe closed and
//! nothing else. Three separate micro-VM production defects all surfaced as
//! exactly that string, and telling them apart took a session. The worker had
//! said why each time, on the stream nobody was reading.
//!
//! The pure halves of the fix are unit-tested next to their code
//! (`worker_stderr::format_early_exit_report`, the drain-completion flag, the
//! control-character neutralisation). What only an end-to-end run can show is
//! the **wiring**: that the tail-retaining drainer is actually attached at
//! spawn, that the dispatch path notices `EarlyExit`, and that the worker's
//! words reach `tracing` at a level an operator sees by default. Each of those
//! is one line, and each was silently absent before this test.
//!
//! Deliberately NOT a micro-VM test: the property belongs to every sandboxed
//! worker, and asserting it through a real VM would make a 30-second boot the
//! price of a claim about four lines of core.

use std::io::Write;
use std::sync::{Arc, Mutex};

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_sandbox::{Net, SandboxPolicy};
use kastellan_tests_common::{backend, skip_if_sandbox_unavailable, NoopAuditSink};

/// A `MakeWriter` that appends every log record into a shared buffer, so the
/// test can read back what `tracing` was actually given.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer not poisoned").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The shell the fixture worker is: a process that says something on stderr and
/// exits without ever speaking JSON-RPC.
///
/// `/bin/sh` is present in both jails (bwrap binds `/usr` and symlinks
/// `/bin`; Seatbelt allows the system prefix read-only), which is why the
/// fixture is a shell one-liner rather than a compiled binary — a fixture
/// binary would need building, and the property under test has nothing to do
/// with what the worker is.
const FIXTURE_SHELL: &str = "/bin/sh";

/// A phrase distinctive enough that finding it in the log cannot be a
/// coincidence, and shaped like the real thing this test stands in for
/// (`microvm-init: chown of the relay socket … Halting instead.`).
const LAST_WORDS: &str = "kastellan-test: refusing to serve, relay socket unreachable";

#[test]
fn a_worker_that_dies_before_responding_gets_its_last_words_into_the_log() {
    if skip_if_sandbox_unavailable() {
        return;
    }

    let captured = CapturedLog::default();
    // Process-wide, not thread-local: `dispatch` runs the synchronous
    // `worker.call` inside `tokio::task::block_in_place`, i.e. on a different
    // thread from this one, so a scoped subscriber would capture nothing and
    // the test would fail for the wrong reason.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let policy = SandboxPolicy {
        fs_read: vec![std::path::PathBuf::from(FIXTURE_SHELL)],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        ..SandboxPolicy::default()
    };
    let script = format!("echo '{LAST_WORDS}' >&2; exit 3");
    let args = ["-c", script.as_str()];
    let spec = WorkerSpec {
        policy: &policy,
        program: FIXTURE_SHELL,
        args: &args,
        wall_clock_ms: Some(10_000),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    let result = rt.block_on(async {
        let mut worker = spawn_worker(&*backend(), &spec).expect("spawn the fixture worker");
        let out = dispatch_with_sink(
            &NoopAuditSink,
            &Vault::new(),
            None,
            &mut worker,
            "early-exit-fixture",
            "anything",
            serde_json::json!({}),
        )
        .await;
        let _ = worker.close();
        out
    });

    assert!(
        result.is_err(),
        "the fixture never answers, so the dispatch must fail: {result:?}"
    );

    let log = String::from_utf8_lossy(&captured.0.lock().expect("log buffer").clone()).into_owned();
    assert!(
        log.contains(LAST_WORDS),
        "the worker's own explanation must reach the log at WARN. Without it the \
         operator sees only `worker exited before responding`, which is what made \
         three separate micro-VM defects indistinguishable (#666).\nCaptured log:\n{log}"
    );
    assert!(
        log.contains(FIXTURE_SHELL),
        "the report must name WHICH worker died — on the micro-VM path the process \
         that exits is the launcher, not the tool.\nCaptured log:\n{log}"
    );
}
