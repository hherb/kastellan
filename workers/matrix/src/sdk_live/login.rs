//! Session bring-up for the live worker: restore the persisted login session
//! (or password-login and persist it, `0600`, fsync'd), and bootstrap the
//! account's cross-signing identity. Split out of the former `sdk_live.rs`
//! (now `sdk_live/mod.rs`) for the 500-LOC soft cap; the items moved verbatim
//! (only their visibility changed).

use anyhow::Context as _;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::Client;

use super::{LiveSdkConfig, SESSION_FILE};

/// Restore a persisted session if one exists, else password-login and persist
/// the resulting session so the device identity is stable across restarts.
pub(super) async fn restore_or_login(client: &Client, config: &LiveSdkConfig) -> anyhow::Result<()> {
    let session_path = config.store_dir.join(SESSION_FILE);
    if let Ok(bytes) = std::fs::read(&session_path) {
        let session: MatrixSession =
            serde_json::from_slice(&bytes).context("decode persisted session")?;
        // A matrix-sdk major upgrade (e.g. 0.8→0.18) can invalidate the on-disk
        // SQLite crypto store; restore then fails here. `install` does NOT wipe
        // the store, so name the remedy in the error itself rather than leaving
        // the operator to rediscover it — see the deploy gotcha in HANDOVER.md.
        client.restore_session(session).await.with_context(|| {
            format!(
                "restore session from {session_path:?} — if this follows a \
                 matrix-sdk upgrade the on-disk crypto store is likely \
                 incompatible; remove {:?} and restart with \
                 KASTELLAN_MATRIX_PASSWORD set to re-bootstrap a fresh device",
                config.store_dir
            )
        })?;
        return Ok(());
    }

    // No persisted session → initial password login. The password is only
    // required on this path; restarts restore the session above without it.
    let password = config.password.as_deref().context(
        "KASTELLAN_MATRIX_PASSWORD must be set for the initial login (no persisted session yet)",
    )?;
    client
        .matrix_auth()
        .login_username(&config.user, password)
        .initial_device_display_name(&config.device_name)
        .send()
        .await
        .context("password login")?;

    if let Some(session) = client.matrix_auth().session() {
        let bytes = serde_json::to_vec(&session).context("encode session")?;
        // The session holds the access token + device keys: write it 0600 so the
        // worker's credentials are never world/group-readable at rest.
        write_private(&session_path, &bytes)
            .with_context(|| format!("persist session to {session_path:?}"))?;
    }
    Ok(())
}

/// Ensure the account has a cross-signing identity, bootstrapping one if absent
/// so the bot self-signs its own device. `bootstrap_cross_signing_if_needed` is
/// idempotent (a no-op once an identity exists, e.g. on every session restore);
/// the initial bootstrap uploads keys behind UIA, so it needs the password —
/// available only on the initial password-login path. Without a password and
/// without an existing identity it logs and returns `Ok` (the worker still runs;
/// the shield just persists until an initial login bootstraps it).
pub(super) async fn ensure_cross_signing(client: &Client, config: &LiveSdkConfig) -> anyhow::Result<()> {
    match client.encryption().bootstrap_cross_signing_if_needed(None).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // The keys upload demands user-interactive auth; retry with the password.
            let Some(uiaa_info) = e.as_uiaa_response() else {
                return Err(anyhow::anyhow!("bootstrap cross-signing: {e}"));
            };
            let Some(password) = config.password.as_deref() else {
                eprintln!(
                    "kastellan-worker-matrix: cross-signing needs setup but no password is \
                     available (session restore); skipping — re-run the initial login with a password"
                );
                return Ok(());
            };
            let user_id = client.user_id().context("no user id for cross-signing UIA")?;
            let mut pw = uiaa::Password::new(
                uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(
                    user_id.localpart().to_owned(),
                )),
                password.to_owned(),
            );
            pw.session = uiaa_info.session.clone();
            client
                .encryption()
                .bootstrap_cross_signing(Some(uiaa::AuthData::Password(pw)))
                .await
                .context("bootstrap cross-signing with password UIA")?;
            eprintln!("kastellan-worker-matrix: cross-signing bootstrapped");
            Ok(())
        }
    }
}

/// Write `bytes` to `path`, truncating, with `0600` permissions (owner-only).
/// Used for the persisted login session, which is a secret at rest.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    // Durability across a HARD kill. Under the slice 5b-4b VM backend the store is
    // an ext4 `persistent_store` image: a plain write only reaches the guest page
    // cache, so a SIGKILL of firecracker (a VM crash / the PersistentWorker respawn
    // trigger) discards it — leaving a 0-byte `session.json` that a respawned worker
    // cannot restore (it hangs in `restore_session`, breaking the #321 downtime
    // recovery in a VM). SQLite fsyncs its own WAL, so only this plain-file write was
    // at risk. fsync the file, then fsync the parent dir so the directory entry is
    // durable too. Cheap + correct on every backend (bwrap/Seatbelt included).
    f.sync_all()?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}
