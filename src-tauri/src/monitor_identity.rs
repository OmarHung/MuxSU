//! User-declared equivalences between the EDID identities of one display,
//! shared between paired hosts.
//!
//! A display is normally identified by its full EDID fingerprint, and that is
//! what makes switching safe. Some displays break the assumption that the
//! fingerprint is stable: the MSI MPG 274U publishes `MSI:3CF0` at 3840x2160
//! and `MSI:7CF0` at 1920x1080, so a mode switch reads as a different display
//! on every host at once and the equivalence cannot be derived — only the user
//! can assert it.
//!
//! The two hosts do not even agree on what the fingerprint is: Windows reads
//! that display's serial number and macOS reads none from it. A claim is
//! therefore matched on `same_identity` rather than on an exact fingerprint,
//! or a claim made on one host would never apply on the other.
//!
//! A claim maps one `alias` identity onto one `primary` identity. Each entry
//! carries its own timestamp, like `input_label`, so paired hosts merge entry
//! by entry. Claims never widen what may be *switched*: they decide which
//! identities count as the same shared display, while every write still
//! demands an exact fingerprint match against a display present right now.

use muxsu_core::{MonitorFingerprint, MonitorIdentityLink};

/// Most entries sent in or accepted from one notice; keeps a notice well under
/// the agent's packet limit.
pub const MAX_SHARED_LINKS: usize = 32;

/// Longest EDID field a claim may carry. Real ones are a three-letter vendor,
/// a four-digit product code and a serial of at most 13 characters.
const MAX_FINGERPRINT_FIELD_LEN: usize = 32;

/// Longest alias chain followed before giving up, so a malformed or hostile
/// notice cannot spin `primary_for` on a cycle.
const MAX_CHAIN_DEPTH: usize = 8;

/// Whether two fingerprints name the same identity for the purpose of a claim.
///
/// Deliberately not `matches_exactly`, which treats "one side has a serial
/// number, the other does not" as a difference. Two hosts reading the same
/// panel differ in exactly that way: Windows reads this MSI's serial, macOS
/// reads none from it. A claim made on one host could then never match
/// anything on the other — it arrived, sat unused, and the display went on
/// being offered for merging, so merging it again added a second entry that
/// looked identical to the first.
///
/// Two different serials still mean two displays. Only an absent one is read
/// as unknown rather than as a difference, and a host that cannot read a
/// serial cannot tell two of a model apart on any other basis either.
///
/// That rule is stricter than the hardware warrants, and deliberately so. An
/// EDID holds two unrelated serials — a 32-bit number and a text descriptor —
/// and each host reads only one of them, so one display shared between two
/// computers can report two entirely different serials: this pair of machines
/// reads an Acer VG252Q as `TH6TT0028525` on Windows and `576726074` on macOS.
/// This function has no way to tell that apart from two units of the model, so
/// it calls them two displays. What keeps that from costing a switch is
/// `shared_monitor_index_for_peer`, which falls back to the model alone when no
/// identity matches, and refuses as soon as more than one display could be
/// meant. Nothing here should be loosened to cover the case instead.
///
/// Switching is untouched by this: a write still demands an exact match
/// against a display present right now, and refuses outright when more than
/// one display matches.
///
/// The webview receives this module's resolved identity for every fingerprint
/// it renders, so this remains the single implementation of the rule.
pub fn same_identity(left: &MonitorFingerprint, right: &MonitorFingerprint) -> bool {
    left.is_same_model(right)
        && match (&left.serial_number, &right.serial_number) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
}

fn is_entry_for(entry: &MonitorIdentityLink, alias: &MonitorFingerprint) -> bool {
    same_identity(&entry.alias, alias)
}

fn linked_primary<'a>(
    links: &'a [MonitorIdentityLink],
    alias: &MonitorFingerprint,
) -> Option<&'a MonitorFingerprint> {
    links
        .iter()
        .find(|entry| is_entry_for(entry, alias))
        .and_then(|entry| entry.primary.as_ref())
}

/// The identity `fingerprint` should be treated as, following the user's
/// claims. An identity with no claim is its own primary, and a chain that
/// loops or runs too deep resolves to the last identity reached rather than
/// spinning.
pub fn primary_for<'a>(
    links: &'a [MonitorIdentityLink],
    fingerprint: &'a MonitorFingerprint,
) -> &'a MonitorFingerprint {
    let mut current = fingerprint;
    for _ in 0..MAX_CHAIN_DEPTH {
        match linked_primary(links, current) {
            // A claim pointing at itself is not a step; stop rather than loop.
            Some(next) if !same_identity(next, current) => current = next,
            _ => return current,
        }
    }
    current
}

