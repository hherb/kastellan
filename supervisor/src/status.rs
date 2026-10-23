//! [`ServiceStatus`]: the backend-neutral reading of a service's state.

use serde::{Deserialize, Serialize};

use crate::{Supervisor, SupervisorError};

/// Coarse runtime state of a service, normalized across backends.
///
/// A caller polls for the state it wants: `Active` after a start, `Inactive`
/// after a stop. [`Transitioning`](Self::Transitioning) exists so that
/// neither wait can be fooled by a service caught between the two (#902).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ServiceStatus {
    /// The service is up and serving: systemd's `active`/`reloading`/
    /// `refreshing`, launchd's `running`.
    Active,
    /// The service exists and is settled not running: systemd's `inactive`/
    /// `maintenance`, launchd's `not running`/`exited`/`languishing`, or an
    /// agent whose plist is on disk but is not loaded.
    Inactive,
    /// The service is on its way up or down and may still have a process —
    /// systemd's `activating`/`deactivating`, launchd's `spawning` or
    /// `SIGTERMed`. Neither "running" nor "stopped": a wait for `Inactive`
    /// after a stop must not end here while the process is in its stop
    /// timeout (#902).
    ///
    /// Also any state word a backend does not recognise: no list of the
    /// manager's words is provably complete (#898's `xpcproxy`). An unknown
    /// word must not fail a health wait outright, nor end a stop-wait early;
    /// read as moving, it keeps either wait polling, and a timeout names it.
    Transitioning,
    /// The service exists and entered the failed state (systemd's `failed`;
    /// launchd has no such word and never reports it).
    Failed,
    /// The service is not known to the supervisor (no installed unit / agent).
    NotInstalled,
}

/// One-line text for a status plus the service manager's own state word and,
/// when a wait has given up, its crash-loop evidence, for timeout messages:
/// `Transitioning (state word: activating; Result=exit-code, NRestarts=31)`.
///
/// The word is what `status` folds away — `languishing` and `exited` both
/// read `Inactive` — and it is often the only clue to why a health wait gave
/// up (#900). The evidence says how often the service has already died: a
/// crash loop and a slow first start share every word (#904). Without
/// either this is just the status's name.
pub fn describe_status(status: ServiceStatus, word: Option<&str>, evidence: Option<&str>) -> String {
    let word = word.map(|w| format!("state word: {w}"));
    let details: Vec<String> = word.into_iter().chain(evidence.map(str::to_string)).collect();
    if details.is_empty() {
        format!("{status:?}")
    } else {
        format!("{status:?} ({})", details.join("; "))
    }
}

/// [`describe_status`] for a wait that has given up: asks the manager, once,
/// for [`Supervisor::crash_evidence`] too (#904). Call it on the timeout path
/// only — it is one more call to the manager, and the poll loop stays one
/// call per tick. A service that is not installed has no evidence to read.
pub fn describe_last_status<S: Supervisor + ?Sized>(
    sup: &S,
    name: &str,
    status: ServiceStatus,
    word: Option<&str>,
) -> String {
    let evidence = match status {
        ServiceStatus::NotInstalled => None,
        _ => evidence_text(sup.crash_evidence(name)),
    };
    describe_status(status, word, evidence.as_deref())
}

/// The evidence as timeout text. A failed read is named, never dropped: a
/// missing clause would read as "the manager had nothing to say".
fn evidence_text(read: Result<Option<String>, SupervisorError>) -> Option<String> {
    match read {
        Ok(evidence) => evidence,
        Err(e) => Some(format!("crash evidence unreadable: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_status_names_the_word_status_folds_away() {
        assert_eq!(
            describe_status(ServiceStatus::Inactive, Some("languishing"), None),
            "Inactive (state word: languishing)"
        );
        assert_eq!(describe_status(ServiceStatus::NotInstalled, None, None), "NotInstalled");
    }

    /// #904: the evidence follows the word, so a crash loop no longer reads
    /// like a slow first start.
    #[test]
    fn describe_status_appends_the_crash_loop_evidence() {
        assert_eq!(
            describe_status(
                ServiceStatus::Transitioning,
                Some("activating"),
                Some("Result=exit-code, NRestarts=31")
            ),
            "Transitioning (state word: activating; Result=exit-code, NRestarts=31)"
        );
        assert_eq!(describe_status(ServiceStatus::Failed, None, Some("runs = 2")), "Failed (runs = 2)");
    }

    #[test]
    fn evidence_text_names_a_failed_read() {
        assert_eq!(evidence_text(Ok(None)), None);
        assert_eq!(evidence_text(Ok(Some("runs = 3".into()))).as_deref(), Some("runs = 3"));
        let failed = evidence_text(Err(SupervisorError::Backend("launchctl print timed out".into())));
        assert_eq!(
            failed.as_deref(),
            Some("crash evidence unreadable: supervisor backend command failed: launchctl print timed out")
        );
    }
}
