//! Custom host icons and colours shared between paired hosts.
//!
//! Keyed by discovery id like `host_alias`, and merged the same way: each
//! entry carries its own timestamp, and the newer entry for a host wins.
//! Icons and colours are names from fixed sets the frontend knows how to draw,
//! never free text, so nothing a paired host sends can reach the page as markup.

use muxsu_core::HostAppearance;

use crate::host_order::is_valid_host_id;

/// Icons a host can wear. The frontend maps each to a drawing.
pub const ICONS: [&str; 12] = [
    "desktop", "laptop", "tower", "server", "gamepad", "tablet", "tv", "work", "home", "code",
    "media", "school",
];
/// Colours a host can wear. The frontend maps each to a light and a dark shade.
pub const COLORS: [&str; 12] = [
    "indigo", "teal", "pink", "blue", "orange", "green", "purple", "red", "yellow", "graphite",
    "black", "silver",
];
/// Most entries sent in or accepted from one notice, like `host_alias`.
pub const MAX_SHARED_APPEARANCES: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum AppearanceError {
    UnknownIcon,
    UnknownColor,
}

/// Checks an icon and colour chosen for a host. An empty value means the
/// host's default.
pub fn validate(icon: &str, color: &str) -> Result<(), AppearanceError> {
    if !icon.is_empty() && !ICONS.contains(&icon) {
        return Err(AppearanceError::UnknownIcon);
    }
    if !color.is_empty() && !COLORS.contains(&color) {
        return Err(AppearanceError::UnknownColor);
    }
    Ok(())
}

/// The custom icon and colour for `host_id`, each `None` when left at default.
pub fn appearance_for<'a>(
    appearances: &'a [HostAppearance],
    host_id: &str,
) -> (Option<&'a str>, Option<&'a str>) {
    let entry = appearances.iter().find(|entry| entry.host_id == host_id);
    let pick = |value: Option<&'a str>| value.filter(|value| !value.is_empty());
    (
        pick(entry.map(|entry| entry.icon.as_str())),
        pick(entry.map(|entry| entry.color.as_str())),
    )
}

/// `appearances` with `host_id` given `icon` and `color` at `now_ms`. The
/// timestamp never moves backwards for that host, so paired hosts holding the
/// previous entry accept the change.
pub fn with_appearance(
    appearances: &[HostAppearance],
    host_id: &str,
    icon: String,
    color: String,
    now_ms: u64,
) -> Vec<HostAppearance> {
    let previous = appearances
        .iter()
        .find(|entry| entry.host_id == host_id)
        .map_or(0, |entry| entry.updated_at_ms);
    let updated = HostAppearance {
        host_id: host_id.to_owned(),
        icon,
        color,
        updated_at_ms: now_ms.max(previous + 1),
    };
    appearances
        .iter()
        .filter(|entry| entry.host_id != host_id)
        .cloned()
        .chain(std::iter::once(updated))
        .collect()
}

/// `current` merged with entries from a paired host, keeping the newer entry
/// for each host. Malformed entries, including icons or colours this version
/// does not know, are skipped; a notice with more entries than
/// `MAX_SHARED_APPEARANCES` is rejected. Returns `None` when nothing changes.
pub fn merged_appearances(
    current: &[HostAppearance],
    incoming: &[HostAppearance],
) -> Option<Vec<HostAppearance>> {
    if incoming.len() > MAX_SHARED_APPEARANCES {
        return None;
    }
    let mut merged = current.to_vec();
    let mut changed = false;
    for entry in incoming {
        if !is_valid_host_id(&entry.host_id) || validate(&entry.icon, &entry.color).is_err() {
            continue;
        }
        match merged
            .iter_mut()
            .find(|existing| existing.host_id == entry.host_id)
        {
            Some(existing) if existing.updated_at_ms >= entry.updated_at_ms => {}
            Some(existing) => {
                *existing = entry.clone();
                changed = true;
            }
            None => {
                merged.push(entry.clone());
                changed = true;
            }
        }
    }
    changed.then_some(merged)
}

/// Whether a paired host holding `theirs` would gain anything from our entries.
pub fn has_newer_entries(ours: &[HostAppearance], theirs: &[HostAppearance]) -> bool {
    merged_appearances(theirs, &shareable_appearances(ours)).is_some()
}

