//! OpenCL device detection from KataGo's own startup log, and the choice of
//! device to pin.
//!
//! KataGo numbers devices across all platforms in enumeration order and logs:
//!
//! ```text
//! Found OpenCL Platform 0: AMD Accelerated Parallel Processing (Advanced Micro Devices, Inc.) (OpenCL 2.1 AMD-APP (3581.0))
//! Found 1 device(s) on platform 0 with type CPU or GPU or Accelerator
//! Found OpenCL Platform 1: rusticl (Mesa/X.org) (OpenCL 3.1 )
//! Found 1 device(s) on platform 1 with type CPU or GPU or Accelerator
//! Found OpenCL Device 0: gfx1030 (Advanced Micro Devices, Inc.) (score 11000200)
//! Found OpenCL Device 1: AMD Radeon RX 6800 XT (radeonsi, navi21, ACO, DRM 3.64, 7.2.6-arch2-1) (AMD) (score 11000301)
//! Creating context for OpenCL Platform: AMD Accelerated Parallel Processing ...
//! Using OpenCL Device 1: AMD Radeon RX 6800 XT ...
//! ```
//!
//! Its default pick is the highest score, which can be Mesa's rusticl even
//! when AMD's own ROCm runtime exposes the same GPU (~1.6× faster).

/// One OpenCL device as KataGo numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenClDevice {
    /// KataGo's global device index (what `openclDeviceToUseThreadN` takes).
    pub index: usize,
    pub name: String,
    /// Name of the platform (ICD) exposing it.
    pub platform: String,
    pub score: Option<i64>,
}

/// What a KataGo startup log says about OpenCL devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub devices: Vec<OpenClDevice>,
    /// The device KataGo went on to use (its own choice, or the config's).
    pub using: Option<usize>,
}

/// Platforms of GPU vendors' own OpenCL runtimes — preferred over Mesa's.
const VENDOR_PLATFORMS: [&str; 4] = [
    "AMD Accelerated Parallel Processing",
    "NVIDIA CUDA",
    "Intel(R) OpenCL Graphics",
    "Intel(R) OpenCL HD Graphics",
];

/// Mesa's OpenCL platforms (slower than a vendor runtime on the same GPU).
const MESA_PLATFORMS: [&str; 3] = ["rusticl", "Clover", "Mesa"];

/// Whether this log line is the last one the device probe needs.
#[must_use]
pub fn ends_device_listing(line: &str) -> bool {
    let msg = message(line);
    msg.starts_with("Using OpenCL Device") || msg.starts_with("Started, ready")
}

/// Parse the device listing out of KataGo's log lines.
#[must_use]
pub fn parse<'line>(lines: impl IntoIterator<Item = &'line str>) -> Listing {
    // (platform index, name) and per-platform device counts, in log order.
    let mut platforms: Vec<(usize, String)> = Vec::new();
    let mut counts: Vec<(usize, usize)> = Vec::new();
    let mut found: Vec<(usize, String, Option<i64>)> = Vec::new();
    let mut using = None;

    for line in lines {
        let msg = message(line);
        if let Some(rest) = msg.strip_prefix("Found OpenCL Platform ")
            && let Some((idx, name)) = rest.split_once(':')
            && let Ok(idx) = idx.trim().parse()
        {
            platforms.push((idx, name.trim().to_owned()));
        } else if let Some(rest) = msg.strip_prefix("Found ")
            && let Some((count, tail)) = rest.split_once(" device(s) on platform ")
            && let Ok(count) = count.trim().parse()
            && let Some(idx) = tail
                .split_whitespace()
                .next()
                .and_then(|idx| idx.parse().ok())
        {
            counts.push((idx, count));
        } else if let Some(rest) = msg.strip_prefix("Found OpenCL Device ")
            && let Some((idx, desc)) = rest.split_once(':')
            && let Ok(idx) = idx.trim().parse()
        {
            let (name, score) = split_score(desc.trim());
            found.push((idx, name, score));
        } else if let Some(rest) = msg.strip_prefix("Using OpenCL Device ")
            && using.is_none()
        {
            using = rest
                .split(':')
                .next()
                .and_then(|idx| idx.trim().parse().ok());
        }
    }

    // Device indices run across platforms in platform order.
    counts.sort_unstable();
    let mut owner = Vec::new();
    for (platform, count) in counts {
        owner.extend(std::iter::repeat_n(platform, count));
    }
    let devices = found
        .into_iter()
        .map(|(index, name, score)| {
            let platform = owner
                .get(index)
                .and_then(|pidx| platforms.iter().find(|(idx, _)| idx == pidx))
                .map(|(_, name)| name.clone())
                .unwrap_or_default();
            OpenClDevice {
                index,
                name,
                platform,
                score,
            }
        })
        .collect();
    Listing { devices, using }
}

