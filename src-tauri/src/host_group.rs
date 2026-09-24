//! Groupings of shared displays and hosts, kept on this computer.
//!
//! A group names the displays and hosts that are used together, so the switch
//! centre can show one arrangement at a time instead of every display and
//! every host at once. Someone with two PCs, a Mac and several displays rarely
//! switches all of them together; a group is the subset that moves as one.
//!
//! Unlike host names or the host order, a group is never shared with paired
//! hosts. Which displays and hosts belong together depends on where a computer
//! sits: the machine at the desk with two of the displays groups them
//! differently from the one in the next room, and a grouping pushed from there
//! would describe neither. So groups stay on the computer that defined them.

use serde::{Deserialize, Serialize};

/// Longest group name accepted, in characters. Matches the limit on custom
/// host names, so both read the same in the same lists.
pub const MAX_GROUP_NAME_CHARS: usize = 32;
/// Most groups kept. Well past what the switch centre can show as tabs, and
/// enough that the limit is never met in ordinary use.
pub const MAX_GROUPS: usize = 16;

/// A set of shared displays and hosts worked with together.
///
/// An empty `monitor_keys` or `host_ids` places no restriction on that half:
/// a group can name only the hosts it covers and still show every display, so
/// "the two machines at this desk" needs no display list to be useful.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostGroup {
    pub id: String,
    pub name: String,
    /// `monitor_key` of each shared display in the group.
    #[serde(default)]
    pub monitor_keys: Vec<String>,
    /// Discovery ids: paired hosts, and this computer's own `local_host_id`.
    #[serde(default)]
    pub host_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GroupError {
    EmptyName,
    NameTooLong,
    ControlCharacter,
    TooManyGroups,
}

/// Trims `input` and checks it can be shown as a group name.
///
/// An empty name is refused rather than treated as "use a default", because a
/// group is only ever named by its user — there is no machine name to fall
/// back to the way a host has one.
pub fn normalize_group_name(input: &str) -> Result<String, GroupError> {
    let name = input.trim();
    if name.chars().any(char::is_control) {
        return Err(GroupError::ControlCharacter);
    }
    if name.is_empty() {
        return Err(GroupError::EmptyName);
    }
    if name.chars().count() > MAX_GROUP_NAME_CHARS {
        return Err(GroupError::NameTooLong);
    }
    Ok(name.to_owned())
}

/// The group with `id`, if one is defined.
pub fn group_for<'a>(groups: &'a [HostGroup], id: &str) -> Option<&'a HostGroup> {
    groups.iter().find(|group| group.id == id)
}

/// The group the switch centre is showing, or `None` for "show everything".
///
/// An `active_id` naming a group that no longer exists resolves to `None`, so
/// deleting the active group on one screen cannot leave another showing an
/// arrangement that is gone.
pub fn active_group<'a>(groups: &'a [HostGroup], active_id: &str) -> Option<&'a HostGroup> {
    (!active_id.is_empty())
        .then(|| group_for(groups, active_id))
        .flatten()
}

/// `groups` with `group` added, or replaced when its id is already present.
pub fn with_group(groups: &[HostGroup], group: HostGroup) -> Result<Vec<HostGroup>, GroupError> {
    let name = normalize_group_name(&group.name)?;
    let group = HostGroup { name, ..group };
    let mut updated = groups.to_vec();
    match updated.iter().position(|saved| saved.id == group.id) {
        Some(index) => updated[index] = group,
        None if updated.len() >= MAX_GROUPS => return Err(GroupError::TooManyGroups),
        None => updated.push(group),
    }
    Ok(updated)
}

/// `groups` without the one named by `id`.
pub fn without_group(groups: &[HostGroup], id: &str) -> Vec<HostGroup> {
    groups
        .iter()
        .filter(|group| group.id != id)
        .cloned()
        .collect()
}

/// Whether `group` covers the display `monitor_key` names. A group that lists
/// no displays covers all of them.
pub fn includes_monitor(group: &HostGroup, monitor_key: &str) -> bool {
    group.monitor_keys.is_empty() || group.monitor_keys.iter().any(|key| key == monitor_key)
}

/// Whether `group` covers the host `host_id` names. A group that lists no
/// hosts covers all of them.
pub fn includes_host(group: &HostGroup, host_id: &str) -> bool {
    group.host_ids.is_empty() || group.host_ids.iter().any(|id| id == host_id)
}