/// Every identity that resolves to `primary`, `primary` itself first. Used to
/// decide whether a display present right now is the selected shared display.
pub fn identities_for(
    links: &[MonitorIdentityLink],
    primary: &MonitorFingerprint,
) -> Vec<MonitorFingerprint> {
    let mut identities = vec![primary.clone()];
    for entry in links {
        if entry.primary.is_none() || same_identity(&entry.alias, primary) {
            continue;
        }
        if same_identity(primary_for(links, &entry.alias), primary) {
            identities.push(entry.alias.clone());
        }
    }
    identities
}

/// Whether two identities name one physical display. Being the same display is
/// an equivalence, so both sides are resolved: a stored selection can itself be
/// an alias, and comparing only the observed side would never match it.
pub fn is_same_display(
    links: &[MonitorIdentityLink],
    left: &MonitorFingerprint,
    right: &MonitorFingerprint,
) -> bool {
    same_identity(primary_for(links, left), primary_for(links, right))
}

/// `links` with `alias` claimed as `primary` (`None` withdraws the claim) at
/// `now_ms`. The timestamp never moves backwards for that alias, so paired
/// hosts holding the previous entry accept the change.
pub fn with_link(
    links: &[MonitorIdentityLink],
    alias: &MonitorFingerprint,
    primary: Option<&MonitorFingerprint>,
    now_ms: u64,
) -> Vec<MonitorIdentityLink> {
    let previous = links
        .iter()
        .find(|entry| is_entry_for(entry, alias))
        .map_or(0, |entry| entry.updated_at_ms);
    let updated = MonitorIdentityLink {
        alias: alias.clone(),
        primary: primary.cloned(),
        updated_at_ms: now_ms.max(previous + 1),
    };
    links
        .iter()
        .filter(|entry| !is_entry_for(entry, alias))
        .cloned()
        .chain(std::iter::once(updated))
        .collect()
}

/// Whether every field of `fingerprint` is one an EDID could hold, so a
/// claim cannot carry an arbitrarily large identity.
fn is_well_formed(fingerprint: &MonitorFingerprint) -> bool {
    [
        Some(fingerprint.manufacturer_id.as_str()),
        Some(fingerprint.product_code.as_str()),
        fingerprint.serial_number.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|field| field.len() <= MAX_FINGERPRINT_FIELD_LEN && !field.chars().any(char::is_control))
}

/// `current` merged with claims from a paired host, keeping the newer entry
/// for each alias. A claim an identity makes about itself, or one naming a
/// malformed identity, is skipped, and a notice with more entries than
/// `MAX_SHARED_LINKS` is rejected. Claims are returned in every Ping, so once
/// `MAX_SHARED_LINKS` are held only existing ones are updated. Returns `None`
/// when nothing changes.
pub fn merged_links(
    current: &[MonitorIdentityLink],
    incoming: &[MonitorIdentityLink],
) -> Option<Vec<MonitorIdentityLink>> {
    if incoming.len() > MAX_SHARED_LINKS {
        return None;
    }
    let mut merged = current.to_vec();
    let mut changed = false;
    for entry in incoming {
        if !is_well_formed(&entry.alias)
            || entry.primary.as_ref().is_some_and(|primary| {
                !is_well_formed(primary) || same_identity(primary, &entry.alias)
            })
        {
            continue;
        }
        let is_full = merged.len() >= MAX_SHARED_LINKS;
        match merged
            .iter_mut()
            .find(|existing| is_entry_for(existing, &entry.alias))
        {
            Some(existing) if existing.updated_at_ms >= entry.updated_at_ms => {}
            Some(existing) => {
                *existing = entry.clone();
                changed = true;
            }
            None if is_full => {}
            None => {
                merged.push(entry.clone());
                changed = true;
            }
        }
    }
    changed.then_some(merged)
}

/// `links` with entries naming one identity collapsed to the newest of them,
/// or `None` when there is nothing to collapse.
///
/// Matching a claim used to demand an exact fingerprint, so the same
/// equivalence arriving from a host that reads serial numbers differently was
/// stored a second time instead of recognised. Both entries then rendered
/// identically — the same display merged into the same display, listed twice —
/// and there was no way to tell from the screen which was which.
pub fn deduplicated(links: &[MonitorIdentityLink]) -> Option<Vec<MonitorIdentityLink>> {
    let mut kept: Vec<MonitorIdentityLink> = Vec::with_capacity(links.len());
    for entry in links {
        match kept
            .iter_mut()
            .find(|existing| same_identity(&existing.alias, &entry.alias))
        {
            Some(existing) if existing.updated_at_ms >= entry.updated_at_ms => {}
            Some(existing) => *existing = entry.clone(),
            None => kept.push(entry.clone()),
        }
    }
    (kept.len() != links.len()).then_some(kept)
}

