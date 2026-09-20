//! Display models known to publish more than one EDID identity, so the settings
//! page can preselect the identity a newly appeared display probably belongs to.
//!
//! `monitor_identity` holds the rule that only the user may declare two
//! identities to be one display. That rule stays: nothing here writes a claim,
//! and nothing here widens what may be switched. What it removes is the guess.
//! When a display turns up that belongs to no shared display while a shared one
//! has gone unreadable, the user is offered a merge — and until now they were
//! handed a plain list of the missing displays with no indication which one the
//! new identity is. Merging the wrong pair moves one panel's input settings onto
//! another, and is not a mistake they can see on screen.
//!
//! The table is curated by hand from `data/known-identity-groups.json`. Its
//! entries come from hardware notes in `product-facts.md` and from judgments
//! proposed by `scripts/identity-oracle.mjs`, each approved by a person before
//! it lands. Nothing about it runs at display-scan time beyond a lookup: the
//! app never calls a network service to decide this.
//!
//! A group is keyed by vendor and product code only, because "these two product
//! codes are one panel" is a fact about the model rather than about one unit.
//! That is also the limit of the table: someone owning two of the same display,
//! each left in a different mode, would be offered a merge of two genuinely
//! separate panels. So a match only ever preselects an option the user can
//! change, and the declaration remains theirs.

use muxsu_core::MonitorFingerprint;
use serde::Deserialize;
use std::sync::OnceLock;

const TABLE_JSON: &str = include_str!("../data/known-identity-groups.json");

#[derive(Debug, Deserialize)]
struct Table {
    #[serde(default)]
    groups: Vec<Group>,
}

#[derive(Debug, Deserialize)]
struct Group {
    #[serde(default)]
    identities: Vec<Identity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    manufacturer_id: String,
    product_code: String,
}

/// One group as the lookup uses it: the `MANUFACTURER:PRODUCT` keys of every
/// identity the same panel is known to publish.
type ModelKeys = Vec<String>;

fn model_key_of(manufacturer_id: &str, product_code: &str) -> String {
    format!(
        "{}:{}",
        manufacturer_id.trim().to_ascii_uppercase(),
        product_code.trim().to_ascii_uppercase()
    )
}

fn model_key(fingerprint: &MonitorFingerprint) -> String {
    model_key_of(&fingerprint.manufacturer_id, &fingerprint.product_code)
}

/// The shipped table, or an empty one when it cannot be read. A malformed table
/// is a build-time mistake, and the tests below fail on it; at runtime it costs
/// the user a preselected option rather than the app.
fn groups() -> &'static [ModelKeys] {
    static GROUPS: OnceLock<Vec<ModelKeys>> = OnceLock::new();
    GROUPS.get_or_init(|| match serde_json::from_str::<Table>(TABLE_JSON) {
        Ok(table) => table
            .groups
            .into_iter()
            .map(|group| {
                group
                    .identities
                    .iter()
                    .map(|identity| model_key_of(&identity.manufacturer_id, &identity.product_code))
                    .collect()
            })
            .filter(|keys: &ModelKeys| keys.len() > 1)
            .collect(),
        Err(error) => {
            tracing::error!(%error, "known display identity table could not be read");
            Vec::new()
        }
    })
}

/// Whether the table records these two identities as belonging to one panel.
///
/// False for two identities that are already the same model: nothing there
/// needs a group, and `monitor_identity::same_identity` covers it.
pub fn same_panel(left: &MonitorFingerprint, right: &MonitorFingerprint) -> bool {
    let (left, right) = (model_key(left), model_key(right));
    left != right
        && groups()
            .iter()
            .any(|keys| keys.contains(&left) && keys.contains(&right))
}

/// The one candidate the table groups with `alias`, or `None` when it groups
/// none of them or more than one.
///
/// Refusing an ambiguous match is the same discipline switching uses: with two
/// candidates the table cannot say which the user means, and preselecting either
/// would be a guess dressed up as an answer.
pub fn suggested_primary(alias: &MonitorFingerprint, candidates: &[MonitorFingerprint]) -> Option<usize> {
    let mut matched = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| same_panel(alias, candidate));
    let (index, _) = matched.next()?;
    matched.next().is_none().then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(manufacturer_id: &str, product_code: &str) -> MonitorFingerprint {
        MonitorFingerprint::new(manufacturer_id, product_code, None::<String>)
    }

    #[test]
    fn the_shipped_table_parses_and_carries_the_msi_panel() {
        assert!(
            !groups().is_empty(),
            "data/known-identity-groups.json must parse into at least one group"
        );
        assert!(same_panel(
            &fingerprint("MSI", "3CF0"),
            &fingerprint("MSI", "7CF0")
        ));
    }

    #[test]
    fn a_group_is_symmetric_and_ignores_case_and_padding() {
        assert!(same_panel(
            &fingerprint("MSI", "7CF0"),
            &fingerprint("MSI", "3CF0")
        ));
        assert!(same_panel(
            &fingerprint(" msi ", "3cf0"),
            &fingerprint("MSI", "7CF0")
        ));
    }

    #[test]
    fn a_serial_number_does_not_change_whether_two_models_are_one_panel() {
        let with_serial =
            MonitorFingerprint::new("MSI", "3CF0", Some("PC-SERIAL".to_owned()));

        assert!(same_panel(&with_serial, &fingerprint("MSI", "7CF0")));
    }

    #[test]
    fn identities_outside_the_table_and_the_same_model_are_not_grouped() {
        assert!(!same_panel(
            &fingerprint("MSI", "3CF0"),
            &fingerprint("ACR", "0725")
        ));
        assert!(!same_panel(
            &fingerprint("AOC", "2402"),
            &fingerprint("AOC", "2403")
        ));
        // One model is not a group of two identities.
        assert!(!same_panel(
            &fingerprint("MSI", "3CF0"),
            &fingerprint("MSI", "3CF0")
        ));
    }

    #[test]
    fn a_single_grouped_candidate_is_suggested() {
        let candidates = [
            fingerprint("ACR", "0725"),
            fingerprint("MSI", "7CF0"),
            fingerprint("AOC", "2402"),
        ];

        assert_eq!(
            suggested_primary(&fingerprint("MSI", "3CF0"), &candidates),
            Some(1)
        );
    }

    #[test]
    fn nothing_is_suggested_when_no_candidate_or_more_than_one_is_grouped() {
        let unrelated = [fingerprint("ACR", "0725"), fingerprint("AOC", "2402")];
        assert_eq!(suggested_primary(&fingerprint("MSI", "3CF0"), &unrelated), None);
        assert_eq!(suggested_primary(&fingerprint("MSI", "3CF0"), &[]), None);

        // Two units of the display's other mode: the table cannot say which one.
        let ambiguous = [
            MonitorFingerprint::new("MSI", "7CF0", Some("FIRST".to_owned())),
            MonitorFingerprint::new("MSI", "7CF0", Some("SECOND".to_owned())),
        ];
        assert_eq!(suggested_primary(&fingerprint("MSI", "3CF0"), &ambiguous), None);
    }
}
