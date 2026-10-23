//! [`ServiceStatus`]: the backend-neutral reading of a service's state.

use serde::{Deserialize, Serialize};

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

/// One-line text for a status plus the service manager's own state word, for
/// timeout messages: `Inactive (state word: languishing)`.
///
/// The word is what `status` folds away — `languishing` and `exited` both
/// read `Inactive` — and it is often the only clue to why a health wait gave
/// up (#900). Without a word this is just the status's name.
pub fn describe_status(status: ServiceStatus, word: Option<&str>) -> String {
    match word {
        Some(w) => format!("{status:?} (state word: {w})"),
        None => format!("{status:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_status_names_the_word_status_folds_away() {
        assert_eq!(
            describe_status(ServiceStatus::Inactive, Some("languishing")),
            "Inactive (state word: languishing)"
        );
        assert_eq!(describe_status(ServiceStatus::NotInstalled, None), "NotInstalled");
    }
}
