//! What the PC is, said once in the log, and the tier the layer runs at:
//! standard, or low for a PC below what the title was seen to play on.
//!
//! The title asks for 10.5 to 12.7 GB of the card's memory and about
//! 12 threads' worth of the processor, besides what our own threads take
//! (a 16 GB card and a 16-thread processor, 2026-11-26). Below that a
//! session played at 1 to 3 frames a second (a 12 GB card, a 12-thread
//! Intel processor and a nearly full 32 GB of memory, 2026-11-27). What
//! follows the tier:
//! - the size of the display reported to the title (`dxgi.rs`
//!   `display_size`): 1280x720 at the low tier, so that the title has fewer
//!   pixels to draw if it sizes its picture by it. shortcut: not yet seen
//!   to change what the title renders (it renders 1920x1080 for the Series S
//!   on a 3840x2160 display); a run's log says it: the size of the "depth
//!   texture with unordered access" lines.
//! - Other settings follow the card, the processor or the memory alone:
//!   texture detail (`resource.rs` `detail_side`), the room kept on the card
//!   (`room_percent`, `targets_room_kept`), the threads of ours
//!   (`pipeline/make.rs`, `list/thin.rs`), the processor preset
//!   (`victory_core::cpu`) and what is kept in the PC's memory
//!   (`residency::limited_host_ram`).
//!
//! `VICTORY_TIER=low|standard` chooses the tier.

use std::sync::OnceLock;

/// The tier the layer runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Standard,
    Low,
}

/// What the tier is judged by.
#[derive(Clone, Copy, Debug)]
struct Facts {
    /// The card's memory budget, in bytes (0: not known).
    card_budget: u64,
    /// Logical processors.
    threads: usize,
    /// The PC's memory, in bytes (0: not known).
    memory: u64,
}

const GIB: u64 = 1 << 30;

/// Low below a card budget of 9 GB (an 8 GB card or less: the title needs
/// more than such a card has even with textures at half detail), below 8
/// logical processors, or with 16 GB of memory or less.
fn tier_for(asked: Option<&str>, facts: Facts) -> Tier {
    match asked.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("low") => return Tier::Low,
        Some("standard") => return Tier::Standard,
        _ => {}
    }
    let small_card = facts.card_budget != 0 && facts.card_budget < 9 * GIB;
    let few_threads = facts.threads < 8;
    let little_memory = facts.memory != 0 && facts.memory <= 16 * GIB + GIB / 2;
    if small_card || few_threads || little_memory { Tier::Low } else { Tier::Standard }
}

/// The tier ([`tier_for`] this PC), judged and logged once, with what the PC is.
pub(crate) fn tier() -> Tier {
    static TIER: OnceLock<Tier> = OnceLock::new();
    *TIER.get_or_init(|| {
        let card_budget = crate::host::host().and_then(|h| h.video_memory()).map_or(0, |[budget, ..]| budget);
        let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
        let memory = crate::residency::total_memory().unwrap_or(0);
        let facts = Facts { card_budget, threads, memory };
        let tier = tier_for(std::env::var("VICTORY_TIER").ok().as_deref(), facts);
        tracing::info!(
            threads,
            memory_mb = memory >> 20,
            card_budget_mb = card_budget >> 20,
            ?tier,
            "this PC, and the tier the layer runs at (VICTORY_TIER=low|standard to choose; the processor's preset is said by the line before)"
        );
        tier
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(card_gb: u64, threads: usize, memory_gb: u64) -> Facts {
        Facts { card_budget: card_gb * GIB, threads, memory: memory_gb * GIB }
    }

    #[test]
    fn a_pc_below_the_title_s_needs_runs_at_the_low_tier() {
        // 16 GB card, 16 threads, 32 GB; 12 GB card, 12 threads, 32 GB.
        assert_eq!(tier_for(None, pc(14, 16, 32)), Tier::Standard);
        assert_eq!(tier_for(None, pc(10, 12, 32)), Tier::Standard);
        // 8 GB card; 6 threads; 16 GB of memory.
        assert_eq!(tier_for(None, pc(7, 16, 32)), Tier::Low);
        assert_eq!(tier_for(None, pc(14, 6, 32)), Tier::Low);
        assert_eq!(tier_for(None, pc(14, 16, 16)), Tier::Low);
        // Unknown card and memory: by the threads alone.
        assert_eq!(tier_for(None, Facts { card_budget: 0, threads: 16, memory: 0 }), Tier::Standard);
    }

    #[test]
    fn the_tier_can_be_chosen() {
        assert_eq!(tier_for(Some("low"), pc(14, 16, 32)), Tier::Low);
        assert_eq!(tier_for(Some(" Standard"), pc(7, 4, 8)), Tier::Standard);
        assert_eq!(tier_for(Some("other"), pc(7, 4, 8)), Tier::Low);
    }
}
