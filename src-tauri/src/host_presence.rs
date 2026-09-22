//! What this computer last learned about each paired host: whether it answers,
//! and which of the shared displays it can see on its own side.
//!
//! Presence describes this moment on this network, so it is held in memory
//! only and starts out unknown. A value read back from disk would claim a host
//! is up before anything had asked it — exactly the wrong thing to show next
//! to a button that sends a display somewhere.

use std::collections::HashMap;

use serde::Serialize;

/// The last thing a check learned about one paired host, as the windows show
/// it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPresence {
    pub peer_id: String,
    /// Whether the last check got a signed reply from that host.
    pub online: bool,
    /// When the last check finished, in Unix milliseconds. Zero until one has.
    pub checked_at_ms: u64,
    /// When that host last answered, in Unix milliseconds. Zero until it has.
    pub last_seen_at_ms: u64,
    /// `monitor_key` of every shared display that host reported seeing.
    ///
    /// `None` means it did not say: it has not answered, or it runs an agent
    /// that predates the field. That has to read as unknown — a host answering
    /// `Some([])` is the case worth showing, since it is up and the display is
    /// not on it.
    pub attached_monitors: Option<Vec<String>>,
    /// Why the last check failed, ready to show. Empty while online.
    pub detail: String,
}

/// How a check ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// The host answered. `attached` is what it said about the shared
    /// displays, or `None` when it said nothing about them.
    Answered { attached: Option<Vec<String>> },
    /// Nothing valid came back, with the reason to show.
    Silent { detail: String },
}

impl HostPresence {
    /// A host nothing has asked yet.
    pub fn unknown(peer_id: &str) -> Self {
        Self {
            peer_id: peer_id.to_owned(),
            ..Self::default()
        }
    }
}

/// `previous` brought up to date with how a check just ended.
///
/// A host that stopped answering keeps `last_seen_at_ms` — the host list says
/// when it was last up — but loses what it saw: a display list nobody can
/// confirm any more is a guess, and this reports only what a host has said.
pub fn recorded(
    previous: Option<&HostPresence>,
    peer_id: &str,
    check: Check,
    now_ms: u64,
) -> HostPresence {
    let last_seen_at_ms = previous.map_or(0, |presence| presence.last_seen_at_ms);
    match check {
        Check::Answered { attached } => HostPresence {
            peer_id: peer_id.to_owned(),
            online: true,
            checked_at_ms: now_ms,
            last_seen_at_ms: now_ms,
            attached_monitors: attached,
            detail: String::new(),
        },
        Check::Silent { detail } => HostPresence {
            peer_id: peer_id.to_owned(),
            online: false,
            checked_at_ms: now_ms,
            last_seen_at_ms,
            attached_monitors: None,
            detail,
        },
    }
}

/// Every paired host in `peer_ids`, in that order, so the windows can show a
/// host that has never been checked as unknown rather than leaving a gap.
pub fn listed(known: &HashMap<String, HostPresence>, peer_ids: &[String]) -> Vec<HostPresence> {
    peer_ids
        .iter()
        .map(|peer_id| {
            known
                .get(peer_id)
                .cloned()
                .unwrap_or_else(|| HostPresence::unknown(peer_id))
        })
        .collect()
}

/// Drops what is known about hosts that are no longer paired, so a host added
/// again under the same id starts from unknown rather than from whatever it
/// was doing when it was removed.
pub fn forget_unpaired(known: &mut HashMap<String, HostPresence>, peer_ids: &[String]) {
    known.retain(|peer_id, _| peer_ids.iter().any(|paired| paired == peer_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: &str = "peer-1";

    fn answered(attached: Option<&[&str]>) -> Check {
        Check::Answered {
            attached: attached.map(|keys| keys.iter().map(|key| (*key).to_owned()).collect()),
        }
    }

    #[test]
    fn a_host_that_answers_is_online_and_says_what_it_sees() {
        let presence = recorded(None, PEER, answered(Some(&["DEL:1234:SN"])), 1_000);

        assert!(presence.online);
        assert_eq!(presence.checked_at_ms, 1_000);
        assert_eq!(presence.last_seen_at_ms, 1_000);
        assert_eq!(
            presence.attached_monitors.as_deref(),
            Some(["DEL:1234:SN".to_owned()].as_slice())
        );
    }

    /// The whole point of the display column: the host is up, and the cable is
    /// not there. It must not be confused with a host that never said.
    #[test]
    fn a_host_that_sees_no_shared_display_is_not_the_same_as_one_that_did_not_say() {
        let silent_about_displays = recorded(None, PEER, answered(None), 1_000);
        let sees_none = recorded(None, PEER, answered(Some(&[])), 1_000);

        assert_eq!(silent_about_displays.attached_monitors, None);
        assert_eq!(sees_none.attached_monitors, Some(Vec::new()));
    }

    #[test]
    fn a_host_that_stops_answering_keeps_when_it_was_last_up_and_drops_what_it_saw() {
        let online = recorded(None, PEER, answered(Some(&["DEL:1234:SN"])), 1_000);

        let offline = recorded(
            Some(&online),
            PEER,
            Check::Silent {
                detail: "連線逾時".to_owned(),
            },
            5_000,
        );

        assert!(!offline.online);
        assert_eq!(offline.checked_at_ms, 5_000);
        assert_eq!(offline.last_seen_at_ms, 1_000);
        assert_eq!(offline.attached_monitors, None);
        assert_eq!(offline.detail, "連線逾時");
    }

    #[test]
    fn every_paired_host_is_listed_even_before_it_is_checked() {
        let mut known = HashMap::new();
        known.insert(
            PEER.to_owned(),
            recorded(None, PEER, answered(Some(&[])), 1_000),
        );

        let listed = listed(&known, &[PEER.to_owned(), "peer-2".to_owned()]);

        assert_eq!(listed.len(), 2);
        assert!(listed[0].online);
        assert_eq!(listed[1], HostPresence::unknown("peer-2"));
    }

    #[test]
    fn a_removed_host_is_forgotten() {
        let mut known = HashMap::new();
        known.insert(
            PEER.to_owned(),
            recorded(None, PEER, answered(Some(&[])), 1_000),
        );

        forget_unpaired(&mut known, &["peer-2".to_owned()]);

        assert!(known.is_empty());
    }
}
