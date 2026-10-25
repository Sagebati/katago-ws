//! The auto-tune cache: one JSON file of tuned results, each keyed by
//! everything that changes the answer — the GPU(s) visible, the engine binary,
//! backend, precision, model and config file, `max_visits`, board size, games
//! in flight, and the pinned `override_config` keys. A matching entry skips
//! tuning entirely. Delete the file (or the entry) to retune.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{AutotuneMode, EngineBackend, EngineConfig};

use super::overrides::Overrides;
use super::search::Outcome;

/// Bump when the tuning procedure changes enough to invalidate old results.
/// 4: katago-rs (backend and precision in the key, no OpenCL device probe).
const TUNER_VERSION: u32 = 4;

/// Environment variables that change which devices/runtimes the engine sees.
const DEVICE_ENV: [&str; 7] = [
    "CUDA_VISIBLE_DEVICES",
    "HIP_VISIBLE_DEVICES",
    "ROCR_VISIBLE_DEVICES",
    "GPU_DEVICE_ORDINAL",
    "WGPU_ADAPTER_NAME",
    "WGPU_BACKEND",
    "VK_ICD_FILENAMES",
];

/// Identity of a file: where it resolves to, its size and mtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub path: String,
    pub size: u64,
    pub mtime_ns: u64,
}

impl FileStamp {
    /// Stamp `path`; size and mtime stay zero when it can't be read (so a
    /// missing file still yields a stable key rather than an error).
    #[must_use]
    pub fn of(path: &Path) -> Self {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_err| path.to_path_buf());
        let meta = std::fs::metadata(&resolved).ok();
        Self {
            path: resolved.display().to_string(),
            size: meta.as_ref().map_or(0, std::fs::Metadata::len),
            mtime_ns: meta
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |dur| u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX)),
        }
    }
}

/// Everything a tuned result depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    pub tuner: u32,
    /// `thorough` or `fast`.
    pub mode: String,
    /// GPUs (PCI ids) and device-selection env vars.
    pub gpu: Vec<String>,
    pub binary: FileStamp,
    /// katago-rs `-backend`.
    pub backend: EngineBackend,
    /// Whether the GPU convs run in f16.
    pub fp16: bool,
    pub model: FileStamp,
    pub config: FileStamp,
    pub max_visits: u32,
    pub board_size: u8,
    pub concurrency: usize,
    /// The `override_config` the tuning ran under (its keys were pinned).
    pub pinned: String,
}

impl CacheKey {
    /// The key for `cfg` (with `pinned` keys) on this machine. Cheap: file
    /// stats and sysfs reads.
    #[must_use]
    pub fn build(cfg: &EngineConfig, concurrency: usize, pinned: &Overrides) -> Self {
        Self {
            tuner: TUNER_VERSION,
            mode: mode_name(cfg.autotune_mode).to_owned(),
            gpu: gpu_identity(),
            binary: FileStamp::of(&resolve_binary(&cfg.binary)),
            backend: cfg.backend,
            fp16: cfg.fp16,
            model: FileStamp::of(Path::new(&cfg.model)),
            config: FileStamp::of(Path::new(&cfg.config)),
            max_visits: cfg.max_visits,
            board_size: cfg.default_board_size,
            concurrency,
            pinned: pinned.to_string(),
        }
    }
}

/// One record of a trial, kept in the cache for later inspection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialRecord {
    pub candidate: String,
    /// `measured`, `pruned` or `failed`.
    pub outcome: String,
    pub positions_per_sec: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl TrialRecord {
    #[must_use]
    pub fn new(candidate: &str, outcome: &Outcome) -> Self {
        let (kind, rate, error) = match outcome {
            Outcome::Measured(rate) => ("measured", *rate, String::new()),
            Outcome::Pruned(rate) => ("pruned", *rate, String::new()),
            Outcome::Failed(err) => ("failed", 0.0, err.clone()),
        };
        Self {
            candidate: candidate.to_owned(),
            outcome: kind.to_owned(),
            positions_per_sec: rate,
            error,
        }
    }
}

/// A tuned result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: CacheKey,
    /// The tuned keys only (`key=value,…`), merged under the pinned ones.
    pub tuned: String,
    pub positions_per_sec: f64,
    pub tune_secs: f64,
    pub tuned_at_unix: u64,
    pub trials: Vec<TrialRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    entries: Vec<CacheEntry>,
}