/// The device to pin on every server thread, or `None` to leave KataGo's
/// choice alone (only one device, or nothing better to switch to).
///
/// KataGo's pick (the config's, else highest score) is kept if it is on a
/// vendor runtime; if it is on Mesa and a vendor runtime exposes a device, the
/// best-scored vendor device is used instead. A kept pick is still pinned on
/// every thread so that server threads the config file doesn't name can't
/// drift onto another device.
#[must_use]
pub fn choose(listing: &Listing) -> Option<usize> {
    if listing.devices.len() < 2 {
        return None;
    }
    let current = listing
        .using
        .and_then(|idx| listing.devices.iter().find(|dev| dev.index == idx))
        .or_else(|| listing.devices.iter().max_by_key(|dev| dev.score))?;
    if is_vendor(&current.platform) {
        return Some(current.index);
    }
    let vendor = listing
        .devices
        .iter()
        .filter(|dev| is_vendor(&dev.platform))
        .max_by_key(|dev| dev.score);
    match vendor {
        Some(dev) if is_mesa(&current.platform) => Some(dev.index),
        _ => Some(current.index),
    }
}

fn is_vendor(platform: &str) -> bool {
    VENDOR_PLATFORMS
        .iter()
        .any(|name| platform.starts_with(name))
}

fn is_mesa(platform: &str) -> bool {
    MESA_PLATFORMS.iter().any(|name| platform.contains(name))
}

/// A log line without KataGo's `YYYY-MM-DD hh:mm:ss+zzzz: ` prefix.
fn message(line: &str) -> &str {
    let line = line.trim();
    // The timestamp's own colons come before the `: ` separator.
    match line.find(": ") {
        Some(pos) if line.starts_with(|ch: char| ch.is_ascii_digit()) => {
            line.get(pos + 2..).unwrap_or(line)
        }
        _ => line,
    }
}

