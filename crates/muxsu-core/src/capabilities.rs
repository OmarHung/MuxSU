use crate::DisplayInput;

/// Parses the value list declared for VCP 0x60 without probing inputs by switching them.
pub(crate) fn parse_input_sources(capabilities: &[u8]) -> Vec<DisplayInput> {
    declared_values(capabilities, "60")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| DisplayInput::new(value).ok())
        .collect()
}

/// The values declared for VCP 0xD6, the display's own power state.
///
/// `None` when the display does not name the feature, or names it without a
/// value list — either way nothing is known and nothing may be concluded. An
/// empty list is impossible: a declared feature with no readable value reads
/// as `None` too.
pub(crate) fn parse_power_states(capabilities: &[u8]) -> Option<Vec<u32>> {
    declared_values(capabilities, "d6").filter(|values| !values.is_empty())
}

/// The sorted, de-duplicated one-byte values a `vcp(...)` group declares for
/// one feature code.
fn declared_values(capabilities: &[u8], code: &str) -> Option<Vec<u32>> {
    let text = String::from_utf8_lossy(capabilities);
    let vcp_body = group_body_after_token(&text, "vcp")?;
    let body = group_body_after_token(vcp_body, code)?;

    let mut values = body
        .split_ascii_whitespace()
        .filter_map(|token| {
            let token = token.trim_matches(|character: char| !character.is_ascii_hexdigit());
            if token.is_empty() || token.len() > 2 {
                return None;
            }
            u32::from_str_radix(token, 16).ok()
        })
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    Some(values)
}

fn group_body_after_token<'a>(text: &'a str, token: &str) -> Option<&'a str> {
    let bytes = text.as_bytes();
    let mut offset = 0;
    while offset + token.len() <= bytes.len() {
        let candidate = &text[offset..offset + token.len()];
        if candidate.eq_ignore_ascii_case(token)
            && is_token_boundary(bytes.get(offset.wrapping_sub(1)).copied())
            && is_token_boundary(bytes.get(offset + token.len()).copied())
        {
            let suffix = &text[offset + token.len()..];
            let whitespace = suffix.len() - suffix.trim_start_matches(char::is_whitespace).len();
            if suffix.as_bytes().get(whitespace) != Some(&b'(') {
                offset += token.len();
                continue;
            }
            let open = offset + token.len() + whitespace;
            let mut depth = 0_u32;
            for (relative, character) in text[open..].char_indices() {
                match character {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(&text[open + 1..open + relative]);
                        }
                    }
                    _ => {}
                }
            }
            return None;
        }
        offset += 1;
    }
    None
}

fn is_token_boundary(character: Option<u8>) -> bool {
    character.is_none_or(|value| !value.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_and_vendor_specific_input_values() {
        let parsed = parse_input_sources(
            b"(prot(monitor)type(LCD)model(X)vcp(10 12 60(0f 11 1b 1B) D6(01 04)))",
        );
        assert_eq!(
            parsed.iter().map(|input| input.value()).collect::<Vec<_>>(),
            vec![0x0f, 0x11, 0x1b]
        );
    }

    #[test]
    fn does_not_confuse_other_feature_values_with_inputs() {
        assert!(parse_input_sources(b"(vcp(10 12(60) D6(01 04)))").is_empty());
        assert!(parse_input_sources(b"(type(LCD))").is_empty());
    }

    /// Observed on an MSI MPG 274U: the display declares VCP 0xD6 with the
    /// one value 0x05 — the deepest off — and never declares 0x01, so DDC/CI
    /// can turn it off and nothing can turn it back on.
    #[test]
    fn reads_the_power_states_a_display_declares() {
        let msi = b"(prot(monitor)type(lcd)MStarcmds(01 02 03)vcp(02 04 C8 C9 D6(05) DC(00 02) DF FD)mccs_ver(2.1))";
        let acer =
            b"(prot(monitor)type(LCD)vcp(02 04 CC(01 02) D6(01 04 05) DF E0(00 03))mccs_ver(2.2))";

        assert_eq!(parse_power_states(msi), Some(vec![0x05]));
        assert_eq!(parse_power_states(acer), Some(vec![0x01, 0x04, 0x05]));
    }

    /// A feature named without a value list says nothing about which values it
    /// takes, and neither does one that is missing. Both must read as unknown,
    /// because refusing on either would refuse on most displays.
    #[test]
    fn a_display_that_declares_no_power_values_is_not_read_as_declaring_none() {
        assert_eq!(parse_power_states(b"(vcp(02 04 D6 DF))"), None);
        assert_eq!(parse_power_states(b"(vcp(02 04 DF))"), None);
        assert_eq!(parse_power_states(b"(type(LCD))"), None);
    }

    /// A display's capabilities string is not trusted to be well formed. The
    /// depth count starts at the opening parenthesis and returns as soon as it
    /// closes, so a stray one can end a group early but never underflow.
    #[test]
    fn stray_closing_parentheses_do_not_underflow_the_depth() {
        let parsed = parse_input_sources(b")) (vcp(60(0f 11)) ))) 60(1b))");
        assert_eq!(
            parsed.iter().map(|input| input.value()).collect::<Vec<_>>(),
            vec![0x0f, 0x11]
        );
        assert!(parse_input_sources(b"(vcp(60(0f 11").is_empty());
    }

    #[test]
    fn accepts_uppercase_and_irregular_spacing() {
        let parsed = parse_input_sources(b"(VCP( 60 ( 0F\n10\tE0 ) ))");
        assert_eq!(
            parsed.iter().map(|input| input.value()).collect::<Vec<_>>(),
            vec![0x0f, 0x10, 0xe0]
        );
    }
}
