//! Progress (spec §9.1): xp accrues from presence, talking, sharing,
//! watching and chat; the level is derived from xp, never stored, so
//! the curve can change without a migration.

/// The next level costs `LEVEL_COST_BASE × level^LEVEL_COST_EXPONENT`
/// xp. At the accrual rates in the spec (6 xp/min for someone online
/// and talking) level 10 is about four hours of ordinary use, level 20
/// about a day; tune here.
pub const LEVEL_COST_BASE: f64 = 13.0;
pub const LEVEL_COST_EXPONENT: f64 = 1.5;

/// The xp needed to go from `level` to `level + 1`.
#[must_use]
pub fn cost_of_level(level: u16) -> u64 {
    let cost = LEVEL_COST_BASE * f64::from(level).powf(LEVEL_COST_EXPONENT);
    // Rounded and never below 1: the curve is monotone and finite.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cost = cost.round().max(1.0) as u64;
    cost
}

/// Total xp at which `level` is reached (level 1 = 0 xp).
#[must_use]
pub fn xp_for_level(level: u16) -> u64 {
    (1..level).map(cost_of_level).sum()
}

/// The level a total of `xp` puts someone at. Bounded so a corrupt or
/// absurd value cannot loop forever.
#[must_use]
pub fn level_for_xp(xp: u64) -> u16 {
    let mut level = 1u16;
    let mut spent = 0u64;
    while level < 1000 {
        let next = cost_of_level(level);
        if spent + next > xp {
            break;
        }
        spent += next;
        level += 1;
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_curve_monotonically() {
        assert_eq!(level_for_xp(0), 1);
        assert_eq!(cost_of_level(1), 13);
        assert_eq!(level_for_xp(12), 1);
        assert_eq!(level_for_xp(13), 2);
        assert_eq!(xp_for_level(3), 13 + cost_of_level(2));
        let mut last = 0;
        for xp in (0..100_000).step_by(97) {
            let level = level_for_xp(xp);
            assert!(level >= last);
            assert!(xp_for_level(level) <= xp);
            last = level;
        }
        // Six xp a minute: level 10 around four hours, level 20 around a day.
        let four_hours = 4 * 60 * 6;
        assert!((9..=11).contains(&level_for_xp(four_hours)));
        let one_day = 24 * 60 * 6;
        assert!((18..=22).contains(&level_for_xp(one_day)));
    }
}