/// `[engine] autotune_cache`, or the default when empty:
/// `~/.katago/katago-ws-autotune.json` — the directory a worker's volume
/// persists — else
/// `/var/cache/katago-ws/autotune.json` when there is no `$HOME`.
#[must_use]
pub fn resolve_path(configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        return PathBuf::from(configured.trim());
    }
    std::env::var_os("HOME")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || PathBuf::from("/var/cache/katago-ws/autotune.json"),
            |home| {
                PathBuf::from(home)
                    .join(".katago")
                    .join("katago-ws-autotune.json")
            },
        )
}

/// The entry for `key`, if the cache file has one. A missing or unreadable
/// file is just a miss. A `fast` lookup also accepts a `thorough` result for
/// the same setup (it searched more, so it's at least as good).
#[must_use]
pub fn lookup(path: &Path, key: &CacheKey) -> Option<CacheEntry> {
    let entries = read(path).entries;
    if let Some(hit) = entries.iter().find(|entry| &entry.key == key) {
        return Some(hit.clone());
    }
    let thorough = mode_name(AutotuneMode::Thorough);
    if key.mode == thorough {
        return None;
    }
    let wider = CacheKey {
        mode: thorough.to_owned(),
        ..key.clone()
    };
    entries.into_iter().find(|entry| entry.key == wider)
}

fn mode_name(mode: AutotuneMode) -> &'static str {
    match mode {
        AutotuneMode::Thorough => "thorough",
        AutotuneMode::Fast => "fast",
    }
}

