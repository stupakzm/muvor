//! Building an `ObjectMatchRule` that `atk-collection` actually answers.
//!
//! **Measured 2026-08-19, and this cost most of an afternoon.** The `atspi`
//! crate's `ObjectMatchRule` builder produces a rule that GNOME's
//! `Collection` implementation answers with an empty array — no error, no
//! warning, just nothing, on a window where pyatspi finds four buttons with
//! what looks like the same query. Three separate causes, each sufficient on
//! its own:
//!
//! 1. **Every match type must be set.** `MatchType::Invalid` is the builder's
//!    default for any field left untouched, and the server treats `Invalid`
//!    as "match nothing" rather than "ignore this criterion". A rule that
//!    sets only roles and states still carries `Invalid` for attributes and
//!    interfaces, and therefore matches nothing at all.
//! 2. **Roles travel as a bitmask, not as a list.** The signature is `ai`
//!    either way, which is why this passes signature validation and fails in
//!    practice: the server reads four `i32`s as 128 role bits, so a list of
//!    role *numbers* is read as a nonsense set of roles. `[43, 62, 37, 79]`
//!    returns 0 matches; the same roles as a mask return the 4 real buttons.
//! 3. **`traverse` must be `true`**, despite being documented as
//!    unimplemented. With `false` the reply is empty.
//!
//! Verified against pyatspi on gnome-terminal: same window, same four
//! buttons (`Close`, `Find`, `Menu`, `New Tab`), 0.2 ms.
//!
//! So the rule is assembled here, by hand, against the wire signature
//! `(aiia{ss}iaiiasib)`.

use std::collections::HashMap;

use atspi::{Role, State, StateSet};

/// `(states, states_mt, attrs, attrs_mt, roles, roles_mt, ifaces, ifaces_mt, invert)`
pub type MatchRule =
    (Vec<i32>, i32, HashMap<String, String>, i32, Vec<i32>, i32, Vec<String>, i32, bool);

/// `ATSPI_Collection_MATCH_ALL`. Also the correct value for an *empty*
/// criterion — see reason 1 above.
pub const MATCH_ALL: i32 = 1;
/// `ATSPI_Collection_MATCH_ANY`.
pub const MATCH_ANY: i32 = 2;
/// `ATSPI_Collection_SORT_ORDER_CANONICAL`. Zero is `INVALID`, not canonical.
pub const SORT_CANONICAL: u32 = 1;

/// Roles are packed into 128 bits, so a role numbered beyond that cannot be
/// asked for at all. `push button menu` (129) is the only one on this list
/// that falls off the end; it survives on the `Cache` path, which carries
/// role numbers whole.
const ROLE_BITS: usize = 128;

/// The §4.4 rule: any actionable role, all three state bits.
pub fn actionable() -> MatchRule {
    (
        state_bits(
            [State::Visible, State::Showing, State::Sensitive].into_iter().collect::<StateSet>(),
        ),
        MATCH_ALL,
        HashMap::new(),
        // Empty, and still `ALL` rather than `Invalid`. Reason 1.
        MATCH_ALL,
        role_mask(crate::filter::ACTIONABLE),
        MATCH_ANY,
        Vec::new(),
        MATCH_ALL,
        false,
    )
}

/// A `StateSet` as the two `i32`s the wire wants. The bit positions are the
/// AT-SPI state numbers, which is also how `enumflags2` lays out `State`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub fn state_bits(states: StateSet) -> Vec<i32> {
    let bits = states.bits();
    vec![bits as u32 as i32, (bits >> 32) as u32 as i32]
}

/// Roles as a 128-bit mask in four `i32`s. Roles at or beyond [`ROLE_BITS`]
/// are dropped rather than wrapped — a silently mis-set bit would ask for
/// some *other* role, which is worse than not asking.
#[allow(clippy::cast_possible_wrap)]
pub fn role_mask(roles: &[Role]) -> Vec<i32> {
    let mut mask = [0u32; ROLE_BITS / 32];
    for role in roles {
        let n = *role as usize;
        if n < ROLE_BITS {
            mask[n / 32] |= 1 << (n % 32);
        }
    }
    mask.iter().map(|w| *w as i32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are AT-SPI's, taken from `Atspi.Role` on this machine, and
    /// they pin the assumption the whole `Collection` path rests on: that the
    /// Rust enum's discriminant *is* the wire role number.
    #[test]
    fn rust_role_discriminants_are_the_wire_numbers() {
        for (role, wire) in [
            (Role::Button, 43u32),
            (Role::ToggleButton, 62),
            (Role::PageTab, 37),
            (Role::Entry, 79),
            (Role::Link, 88),
            (Role::MenuItem, 35),
            (Role::CheckBox, 7),
            (Role::ListItem, 32),
            (Role::TreeItem, 91),
            (Role::ComboBox, 11),
            (Role::RadioButton, 44),
            (Role::Slider, 51),
            (Role::SpinButton, 52),
            (Role::PasswordText, 40),
        ] {
            assert_eq!(role as u32, wire, "{role}");
        }
    }

    /// The exact mask that returned gnome-terminal's four buttons, where the
    /// same roles sent as a list returned nothing.
    #[test]
    fn known_good_mask_from_the_wire() {
        let roles = [Role::Button, Role::ToggleButton, Role::PageTab, Role::Entry];
        assert_eq!(role_mask(&roles), vec![0, 1_073_743_904, 32_768, 0]);
    }

    #[test]
    fn roles_past_the_mask_are_dropped_not_wrapped() {
        // push button menu is 129: one bit past the end of the mask.
        assert!(Role::PushButtonMenu as usize >= ROLE_BITS);
        assert_eq!(role_mask(&[Role::PushButtonMenu]), vec![0, 0, 0, 0]);
    }

    /// The states half, against the value observed on the wire: VISIBLE (30),
    /// SHOWING (25) and SENSITIVE (24) is `0x4300_0000`.
    #[test]
    fn state_bits_match_the_wire() {
        let set: StateSet =
            [State::Visible, State::Showing, State::Sensitive].into_iter().collect();
        assert_eq!(state_bits(set), vec![0x4300_0000, 0]);
    }

    #[test]
    fn no_criterion_is_left_invalid() {
        let (_, smt, _, amt, _, rmt, _, imt, invert) = actionable();
        for mt in [smt, amt, rmt, imt] {
            assert_ne!(mt, 0, "MatchType::Invalid matches nothing, even when the set is empty");
        }
        assert!(!invert);
    }
}
