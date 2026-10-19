//! What the host offers: CPU cores and the GPU's SIMD lane count. They don't
//! decide the best server-thread count (that follows the KataGo build — see
//! `search`), but they size the search: lanes set how many positions the GPU
//! needs in flight, cores cap how many NN server threads are worth trying.

use std::path::Path;

/// KFD (amdgpu compute) topology: one directory per CPU/GPU node.
const KFD_NODES: &str = "/sys/class/kfd/kfd/topology/nodes";

/// The host's relevant capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hardware {
    /// Logical CPUs this process may use (cgroup/affinity aware).
    pub cpus: usize,
    /// SIMD lanes of the biggest GPU, when the platform reports it.
    pub gpu_lanes: Option<u32>,
}

impl Hardware {
    /// Probe the host. Cheap: sysfs reads, plus `clinfo` only when sysfs has
    /// no answer (non-AMD GPUs) and it is installed.
    pub async fn probe() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let gpu_lanes = match kfd_lanes(Path::new(KFD_NODES)) {
            Some(lanes) => Some(lanes),
            None => clinfo_lanes().await,
        };
        Self { cpus, gpu_lanes }
    }
}

/// Lanes of the biggest AMD GPU from the KFD topology: compute units (SIMDs ÷
/// SIMDs per CU) × 64 — both GCN (4×SIMD16) and RDNA (2×SIMD32) have 64 lanes
/// per CU.
fn kfd_lanes(nodes: &Path) -> Option<u32> {
    std::fs::read_dir(nodes)
        .ok()?
        .flatten()
        .filter_map(|node| std::fs::read_to_string(node.path().join("properties")).ok())
        .filter_map(|props| parse_kfd_properties(&props))
        .max()
}

/// `simd_count` / `simd_per_cu` × 64 from one node's `properties`; `None` for
/// CPU nodes (no SIMDs).
#[must_use]
pub fn parse_kfd_properties(props: &str) -> Option<u32> {
    let field = |name: &str| -> Option<u32> {
        props.lines().find_map(|line| {
            let (key, val) = line.split_once(' ')?;
            (key == name).then(|| val.trim().parse().ok())?
        })
    };
    let simds = field("simd_count").filter(|&count| count > 0)?;
    let per_cu = field("simd_per_cu").filter(|&count| count > 0)?;
    Some(simds / per_cu * 64)
}

async fn clinfo_lanes() -> Option<u32> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("clinfo")
            .arg("--raw")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_clinfo_lanes(&String::from_utf8_lossy(&output.stdout))
}

/// Lanes of the biggest GPU in `clinfo --raw` output: compute units × lanes
/// per unit for the vendor (NVIDIA SM: 128, Intel EU: 8, otherwise 64). AMD's
/// ROCm runtime counts RDNA work-group processors (2 CUs) as one unit, so the
/// largest report across platforms is taken.
#[must_use]
pub fn parse_clinfo_lanes(raw: &str) -> Option<u32> {
    // Fields of one device, keyed by the `[PLATFORM/N]` tag.
    let mut devices: Vec<(String, Option<u32>, Option<String>, bool)> = Vec::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        let (Some(tag), Some(key)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !tag.starts_with('[') {
            continue;
        }
        let value = parts.collect::<Vec<_>>().join(" ");
        let idx = match devices.iter().position(|(have, ..)| have == tag) {
            Some(idx) => idx,
            None => {
                devices.push((tag.to_owned(), None, None, false));
                devices.len() - 1
            }
        };
        let Some(dev) = devices.get_mut(idx) else {
            continue;
        };
        match key {
            "CL_DEVICE_MAX_COMPUTE_UNITS" => dev.1 = value.parse().ok(),
            "CL_DEVICE_VENDOR_ID" => dev.2 = Some(value),
            "CL_DEVICE_TYPE" => dev.3 = value.contains("GPU"),
            _ => {}
        }
    }
    devices
        .iter()
        .filter(|(.., gpu)| *gpu)
        .filter_map(|(_, units, vendor, _)| {
            let per_unit = match vendor.as_deref() {
                Some("0x10de") => 128,
                Some("0x8086") => 8,
                _ => 64,
            };
            units.map(|units| units * per_unit)
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kfd_properties_give_lanes_for_gpus_only() {
        let gpu = "cpu_cores_count 0\nsimd_count 144\nsimd_per_cu 2\nwave_front_size 32\n";
        assert_eq!(parse_kfd_properties(gpu), Some(72 * 64));
        let cpu = "cpu_cores_count 12\nsimd_count 0\nsimd_per_cu 0\n";
        assert_eq!(parse_kfd_properties(cpu), None);
    }

    #[test]
    fn clinfo_takes_the_largest_gpu_report() {
        // RX 6800 XT through ROCm (36 WGPs) and Mesa rusticl (72 CUs).
        let raw = "\
[AMD/0]     CL_DEVICE_NAME                                  gfx1030
[AMD/0]     CL_DEVICE_VENDOR_ID                             0x1002
[AMD/0]     CL_DEVICE_TYPE                                  CL_DEVICE_TYPE_GPU
[AMD/0]     CL_DEVICE_MAX_COMPUTE_UNITS                     36
[MESA/0]    CL_DEVICE_VENDOR_ID                             0x1002
[MESA/0]    CL_DEVICE_TYPE                                  CL_DEVICE_TYPE_GPU
[MESA/0]    CL_DEVICE_MAX_COMPUTE_UNITS                     72
[POCL/0]    CL_DEVICE_TYPE                                  CL_DEVICE_TYPE_CPU
[POCL/0]    CL_DEVICE_MAX_COMPUTE_UNITS                     512";
        assert_eq!(parse_clinfo_lanes(raw), Some(72 * 64));
        let nvidia = "\
[NV/0]  CL_DEVICE_VENDOR_ID  0x10de
[NV/0]  CL_DEVICE_TYPE  CL_DEVICE_TYPE_GPU
[NV/0]  CL_DEVICE_MAX_COMPUTE_UNITS  128";
        assert_eq!(parse_clinfo_lanes(nvidia), Some(128 * 128));
        assert_eq!(parse_clinfo_lanes(""), None);
    }
}