/// The newest entries that fit in one notice.
pub fn shareable_appearances(appearances: &[HostAppearance]) -> Vec<HostAppearance> {
    let mut newest = appearances.to_vec();
    newest.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_ms));
    newest.truncate(MAX_SHARED_APPEARANCES);
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC_ID: &str = "2cf05de0c029-windows";
    const MAC_ID: &str = "aabbccddeeff-mac";

    fn look(host_id: &str, icon: &str, color: &str, updated_at_ms: u64) -> HostAppearance {
        HostAppearance {
            host_id: host_id.to_owned(),
            icon: icon.to_owned(),
            color: color.to_owned(),
            updated_at_ms,
        }
    }

    #[test]
    fn only_known_icons_and_colours_are_accepted() {
        assert_eq!(validate("gamepad", "orange"), Ok(()));
        assert_eq!(validate("", ""), Ok(()));
        assert_eq!(validate("rocket", ""), Err(AppearanceError::UnknownIcon));
        assert_eq!(validate("", "#ff0000"), Err(AppearanceError::UnknownColor));
        assert_eq!(
            validate("<img src=x>", "blue"),
            Err(AppearanceError::UnknownIcon)
        );
    }

    #[test]
    fn empty_values_fall_back_to_the_default() {
        let appearances = vec![look(PC_ID, "gamepad", "", 10)];

        assert_eq!(appearance_for(&appearances, PC_ID), (Some("gamepad"), None));
        assert_eq!(appearance_for(&appearances, MAC_ID), (None, None));
    }

    #[test]
    fn changing_replaces_the_entry_with_a_newer_timestamp() {
        let appearances = vec![
            look(PC_ID, "desktop", "blue", 500),
            look(MAC_ID, "laptop", "teal", 20),
        ];

        let changed = with_appearance(
            &appearances,
            PC_ID,
            "gamepad".to_owned(),
            "red".to_owned(),
            100,
        );

        assert_eq!(
            appearance_for(&changed, PC_ID),
            (Some("gamepad"), Some("red"))
        );
        assert_eq!(
            appearance_for(&changed, MAC_ID),
            (Some("laptop"), Some("teal"))
        );
        let entry = changed.iter().find(|entry| entry.host_id == PC_ID).unwrap();
        assert_eq!(entry.updated_at_ms, 501);
    }

    #[test]
    fn merging_keeps_the_newer_entry_for_each_host() {
        let current = vec![
            look(PC_ID, "gamepad", "red", 200),
            look(MAC_ID, "laptop", "teal", 100),
        ];
        let incoming = vec![
            look(PC_ID, "server", "blue", 150),
            look(MAC_ID, "work", "green", 300),
        ];

        let merged = merged_appearances(&current, &incoming).unwrap();

        assert_eq!(
            appearance_for(&merged, PC_ID),
            (Some("gamepad"), Some("red"))
        );
        assert_eq!(
            appearance_for(&merged, MAC_ID),
            (Some("work"), Some("green"))
        );
        assert_eq!(merged_appearances(&merged, &merged), None);
    }

    #[test]
    fn a_newer_reset_from_a_peer_restores_the_default() {
        let current = vec![look(PC_ID, "gamepad", "red", 200)];

        let merged = merged_appearances(&current, &[look(PC_ID, "", "", 300)]).unwrap();

        assert_eq!(appearance_for(&merged, PC_ID), (None, None));
    }

    #[test]
    fn malformed_or_oversized_notices_are_not_adopted() {
        assert_eq!(
            merged_appearances(&[], &[look("<bad id>", "tv", "", 1)]),
            None
        );
        assert_eq!(
            merged_appearances(&[], &[look(PC_ID, "rocket", "", 1)]),
            None
        );
        assert_eq!(
            merged_appearances(&[], &[look(PC_ID, "", "chartreuse", 1)]),
            None
        );
        let too_many: Vec<HostAppearance> = (0..=MAX_SHARED_APPEARANCES)
            .map(|index| look(&format!("host-{index}"), "tv", "", 1))
            .collect();
        assert_eq!(merged_appearances(&[], &too_many), None);
    }

    #[test]
    fn a_peer_needs_ours_only_when_we_hold_something_newer() {
        let ours = vec![look(PC_ID, "gamepad", "red", 200)];

        assert!(has_newer_entries(&ours, &[]));
        assert!(has_newer_entries(&ours, &[look(PC_ID, "tv", "", 100)]));
        assert!(!has_newer_entries(&ours, &ours));
        assert!(!has_newer_entries(&[], &ours));
    }
}