/// Whether a paired host holding `theirs` is missing anything in `ours`, so a
/// notice is only sent when it would change something.
pub fn needs_push(ours: &[MonitorIdentityLink], theirs: &[MonitorIdentityLink]) -> bool {
    ours.iter().any(|entry| {
        !theirs.iter().any(|other| {
            is_entry_for(other, &entry.alias) && other.updated_at_ms >= entry.updated_at_ms
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serialled(product: &str, serial: &str) -> MonitorFingerprint {
        MonitorFingerprint::new("MSI", product, Some(serial.to_owned()))
    }

    /// Windows reads this display's serial number and macOS reads none from
    /// it, so a claim made on one host has to apply on the other.
    #[test]
    fn a_claim_applies_across_hosts_that_read_the_serial_differently() {
        let from_windows = vec![MonitorIdentityLink {
            alias: serialled("7CF0", "CF0H246200009"),
            primary: Some(serialled("3CF0", "CF0H246200009")),
            updated_at_ms: 1,
        }];

        // What this Mac sees: the same panel, with no serial in either mode.
        assert!(is_same_display(
            &from_windows,
            &fingerprint("7CF0"),
            &fingerprint("3CF0")
        ));
    }

    /// The widening goes only as far as an absent serial. Two displays that
    /// both report one, and report different ones, stay two displays.
    #[test]
    fn two_displays_with_different_serials_are_still_two_displays() {
        let links = vec![MonitorIdentityLink {
            alias: serialled("3CF0", "first-panel"),
            primary: Some(fingerprint("7CF0")),
            updated_at_ms: 1,
        }];

        assert!(!is_same_display(
            &links,
            &serialled("3CF0", "second-panel"),
            &fingerprint("7CF0")
        ));
    }

    #[test]
    fn claims_naming_one_display_collapse_to_the_newest() {
        let links = vec![
            MonitorIdentityLink {
                alias: serialled("7CF0", "CF0H246200009"),
                primary: Some(serialled("3CF0", "CF0H246200009")),
                updated_at_ms: 1,
            },
            MonitorIdentityLink {
                alias: fingerprint("7CF0"),
                primary: Some(fingerprint("3CF0")),
                updated_at_ms: 2,
            },
        ];

        let collapsed = deduplicated(&links).expect("two claims name one display");

        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].updated_at_ms, 2);
        assert!(
            deduplicated(&collapsed).is_none(),
            "nothing left to collapse"
        );
    }

    fn fingerprint(product: &str) -> MonitorFingerprint {
        MonitorFingerprint::new("MSI", product, None::<String>)
    }

    fn link(alias: &str, primary: Option<&str>, updated_at_ms: u64) -> MonitorIdentityLink {
        MonitorIdentityLink {
            alias: fingerprint(alias),
            primary: primary.map(fingerprint),
            updated_at_ms,
        }
    }

    #[test]
    fn an_identity_without_a_claim_is_its_own_primary() {
        let alone = fingerprint("3CF0");

        assert_eq!(primary_for(&[], &alone), &alone);
    }

    #[test]
    fn a_claimed_identity_resolves_to_the_display_it_was_merged_into() {
        let links = [link("7CF0", Some("3CF0"), 10)];

        assert_eq!(
            primary_for(&links, &fingerprint("7CF0")),
            &fingerprint("3CF0")
        );
        assert!(is_same_display(
            &links,
            &fingerprint("3CF0"),
            &fingerprint("7CF0")
        ));
    }

    #[test]
    fn a_withdrawn_claim_stops_resolving() {
        let links = with_link(
            &[link("7CF0", Some("3CF0"), 10)],
            &fingerprint("7CF0"),
            None,
            20,
        );

        assert_eq!(
            primary_for(&links, &fingerprint("7CF0")),
            &fingerprint("7CF0")
        );
        assert!(!is_same_display(
            &links,
            &fingerprint("3CF0"),
            &fingerprint("7CF0")
        ));
    }

    #[test]
    fn a_stored_selection_that_is_itself_an_alias_still_matches_its_display() {
        // A selection stored under the alias must still recognise itself and
        // the display it points at; otherwise nothing matches it and every
        // "add to shared" makes another copy.
        let links = [link("7CF0", Some("3CF0"), 10)];

        assert!(is_same_display(
            &links,
            &fingerprint("7CF0"),
            &fingerprint("7CF0")
        ));
        assert!(is_same_display(
            &links,
            &fingerprint("7CF0"),
            &fingerprint("3CF0")
        ));
    }

    #[test]
    fn two_aliases_of_one_display_name_the_same_display() {
        let links = [
            link("7CF0", Some("3CF0"), 10),
            link("5CF0", Some("3CF0"), 10),
        ];

        assert!(is_same_display(
            &links,
            &fingerprint("7CF0"),
            &fingerprint("5CF0")
        ));
    }

    #[test]
    fn an_unrelated_identity_is_never_treated_as_the_shared_display() {
        let links = [link("7CF0", Some("3CF0"), 10)];

        assert!(!is_same_display(
            &links,
            &fingerprint("3CF0"),
            &MonitorFingerprint::new("ACR", "0725", Some("576726074".to_owned()))
        ));
    }

    #[test]
    fn a_serial_number_still_separates_two_displays_of_the_same_model() {
        let ours = MonitorFingerprint::new("DEL", "A1B2", Some("first".to_owned()));
        let theirs = MonitorFingerprint::new("DEL", "A1B2", Some("second".to_owned()));

        assert!(!is_same_display(&[], &ours, &theirs));
    }

    #[test]
    fn a_chain_of_claims_resolves_to_the_end_of_the_chain() {
        let links = [
            link("7CF0", Some("5CF0"), 10),
            link("5CF0", Some("3CF0"), 10),
        ];

        assert_eq!(
            primary_for(&links, &fingerprint("7CF0")),
            &fingerprint("3CF0")
        );
    }

    #[test]
    fn a_looping_claim_resolves_instead_of_spinning() {
        let links = [
            link("7CF0", Some("3CF0"), 10),
            link("3CF0", Some("7CF0"), 10),
        ];

        // Either end is an acceptable answer; not hanging is the point.
        let start = fingerprint("7CF0");
        let resolved = primary_for(&links, &start);
        assert!(resolved == &fingerprint("3CF0") || resolved == &fingerprint("7CF0"));
    }

    #[test]
    fn every_alias_of_a_display_is_listed_with_the_primary_first() {
        let links = [
            link("7CF0", Some("3CF0"), 10),
            link("5CF0", Some("3CF0"), 10),
        ];

        assert_eq!(
            identities_for(&links, &fingerprint("3CF0")),
            vec![
                fingerprint("3CF0"),
                fingerprint("7CF0"),
                fingerprint("5CF0")
            ]
        );
    }

    #[test]
    fn a_display_without_claims_lists_only_itself() {
        assert_eq!(
            identities_for(&[], &fingerprint("3CF0")),
            vec![fingerprint("3CF0")]
        );
    }

    #[test]
    fn setting_a_claim_again_moves_its_timestamp_forward() {
        let first = with_link(&[], &fingerprint("7CF0"), Some(&fingerprint("3CF0")), 10);
        let second = with_link(&first, &fingerprint("7CF0"), None, 5);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].updated_at_ms, 11);
    }

    #[test]
    fn a_paired_host_claim_is_adopted_and_an_older_one_is_ignored() {
        let ours = [link("7CF0", Some("3CF0"), 20)];

        assert_eq!(merged_links(&ours, &[link("7CF0", None, 10)]), None);
        assert_eq!(
            merged_links(&ours, &[link("7CF0", None, 30)]),
            Some(vec![link("7CF0", None, 30)])
        );
    }

    /// Claims are stored and sent back in every Ping, so what a paired host can
    /// add is bounded: no oversized identities, and no more than one notice's
    /// worth of claims in total.
    #[test]
    fn a_malformed_claim_is_skipped_and_the_stored_claims_stay_bounded() {
        let oversized = MonitorIdentityLink {
            alias: MonitorFingerprint::new("MSI", "X".repeat(200), None::<String>),
            primary: None,
            updated_at_ms: 10,
        };
        assert_eq!(merged_links(&[], &[oversized]), None);

        let full: Vec<MonitorIdentityLink> = (0..MAX_SHARED_LINKS)
            .map(|index| link(&format!("{index:04X}"), None, 10))
            .collect();
        assert_eq!(merged_links(&full, &[link("FFFF", None, 10)]), None);

        let updated = merged_links(&full, &[link("0000", Some("3CF0"), 20)]).unwrap();
        assert_eq!(updated.len(), MAX_SHARED_LINKS);
    }

    #[test]
    fn a_claim_an_identity_makes_about_itself_is_skipped() {
        assert_eq!(merged_links(&[], &[link("3CF0", Some("3CF0"), 10)]), None);
    }

    #[test]
    fn an_oversized_notice_is_rejected_whole() {
        let incoming = (0..=MAX_SHARED_LINKS)
            .map(|index| link(&format!("{index:04X}"), Some("3CF0"), 10))
            .collect::<Vec<_>>();

        assert_eq!(merged_links(&[], &incoming), None);
    }

    #[test]
    fn a_notice_is_only_pushed_when_the_paired_host_is_behind() {
        let ours = [link("7CF0", Some("3CF0"), 20)];

        assert!(needs_push(&ours, &[]));
        assert!(needs_push(&ours, &[link("7CF0", Some("3CF0"), 10)]));
        assert!(!needs_push(&ours, &[link("7CF0", Some("3CF0"), 20)]));
    }
}
