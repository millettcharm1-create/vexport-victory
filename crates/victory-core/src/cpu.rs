//! The PC's processor: whose it is, and from that the preset the layer runs
//! with (`VICTORY_CPU_PRESET=amd|intel` to choose another).
//!
//! The console's processor is AMD's (Zen 2), and the title's code uses
//! instructions only AMD's have: `monitorx` and `mwaitx`, with which its
//! threads wait for a value to change, and `clzero`, `extrq`/`insertq`.
//! On an AMD processor they run as they are. On an Intel one they fault and
//! are emulated (`victory-loader`'s `console_insn.rs`), and a wait that
//! sleeps the core on AMD's became a spin with `pause`: every thread that
//! waits held a core at full use (an Intel PC with 12 threads, 2026-11-27).
//! The Intel preset gives the core away instead at each pass of such a wait
//! (`console_insn.rs` `yields_waits`).

use std::sync::OnceLock;

/// Whose processor the PC has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

/// The vendor string of `cpuid` leaf 0 (12 bytes: EBX, EDX, ECX).
fn vendor_of(text: &[u8; 12]) -> Vendor {
    match text {
        b"AuthenticAMD" | b"HygonGenuine" => Vendor::Amd,
        b"GenuineIntel" => Vendor::Intel,
        _ => Vendor::Other,
    }
}

/// The PC's processor's vendor, from `cpuid`.
pub fn vendor() -> Vendor {
    static VENDOR: OnceLock<Vendor> = OnceLock::new();
    *VENDOR.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `cpuid` leaf 0 exists on every x86-64 processor.
            #[allow(unused_unsafe)]
            let r = unsafe { std::arch::x86_64::__cpuid(0) };
            let mut text = [0u8; 12];
            text[..4].copy_from_slice(&r.ebx.to_le_bytes());
            text[4..8].copy_from_slice(&r.edx.to_le_bytes());
            text[8..].copy_from_slice(&r.ecx.to_le_bytes());
            vendor_of(&text)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Vendor::Other
        }
    })
}

/// The preset the layer runs with for the processor: AMD's (the title's
/// own processor's kind) or Intel's. Another vendor's gets Intel's: it
/// lacks AMD's instructions just as Intel's do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Amd,
    Intel,
}

/// [`Preset`] by `VICTORY_CPU_PRESET` (`amd` or `intel`), else by the
/// processor's [`vendor`].
pub fn preset() -> Preset {
    static PRESET: OnceLock<Preset> = OnceLock::new();
    *PRESET.get_or_init(|| preset_for(std::env::var("VICTORY_CPU_PRESET").ok().as_deref(), vendor()))
}

fn preset_for(asked: Option<&str>, vendor: Vendor) -> Preset {
    match asked.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("amd") => Preset::Amd,
        Some("intel") => Preset::Intel,
        _ if vendor == Vendor::Amd => Preset::Amd,
        _ => Preset::Intel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendor_string_names_the_maker() {
        assert_eq!(vendor_of(b"AuthenticAMD"), Vendor::Amd);
        assert_eq!(vendor_of(b"GenuineIntel"), Vendor::Intel);
        assert_eq!(vendor_of(b"VIA VIA VIA "), Vendor::Other);
    }

    #[test]
    fn the_preset_follows_the_processor_unless_asked() {
        assert_eq!(preset_for(None, Vendor::Amd), Preset::Amd);
        assert_eq!(preset_for(None, Vendor::Intel), Preset::Intel);
        assert_eq!(preset_for(None, Vendor::Other), Preset::Intel);
        assert_eq!(preset_for(Some("Intel"), Vendor::Amd), Preset::Intel);
        assert_eq!(preset_for(Some(" amd "), Vendor::Intel), Preset::Amd);
        assert_eq!(preset_for(Some("other"), Vendor::Amd), Preset::Amd);
    }

    #[test]
    fn this_pc_has_a_vendor() {
        // Whatever it is, asking does not fail.
        let _ = vendor();
    }
}
