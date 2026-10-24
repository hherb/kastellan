//! Waiting for the guard backend's `/props` at boot (issue [#609]).
//!
//! # Why boot waits at all
//!
//! An unreachable `/props` is fatal (wiring-spec D6): starting without the
//! tier would turn a security control off behind a correct-looking log line.
//! That refusal used to be **immediate**, which made a guard backend starting
//! a few seconds after the daemon indistinguishable from one that is not
//! there — and both crash-looped the whole agent onto the supervisor's
//! restart ramp, which tops out at 300 s. Since #609 the guard backend is a
//! supervised unit that core is `After=`, but `After=` orders the *start*,
//! not readiness, and launchd has no ordering at all.
//!
//! # What is waited for, and what is not
//!
//! Measured on the DGX against `llama-server` loading Shieldstral 3B Q8
//! (2026-10-24): ~0.3 s of **connection refused** while the process starts,
//! then ~5 s of **HTTP 503 `Loading model`**, then 200. Those two answers —
//! [`GuardErrorKind::Connect`] / [`GuardErrorKind::ConnectTimeout`] and a
//! 503 — are the "not up *yet*" ones and are retried for up to
//! [`PROPS_WAIT_BUDGET_MS`].
//!
//! Everything else refuses at once, as before: a 404 or a body that is not
//! JSON means the URL points at something that is not a llama.cpp server, and
//! waiting will not change that. A request **timeout** is not retried
//! either: the server accepted the connection and then said nothing, which is
//! a hung backend, not a starting one.
//!
//! D6 is unchanged: when the budget runs out, the same fatal
//! `PropsUnavailable` follows. The wait only stops a start race from looking
//! like an outage.
//!
//! [#609]: https://github.com/hherb/kastellan/issues/609

use std::future::Future;
use std::time::{Duration, Instant};

use kastellan_llm_router::RouterError;

use super::error_kind::{self, GuardErrorKind};

/// How long boot keeps asking a not-yet-ready backend before refusing.
///
/// 60 s: twelve times the measured 5 s model load, so a slower host or a
/// bigger guard model still fits, and short enough that a backend that will
/// never come up costs one minute per restart rather than hiding the refusal.
pub const PROPS_WAIT_BUDGET_MS: u64 = 60_000;

/// Pause between attempts. Shorter would only add log lines; the load takes
/// seconds, not milliseconds.
pub const PROPS_RETRY_INTERVAL_MS: u64 = 2_000;

/// Is this `/props` failure a backend that is not up **yet**? Pure.
///
/// Takes the classification and the HTTP status rather than the error,
/// because a `reqwest::Error` cannot be built by hand and the connect arms
/// would then be reachable by no unit test (the same split as
/// [`error_kind::classify_transport`]).
pub fn not_up_yet(kind: GuardErrorKind, http_status: Option<u16>) -> bool {
    match kind {
        GuardErrorKind::Connect | GuardErrorKind::ConnectTimeout => true,
        GuardErrorKind::HttpStatus => http_status == Some(503),
        GuardErrorKind::Timeout
        | GuardErrorKind::Transport
        | GuardErrorKind::Decode
        | GuardErrorKind::Config
        | GuardErrorKind::Other => false,
    }
}