/// Drops displays and hosts from `group` that the computer no longer has, so a
/// removed display or an unpaired host does not linger in a group forever.
pub fn pruned_group(group: &HostGroup, monitor_keys: &[String], host_ids: &[String]) -> HostGroup {
    HostGroup {
        id: group.id.clone(),
        name: group.name.clone(),
        monitor_keys: group
            .monitor_keys
            .iter()
            .filter(|key| monitor_keys.contains(key))
            .cloned()
            .collect(),
        host_ids: group
            .host_ids
            .iter()
            .filter(|id| host_ids.contains(id))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, name: &str) -> HostGroup {
        HostGroup {
            id: id.to_owned(),
            name: name.to_owned(),
            monitor_keys: vec!["ACR:1234:serial-a".to_owned()],
            host_ids: vec!["host-1".to_owned()],
        }
    }

    #[test]
    fn a_name_of_only_spaces_is_refused() {
        assert_eq!(normalize_group_name("   "), Err(GroupError::EmptyName));
    }

    #[test]
    fn a_name_keeps_its_text_and_loses_its_padding() {
        assert_eq!(normalize_group_name("  書房  "), Ok("書房".to_owned()));
    }

    #[test]
    fn a_name_past_the_limit_is_refused() {
        let name = "x".repeat(MAX_GROUP_NAME_CHARS + 1);

        assert_eq!(normalize_group_name(&name), Err(GroupError::NameTooLong));
    }

    #[test]
    fn a_name_carrying_a_control_character_is_refused() {
        assert_eq!(
            normalize_group_name("desk\nsetup"),
            Err(GroupError::ControlCharacter)
        );
    }

    #[test]
    fn saving_a_group_twice_replaces_it_rather_than_listing_it_again() {
        let groups = vec![group("g1", "書房")];

        let updated = with_group(&groups, group("g1", "客廳")).unwrap();

        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].name, "客廳");
    }

    #[test]
    fn groups_stop_at_the_limit() {
        let groups = (0..MAX_GROUPS)
            .map(|index| group(&format!("g{index}"), "群組"))
            .collect::<Vec<_>>();

        let refused = with_group(&groups, group("extra", "再一個"));

        assert_eq!(refused, Err(GroupError::TooManyGroups));
    }

    #[test]
    fn the_limit_does_not_block_editing_a_group_that_already_exists() {
        let groups = (0..MAX_GROUPS)
            .map(|index| group(&format!("g{index}"), "群組"))
            .collect::<Vec<_>>();

        let updated = with_group(&groups, group("g0", "改名")).unwrap();

        assert_eq!(updated.len(), MAX_GROUPS);
        assert_eq!(updated[0].name, "改名");
    }

    #[test]
    fn an_active_group_that_was_deleted_shows_everything_again() {
        let groups = vec![group("g1", "書房")];
        let remaining = without_group(&groups, "g1");

        assert_eq!(active_group(&remaining, "g1"), None);
    }

    #[test]
    fn no_active_group_shows_everything() {
        let groups = vec![group("g1", "書房")];

        assert_eq!(active_group(&groups, ""), None);
    }

    #[test]
    fn a_group_listing_nothing_covers_every_display_and_host() {
        let everything = HostGroup {
            id: "g1".to_owned(),
            name: "全部".to_owned(),
            monitor_keys: Vec::new(),
            host_ids: Vec::new(),
        };

        assert!(includes_monitor(&everything, "ACR:9999:other"));
        assert!(includes_host(&everything, "host-9"));
    }

    #[test]
    fn a_group_covers_only_what_it_lists() {
        let group = group("g1", "書房");

        assert!(includes_monitor(&group, "ACR:1234:serial-a"));
        assert!(!includes_monitor(&group, "ACR:9999:other"));
        assert!(includes_host(&group, "host-1"));
        assert!(!includes_host(&group, "host-9"));
    }

    #[test]
    fn a_display_removed_from_sharing_drops_out_of_its_groups() {
        let group = group("g1", "書房");

        let pruned = pruned_group(&group, &[], &["host-1".to_owned()]);

        assert!(pruned.monitor_keys.is_empty());
        assert_eq!(pruned.host_ids, vec!["host-1".to_owned()]);
        assert_eq!(pruned.name, "書房");
    }

    #[test]
    fn an_unpaired_host_drops_out_of_its_groups() {
        let group = group("g1", "書房");

        let pruned = pruned_group(&group, &["ACR:1234:serial-a".to_owned()], &[]);

        assert!(pruned.host_ids.is_empty());
        assert_eq!(pruned.monitor_keys, vec!["ACR:1234:serial-a".to_owned()]);
    }
}