/// Add or replace the entry for `entry.key`, writing atomically.
pub fn store(path: &Path, entry: CacheEntry) -> Result<(), String> {
    let mut file = read(path);
    file.entries.retain(|old| old.key != entry.key);
    file.entries.push(entry);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    }
    let json = serde_json::to_vec_pretty(&file).map_err(|err| err.to_string())?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, json).map_err(|err| format!("write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|err| format!("rename to {}: {err}", path.display()))
}

#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}

fn read(path: &Path) -> CacheFile {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

/// `binary` as the engine will be launched: a bare name is looked up on `PATH`.
fn resolve_binary(binary: &str) -> PathBuf {
    let path = Path::new(binary);
    if binary.contains('/') {
        return path.to_path_buf();
    }
    std::env::var_os("PATH")
        .and_then(|dirs| {
            std::env::split_paths(&dirs)
                .map(|dir| dir.join(binary))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// The visible GPUs, as stable strings: DRM cards' PCI vendor/device/revision,
/// the NVIDIA driver version, and device-selection environment variables.
fn gpu_identity() -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(cards) = std::fs::read_dir("/sys/class/drm") {
        for card in cards.flatten() {
            let name = card.file_name().to_string_lossy().into_owned();
            let is_card = name.strip_prefix("card").is_some_and(|num| {
                !num.is_empty() && num.bytes().all(|byte| byte.is_ascii_digit())
            });
            if !is_card {
                continue;
            }
            let attr = |file: &str| {
                std::fs::read_to_string(card.path().join("device").join(file))
                    .map(|val| val.trim().to_owned())
                    .unwrap_or_default()
            };
            ids.push(format!(
                "pci {}:{} rev {}",
                attr("vendor"),
                attr("device"),
                attr("revision")
            ));
        }
    }
    if let Ok(version) = std::fs::read_to_string("/proc/driver/nvidia/version")
        && let Some(first) = version.lines().next()
    {
        ids.push(format!("nvidia {}", first.trim()));
    }
    for var in DEVICE_ENV {
        if let Some(val) = std::env::var_os(var) {
            ids.push(format!("env {var}={}", val.to_string_lossy()));
        }
    }
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model_size: u64) -> CacheKey {
        CacheKey {
            tuner: TUNER_VERSION,
            mode: "thorough".to_owned(),
            gpu: vec!["pci 0x1002:0x73bf rev 0xc1".to_owned()],
            binary: FileStamp {
                path: "/opt/katago-rs/katago-rs".to_owned(),
                size: 7,
                mtime_ns: 1,
            },
            backend: EngineBackend::Rocm,
            fp16: true,
            model: FileStamp {
                path: "/opt/katago-rs/nets/g170e-b10c128.bpk".to_owned(),
                size: model_size,
                mtime_ns: 1,
            },
            config: FileStamp {
                path: "/opt/katago-rs/analysis.cfg".to_owned(),
                size: 3,
                mtime_ns: 1,
            },
            max_visits: 100,
            board_size: 19,
            concurrency: 2,
            pinned: String::new(),
        }
    }

    fn entry(key: CacheKey, tuned: &str) -> CacheEntry {
        CacheEntry {
            key,
            tuned: tuned.to_owned(),
            positions_per_sec: 160.0,
            tune_secs: 50.0,
            tuned_at_unix: 0,
            trials: vec![TrialRecord::new("x", &Outcome::Pruned(1.0))],
        }
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("katago-ws-autotune-test-{}", std::process::id()))
            .join(name)
    }

    #[test]
    fn store_then_lookup_by_exact_key() {
        let path = scratch("store.json");
        let _ = std::fs::remove_file(&path);
        assert!(lookup(&path, &key(1)).is_none(), "a missing file is a miss");

        store(&path, entry(key(1), "nnMaxBatchSize=32")).unwrap();
        store(&path, entry(key(2), "nnMaxBatchSize=64")).unwrap();
        assert_eq!(lookup(&path, &key(1)).unwrap().tuned, "nnMaxBatchSize=32");
        assert_eq!(lookup(&path, &key(2)).unwrap().tuned, "nnMaxBatchSize=64");
        assert!(lookup(&path, &key(3)).is_none(), "another model is a miss");

        // Re-storing a key replaces its entry instead of adding one.
        store(&path, entry(key(1), "nnMaxBatchSize=48")).unwrap();
        assert_eq!(lookup(&path, &key(1)).unwrap().tuned, "nnMaxBatchSize=48");
        assert_eq!(read(&path).entries.len(), 2);
    }

    #[test]
    fn fast_mode_reuses_a_thorough_result_but_not_the_reverse() {
        let path = scratch("modes.json");
        let _ = std::fs::remove_file(&path);
        store(&path, entry(key(1), "thorough-result")).unwrap();
        let fast = CacheKey {
            mode: "fast".to_owned(),
            ..key(1)
        };
        assert_eq!(lookup(&path, &fast).unwrap().tuned, "thorough-result");

        let _ = std::fs::remove_file(&path);
        store(&path, entry(fast, "fast-result")).unwrap();
        assert!(
            lookup(&path, &key(1)).is_none(),
            "thorough retunes over a fast result"
        );
    }

    #[test]
    fn any_relevant_change_misses() {
        let base = key(1);
        let mut visits = base.clone();
        visits.max_visits = 200;
        let mut pinned = base.clone();
        pinned.pinned = "numAnalysisThreads=64".to_owned();
        let mut gpu = base.clone();
        gpu.gpu.push("env HIP_VISIBLE_DEVICES=1".to_owned());
        let mut fp16 = base.clone();
        fp16.fp16 = !base.fp16;
        let mut backend = base.clone();
        backend.backend = EngineBackend::Wgpu;
        let mut binary = base.clone();
        binary.binary.mtime_ns = 2;
        for other in [visits, pinned, gpu, binary, fp16, backend] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn a_corrupt_file_is_a_miss_and_gets_replaced() {
        let path = scratch("corrupt.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(lookup(&path, &key(1)).is_none());
        store(&path, entry(key(1), "a=1")).unwrap();
        assert_eq!(lookup(&path, &key(1)).unwrap().tuned, "a=1");
    }

    #[test]
    fn configured_path_wins_over_the_default() {
        assert_eq!(
            resolve_path("/data/tune.json"),
            PathBuf::from("/data/tune.json")
        );
        let default = resolve_path("");
        assert!(
            default.ends_with(".katago/katago-ws-autotune.json")
                || default.ends_with("katago-ws/autotune.json")
        );
    }

    #[test]
    fn file_stamps_identify_real_files() {
        let path = scratch("stamp.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"abc").unwrap();
        let stamp = FileStamp::of(&path);
        assert_eq!(stamp.size, 3);
        assert!(stamp.mtime_ns > 0);
        let missing = FileStamp::of(Path::new("/nonexistent/katago"));
        assert_eq!(missing.size, 0);
    }
}
