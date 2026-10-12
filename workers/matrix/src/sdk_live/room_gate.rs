//! The room edge: the event handlers that decide what reaches the inbound
//! buffer (text from an allowed peer, in a two-party room, once the sync is
//! live) and which invites the bot accepts. Split out of `sdk_live.rs` (#835)
//! to keep that file under the 500-LOC soft cap before the drain grew it; the
//! items moved verbatim (only their visibility changed).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent};
use matrix_sdk::{Client, Room, RoomMemberships, RoomState};

use kastellan_matrix_wire::{push_bounded_fair, Event, FairPush};

use super::INBOUND_CAP;

/// Register the room-message event handler: decode text bodies, skip our own
/// echoes + the initial-sync backlog (`live`), and push onto the bounded inbound
/// buffer.
pub(super) fn register_message_handler(
    client: &Client,
    buffer: Arc<Mutex<VecDeque<Event>>>,
    own_user_id: matrix_sdk::ruma::OwnedUserId,
    live: Arc<AtomicBool>,
    allowed_peers: Arc<Vec<String>>,
) {
    client.add_event_handler(move |ev: OriginalSyncRoomMessageEvent, room: Room| {
        let buffer = buffer.clone();
        let own = own_user_id.clone();
        let live = live.clone();
        let allowed = allowed_peers.clone();
        async move {
            // Skip the initial catch-up sync (room history + offline backlog):
            // only surface events once continuous sync is live.
            if !live.load(Ordering::SeqCst) {
                return;
            }
            // Echo-loop guard: never re-ingest messages from our own user id.
            // Note this also drops messages sent by *other* devices logged into
            // the same account — acceptable for a single-identity channel bot,
            // which is the only deployment shape today.
            if ev.sender == own {
                return;
            }
            // Room-edge gate (security audit 2026-09-02, channel F1). The
            // core authorises the *peer*; nothing scoped the *room*, so a
            // third party could invite the bot and the paired operator into
            // a room they control, have the operator's messages executed as
            // instructions and read every reply. Two rules: (1) when a peer
            // set is configured, only those senders are ever buffered;
            // (2) the room must be two-party — its joined+invited members
            // are exactly the bot and one peer — so a shared room is never a
            // command channel, whoever created it. Bodies are never logged.
            if !allowed.is_empty() && !peer_allowed(&allowed, ev.sender.as_str()) {
                eprintln!(
                    "kastellan-worker-matrix: dropping event in {} from a sender outside \
                     KASTELLAN_MATRIX_PEERS",
                    room.room_id()
                );
                return;
            }
            if !room_is_two_party(&room, &own, &allowed).await {
                eprintln!(
                    "kastellan-worker-matrix: dropping event in {}: not a two-party room \
                     (bot + one peer)",
                    room.room_id()
                );
                return;
            }
            if let MessageType::Text(text) = ev.content.msgtype {
                let event = Event {
                    conversation: room.room_id().to_string(),
                    peer: ev.sender.to_string(),
                    body: text.body,
                };
                match push_bounded_fair(
                    &mut buffer.lock().expect("buffer not poisoned"),
                    event,
                    INBOUND_CAP,
                    INBOUND_PER_PEER_CAP,
                ) {
                    FairPush::Pushed => {}
                    FairPush::PushedEvictedOldest => {
                        eprintln!("kastellan-worker-matrix: inbound buffer full; dropped oldest event");
                    }
                    FairPush::DroppedSenderFlood => {
                        eprintln!(
                            "kastellan-worker-matrix: sender {} at its per-peer buffer cap; \
                             dropped its newest event",
                            ev.sender
                        );
                    }
                }
            }
        }
    });
}

/// Per-sender share of the inbound buffer (see `push_bounded_fair`).
const INBOUND_PER_PEER_CAP: usize = 64;

/// Case-insensitive membership of a Matrix user id in the configured peer set.
fn peer_allowed(allowed: &[String], user_id: &str) -> bool {
    let u = user_id.to_ascii_lowercase();
    allowed.contains(&u)
}

/// True iff the room's joined + invited members are exactly the bot and one
/// other account (which, when a peer set is configured, must be in it).
/// Invited-but-not-joined members count: a pending third-party invite would
/// otherwise let them join later and read the exchange.
async fn room_is_two_party(
    room: &Room,
    own: &matrix_sdk::ruma::UserId,
    allowed: &[String],
) -> bool {
    let members = match room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("kastellan-worker-matrix: could not list members of {}: {e}", room.room_id());
            return false; // fail closed
        }
    };
    let others: Vec<&matrix_sdk::ruma::UserId> = members
        .iter()
        .map(|m| m.user_id())
        .filter(|u| *u != own)
        .collect();
    match others.as_slice() {
        [one] => allowed.is_empty() || peer_allowed(allowed, one.as_str()),
        _ => false,
    }
}

/// Auto-accept room invites addressed to the bot so a user can simply start a
/// (DM) conversation with it. This only *joins* the room — it grants no trust:
/// authorization stays at the core bus ([`channel::auth::DbPeerAuthorizer`]),
/// which drops messages from unpaired senders. Joining is retried a few times
/// to ride out transient server lag (federation is off here, so a couple of
/// short retries suffice).
pub(super) fn register_autojoin_handler(client: &Client, allowed_peers: Arc<Vec<String>>) {
    client.add_event_handler(move |ev: StrippedRoomMemberEvent, client: Client, room: Room| {
        let allowed = allowed_peers.clone();
        async move {
        // Only react to the invite *for us*; ignore membership churn for others.
        let Some(me) = client.user_id() else { return };
        if ev.state_key != me {
            return;
        }
        if room.state() != RoomState::Invited {
            return;
        }
        let room_id = room.room_id().to_string();
        // Decline invites from outside the configured peer set (security
        // audit 2026-09-02, channel F1): joining is not trust, but every
        // joined room is synced, stored and a flood surface, and the old
        // handler joined *any* room from *anyone*. `ev.sender` is the
        // inviter. Leaving rejects the invite so it does not linger.
        if !allowed.is_empty() && !peer_allowed(&allowed, ev.sender.as_str()) {
            eprintln!(
                "kastellan-worker-matrix: declining invite to {room_id} from a sender outside \
                 KASTELLAN_MATRIX_PEERS"
            );
            if let Err(e) = room.leave().await {
                eprintln!("kastellan-worker-matrix: could not decline invite to {room_id}: {e}");
            }
            return;
        }
        for attempt in 0..3u32 {
            match room.join().await {
                Ok(()) => {
                    eprintln!("kastellan-worker-matrix: auto-joined invited room {room_id}");
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "kastellan-worker-matrix: join {room_id} failed (attempt {}): {e}",
                        attempt + 1
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1)))
                        .await;
                }
            }
        }
        eprintln!("kastellan-worker-matrix: gave up auto-joining {room_id}");
        }
    });
}
