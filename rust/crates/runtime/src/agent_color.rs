//! Deterministic color assignment for background sub-agents.
//!
//! Coordinator UIs (and any other multi-agent viewer) benefit from
//! being able to render each concurrent sub-agent with a
//! distinguishable label. The color is derived from `agent_id`, not
//! from `subagent_type`, so two concurrent workers of the SAME type
//! still get different colors.
//!
//! Ports CC-fork's `agentColorManager.ts` palette. Sudocode diverges
//! on the mapping key (they hash by `agentType`; we hash by
//! `agent_id`) because sudocode's `agent-XXX` IDs are already
//! unique per spawn while their preset types often repeat.
//!
//! # Determinism guarantee
//!
//! `assign_agent_color(id)` returns the same value for the same
//! input every time within a process and across processes — the
//! hash is a portable byte-level fold, not [`std::hash::Hasher`]
//! (which uses a randomized seed).

/// 8-color palette, byte-identical to CC-fork's `AGENT_COLORS`.
///
/// Order MATTERS — changing it re-shuffles every existing agent's
/// color, which would be surprising to a coordinator UI that
/// remembers the previous mapping. Adding a NEW color at the end is
/// safe (existing IDs' colors don't move unless they happened to
/// modulo-hash into the new slot).
pub const AGENT_COLOR_PALETTE: &[&str] = &[
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];

/// Assign a palette color to `agent_id`. Returns `None` for empty
/// IDs (which shouldn't occur but is defensive).
///
/// The `subagent_type == "general-purpose"` carve-out from CC-fork
/// is intentionally NOT ported: sudocode uses the color to
/// disambiguate concurrent BACKGROUND agents in the parent's UI,
/// and general-purpose is the MOST common background type — hiding
/// its color would defeat the point.
#[must_use]
pub fn assign_agent_color(agent_id: &str) -> Option<&'static str> {
    if agent_id.is_empty() {
        return None;
    }
    let idx = portable_hash(agent_id.as_bytes()) as usize % AGENT_COLOR_PALETTE.len();
    Some(AGENT_COLOR_PALETTE[idx])
}

/// Portable byte-level hash — matches the Java-style `hashCode`
/// (shift-5 subtract-add), same primitive used in
/// [`crate::memory::loader::simple_hash`]. Kept private to this
/// module so the caller sees a stable palette abstraction.
fn portable_hash(bytes: &[u8]) -> u32 {
    let mut h: i32 = 0;
    for &b in bytes {
        h = h.wrapping_shl(5).wrapping_sub(h).wrapping_add(b as i32);
    }
    (h as i64).unsigned_abs() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn palette_has_exactly_eight_colors() {
        assert_eq!(AGENT_COLOR_PALETTE.len(), 8);
    }

    #[test]
    fn palette_matches_cc_fork_order_and_names() {
        // Locking in CC-fork's exact palette shape — the coordinator
        // UI depends on stable name/order semantics.
        assert_eq!(
            AGENT_COLOR_PALETTE,
            &["red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",]
        );
    }

    #[test]
    fn assignment_is_deterministic_for_same_id() {
        let a1 = assign_agent_color("agent-abc123");
        let a2 = assign_agent_color("agent-abc123");
        assert_eq!(a1, a2);
        assert!(a1.is_some());
    }

    #[test]
    fn different_ids_can_get_different_colors() {
        // Not GUARANTEED across all inputs (pigeon-hole with 8 buckets)
        // — but a small sample of realistic agent_ids should span
        // multiple colors, else our hash is broken.
        let ids = [
            "agent-a1b",
            "agent-c2d",
            "agent-e3f",
            "agent-g4h",
            "agent-i5j",
            "agent-k6l",
            "agent-m7n",
            "agent-o8p",
            "agent-q9r",
            "agent-s0t",
        ];
        let colors: HashSet<_> = ids
            .iter()
            .filter_map(|id| assign_agent_color(id))
            .collect();
        assert!(
            colors.len() >= 3,
            "hash must spread across at least 3 palette buckets in a 10-sample; got {colors:?}"
        );
    }

    #[test]
    fn empty_agent_id_gets_no_color() {
        assert!(assign_agent_color("").is_none());
    }

    #[test]
    fn assigned_value_is_always_from_the_palette() {
        for id in [
            "agent-x",
            "agent-yz",
            "agent-🚀",
            "agent-with-lots-of-hyphens-a-b-c-d-e",
        ] {
            let c = assign_agent_color(id).expect("non-empty id");
            assert!(
                AGENT_COLOR_PALETTE.contains(&c),
                "assigned color {c} MUST come from the palette"
            );
        }
    }

    #[test]
    fn every_palette_slot_reachable_from_some_input() {
        // A weaker version of "hash is well-spread": at least one
        // input among a large-enough space hits each color. Uses
        // `agent-0`..`agent-999` so a bad hash (e.g., always
        // returning `red`) fails loudly.
        let mut seen: HashSet<&str> = HashSet::new();
        for i in 0..1000 {
            let id = format!("agent-{i}");
            if let Some(c) = assign_agent_color(&id) {
                seen.insert(c);
            }
        }
        assert_eq!(
            seen.len(),
            AGENT_COLOR_PALETTE.len(),
            "every palette color MUST be reachable; missing: {:?}",
            AGENT_COLOR_PALETTE
                .iter()
                .filter(|c| !seen.contains(*c))
                .collect::<Vec<_>>()
        );
    }
}