/// `"gfx1030 (Advanced Micro Devices, Inc.) (score 11000200)"` →
/// (`"gfx1030 (Advanced Micro Devices, Inc.)"`, `Some(11000200)`).
fn split_score(desc: &str) -> (String, Option<i64>) {
    if let Some(pos) = desc.rfind("(score ") {
        let score = desc
            .get(pos + 7..)
            .and_then(|rest| rest.trim_end_matches(')').trim().parse().ok());
        let name = desc.get(..pos).unwrap_or(desc).trim().to_owned();
        return (name, score);
    }
    (desc.to_owned(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from KataGo v1.16.5 on the RX 6800 XT host (ROCm + rusticl).
    const ROCM_AND_RUSTICL: &str = "\
2026-10-15 00:06:31+0000: Found OpenCL Platform 0: AMD Accelerated Parallel Processing (Advanced Micro Devices, Inc.) (OpenCL 2.1 AMD-APP (3581.0))
2026-10-15 00:06:31+0000: Found 1 device(s) on platform 0 with type CPU or GPU or Accelerator
2026-10-15 00:06:31+0000: Found OpenCL Platform 1: rusticl (Mesa/X.org) (OpenCL 3.1 )
2026-10-15 00:06:31+0000: Found 1 device(s) on platform 1 with type CPU or GPU or Accelerator
2026-10-15 00:06:31+0000: Found OpenCL Device 0: gfx1030 (Advanced Micro Devices, Inc.) (score 11000200)
2026-10-15 00:06:31+0000: Found OpenCL Device 1: AMD Radeon RX 6800 XT (radeonsi, navi21, ACO, DRM 3.64, 7.2.6-arch2-1) (AMD) (score 11000301)
2026-10-15 00:06:31+0000: Creating context for OpenCL Platform: rusticl (Mesa/X.org) (OpenCL 3.1 )
2026-10-15 00:06:31+0000: Using OpenCL Device 1: AMD Radeon RX 6800 XT (radeonsi, navi21, ACO, DRM 3.64, 7.2.6-arch2-1) (AMD) OpenCL 3.0  (Extensions: cl_khr_fp16)";

    #[test]
    fn parses_devices_with_their_platforms() {
        let listing = parse(ROCM_AND_RUSTICL.lines());
        assert_eq!(listing.using, Some(1));
        assert_eq!(listing.devices.len(), 2);
        let rocm = &listing.devices[0];
        assert_eq!(rocm.index, 0);
        assert_eq!(rocm.name, "gfx1030 (Advanced Micro Devices, Inc.)");
        assert!(
            rocm.platform
                .starts_with("AMD Accelerated Parallel Processing")
        );
        assert_eq!(rocm.score, Some(11_000_200));
        let mesa = &listing.devices[1];
        assert!(mesa.platform.starts_with("rusticl"));
        assert_eq!(mesa.score, Some(11_000_301));
    }

    #[test]
    fn prefers_rocm_over_rusticl() {
        let listing = parse(ROCM_AND_RUSTICL.lines());
        assert_eq!(choose(&listing), Some(0));
        // Same result when the log was cut before the "Using" line.
        let cut: Vec<&str> = ROCM_AND_RUSTICL.lines().take(6).collect();
        let listing = parse(cut);
        assert_eq!(listing.using, None);
        assert_eq!(choose(&listing), Some(0));
    }

    #[test]
    fn keeps_a_vendor_pick_and_pins_it() {
        let mut listing = parse(ROCM_AND_RUSTICL.lines());
        listing.using = Some(0);
        assert_eq!(choose(&listing), Some(0));
    }

    #[test]
    fn a_single_device_is_left_alone() {
        let lines = "\
Found OpenCL Platform 0: rusticl (Mesa/X.org) (OpenCL 3.1 )
Found 1 device(s) on platform 0 with type CPU or GPU or Accelerator
Found OpenCL Device 0: AMD Radeon RX 6800 XT (radeonsi) (AMD) (score 11000301)
Using OpenCL Device 0: AMD Radeon RX 6800 XT";
        let listing = parse(lines.lines());
        assert_eq!(listing.devices.len(), 1);
        assert_eq!(choose(&listing), None);
    }

    #[test]
    fn mesa_only_keeps_katagos_choice() {
        let lines = "\
Found OpenCL Platform 0: Clover (Mesa) (OpenCL 1.1)
Found 1 device(s) on platform 0 with type CPU or GPU or Accelerator
Found OpenCL Platform 1: Portable Computing Language (OpenCL 3.0 PoCL)
Found 1 device(s) on platform 1 with type CPU or GPU or Accelerator
Found OpenCL Device 0: AMD Radeon (score 1000)
Found OpenCL Device 1: cpu-haswell (score 10)";
        let listing = parse(lines.lines());
        assert_eq!(
            listing.devices[1].platform,
            "Portable Computing Language (OpenCL 3.0 PoCL)"
        );
        assert_eq!(choose(&listing), Some(0));
    }

    #[test]
    fn the_probe_stops_at_the_using_line() {
        assert!(ends_device_listing(
            "2026-10-15 00:06:31+0000: Using OpenCL Device 0: gfx1030"
        ));
        assert!(ends_device_listing(
            "Started, ready to begin handling requests"
        ));
        assert!(!ends_device_listing(
            "2026-10-15 00:06:31+0000: Found OpenCL Device 0: gfx1030 (score 1)"
        ));
    }
}