/// Retry `/props` while the backend is [`not_up_yet`] and the next attempt
/// still starts inside `budget`; return the first answer, or the last error.
///
/// Generic over the call so the loop is testable without a server: the IO
/// is the caller's `props` closure (`GuardClient::props` in production).
pub async fn wait_for_props<F, Fut>(
    mut props: F,
    budget: Duration,
    interval: Duration,
) -> Result<serde_json::Value, RouterError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<serde_json::Value, RouterError>>,
{
    let started = Instant::now();
    let mut attempt = 1u32;
    loop {
        let err = match props().await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let status = match &err {
            RouterError::HttpStatus { status, .. } => Some(*status),
            _ => None,
        };
        if !not_up_yet(error_kind::classify(&err), status)
            || started.elapsed() + interval >= budget
        {
            return Err(err);
        }
        tracing::warn!(
            attempt,
            error = %err,
            retry_in_ms = interval.as_millis() as u64,
            budget_ms = budget.as_millis() as u64,
            "guard backend /props not ready yet; waiting before refusing to boot"
        );
        tokio::time::sleep(interval).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn only_a_starting_backend_is_waited_for() {
        assert!(not_up_yet(GuardErrorKind::Connect, None));
        assert!(not_up_yet(GuardErrorKind::ConnectTimeout, None));
        assert!(not_up_yet(GuardErrorKind::HttpStatus, Some(503)), "llama-server's `Loading model`");
        for status in [400, 404, 500, 502] {
            assert!(!not_up_yet(GuardErrorKind::HttpStatus, Some(status)), "{status}");
        }
        for kind in [
            GuardErrorKind::Timeout,
            GuardErrorKind::Transport,
            GuardErrorKind::Decode,
            GuardErrorKind::Config,
            GuardErrorKind::Other,
        ] {
            assert!(!not_up_yet(kind, None), "{kind:?}");
        }
    }

    fn http(status: u16) -> RouterError {
        RouterError::HttpStatus { status, body: "x".into() }
    }

    const FAST: Duration = Duration::from_millis(5);

    #[tokio::test]
    async fn a_503_then_200_is_waited_out() {
        let calls = Cell::new(0);
        let got = wait_for_props(
            || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move { if n < 3 { Err(http(503)) } else { Ok(serde_json::json!({"ok": n})) } }
            },
            Duration::from_secs(5),
            FAST,
        )
        .await
        .expect("the third answer is a 200");
        assert_eq!(got, serde_json::json!({"ok": 3}));
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn a_wrong_backend_refuses_on_the_first_answer() {
        let calls = Cell::new(0);
        let err = wait_for_props(
            || {
                calls.set(calls.get() + 1);
                async { Err(http(404)) }
            },
            Duration::from_secs(5),
            FAST,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RouterError::HttpStatus { status: 404, .. }));
        assert_eq!(calls.get(), 1, "a 404 will not become a llama.cpp server by waiting");
    }

    #[tokio::test]
    async fn a_backend_that_never_comes_up_is_refused_once_the_budget_is_spent() {
        let calls = Cell::new(0);
        let started = Instant::now();
        let err = wait_for_props(
            || {
                calls.set(calls.get() + 1);
                async { Err(http(503)) }
            },
            Duration::from_millis(200),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RouterError::HttpStatus { status: 503, .. }), "the last error");
        assert!(calls.get() >= 2, "it retried: {}", calls.get());
        assert!(started.elapsed() < Duration::from_secs(2), "and stopped near the budget");
    }

    #[tokio::test]
    async fn a_budget_shorter_than_one_interval_means_one_attempt() {
        let calls = Cell::new(0);
        let _ = wait_for_props(
            || {
                calls.set(calls.get() + 1);
                async { Err(http(503)) }
            },
            Duration::from_millis(10),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(calls.get(), 1);
    }

    /// The real connect refusal, through the real client: a port with nothing
    /// on it yet, and a server that binds it a moment later. The hand-built
    /// errors above cannot stand in for this one — `reqwest::Error` has no
    /// constructor, so only a real socket proves a refused connect reads as
    /// [`not_up_yet`].
    #[tokio::test]
    async fn a_refused_connect_is_waited_out_until_the_server_binds() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        }; // dropped: connects are refused until the task below binds it
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"default_generation_settings":{"n_ctx":131072}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let cfg = kastellan_llm_router::RouterConfig {
            guard_url: Some(format!("http://127.0.0.1:{port}/v1")),
            guard_model: Some("shieldstral-under-test".to_string()),
            ..kastellan_llm_router::RouterConfig::default()
        };
        let client = super::super::super::GuardClient::from_config(&cfg, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let calls = Cell::new(0);
        let props = wait_for_props(
            || {
                calls.set(calls.get() + 1);
                client.props()
            },
            Duration::from_secs(10),
            Duration::from_millis(50),
        )
        .await
        .expect("the server came up inside the budget");
        assert_eq!(props["default_generation_settings"]["n_ctx"], 131072);
        assert!(calls.get() > 1, "the first connect was refused and retried: {}", calls.get());
        server.await.unwrap();
    }

    /// The wait is wired into the boot sequence itself, not just callable:
    /// `GuardTier::from_router_config` against a backend that answers 503
    /// (`Loading model`) once and then a valid `/props` builds the tier. A
    /// boot that called `/props` once would refuse here.
    #[tokio::test]
    async fn the_boot_sequence_waits_out_a_loading_backend() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for attempt in 0.. {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let (status, body) = if attempt == 0 {
                    ("503 Service Unavailable", r#"{"error":{"message":"Loading model"}}"#)
                } else {
                    ("200 OK", r#"{"default_generation_settings":{"n_ctx":131072}}"#)
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let cfg = kastellan_llm_router::RouterConfig {
            guard_url: Some(format!("http://127.0.0.1:{port}/v1")),
            guard_model: Some("shieldstral-under-test".to_string()),
            guard_tau: Some(0.8),
            // Pinned, so the boot probe (which would need chat answers) is skipped.
            guard_timeout_ms: Some(1_000),
            ..kastellan_llm_router::RouterConfig::default()
        };
        let tier = super::super::GuardTier::from_router_config(&cfg, "props-wait-test")
            .await
            .expect("a backend that finishes loading inside the budget boots")
            .expect("the tier is configured");
        assert_eq!(tier.n_ctx(), 131_072);
        server.abort();
    }
}
