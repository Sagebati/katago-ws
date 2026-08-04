//! Where a bare-metal deployment finds its config and data files.
//!
//! Docker bakes absolute paths into `MUXA_ENGINE__*`/`MUXA_CONFIG` env vars, so
//! none of this matters there. A bare-metal `worker` has nothing to go on beyond
//! whatever is next to the binary — this module gives it sane, XDG-conformant
//! defaults instead.

use std::env;
use std::path::{Path, PathBuf};

/// Subdirectory name under each XDG base dir.
const APP_DIR: &str = "katago-ws";

/// Where the resolved `muxa.toml` path came from, and why — logged at startup
/// so "which config file is this process actually using?" has one answer.
pub enum ConfigSource {
    /// `$MUXA_CONFIG` was set; used verbatim even if the file doesn't exist —
    /// a typo'd override path must be visible, not silently ignored.
    Env(PathBuf),
    /// A `muxa.toml` was found in the current directory.
    Cwd(PathBuf),
    /// Found at the XDG config location (`$XDG_CONFIG_HOME/katago-ws/muxa.toml`).
    Xdg(PathBuf),
    /// Nothing found anywhere; falls back to the bare relative name, matching
    /// muxa's own default (all-defaults + `MUXA_*` env vars).
    Default(PathBuf),
}

impl ConfigSource {
    /// The path to load, regardless of which branch produced it.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Env(path) | Self::Cwd(path) | Self::Xdg(path) | Self::Default(path) => path,
        }
    }

    /// Short tag for logging.
    #[must_use]
    pub fn origin(&self) -> &'static str {
        match self {
            Self::Env(_) => "env:MUXA_CONFIG",
            Self::Cwd(_) => "cwd",
            Self::Xdg(_) => "xdg",
            Self::Default(_) => "default",
        }
    }
}

/// `$XDG_CONFIG_HOME/katago-ws` (falls back to `~/.config/katago-ws`).
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_DIR))
}

/// `$XDG_DATA_HOME/katago-ws` (falls back to `~/.local/share/katago-ws`) — where
/// the KataGo model and analysis config default to.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join(APP_DIR))
}

/// `$XDG_STATE_HOME/katago-ws` (falls back to `~/.local/state/katago-ws`, or the
/// data dir on platforms with no state dir) — where the auto-tune cache lives.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join(APP_DIR))
}

/// Resolve the `muxa.toml` path: `$MUXA_CONFIG` > `./muxa.toml` > the XDG config
/// location > the bare default name.
#[must_use]
pub fn resolve_config_file() -> ConfigSource {
    let env_override = env::var_os("MUXA_CONFIG").map(PathBuf::from);
    let cwd_hit = Path::new("muxa.toml").is_file();
    // Only pass the XDG path through if it actually exists — `pick_config`
    // itself does no filesystem access, so existence is checked here.
    let xdg = config_dir()
        .map(|dir| dir.join("muxa.toml"))
        .filter(|path| path.is_file());
    pick_config(env_override, cwd_hit, xdg)
}

/// Default path for a data file (`analysis.cfg`/`model.bin.gz`): a same-named
/// file in the current directory if present (today's behavior, unchanged for
/// anyone with files sitting next to the binary), else the absolute XDG data
/// path (so a "file not found" error names a real directory to populate), else
/// the bare name (no home directory at all — preflight will report it missing).
#[must_use]
pub fn data_file(name: &str) -> String {
    let cwd_hit = Path::new(name).is_file();
    let data = data_dir();
    pick_data_file(name, cwd_hit, data.as_deref())
        .to_string_lossy()
        .into_owned()
}

/// Pure core of [`resolve_config_file`] — no filesystem/env access, so it's
/// trivially testable. `xdg` must already be existence-checked by the caller
/// (`Some` only when that path is a real file), same contract as `cwd_hit`.
fn pick_config(env_override: Option<PathBuf>, cwd_hit: bool, xdg: Option<PathBuf>) -> ConfigSource {
    if let Some(path) = env_override {
        return ConfigSource::Env(path);
    }
    if cwd_hit {
        return ConfigSource::Cwd(PathBuf::from("muxa.toml"));
    }
    if let Some(path) = xdg {
        return ConfigSource::Xdg(path);
    }
    ConfigSource::Default(PathBuf::from("muxa.toml"))
}

/// Pure core of [`data_file`] — no filesystem/env access.
fn pick_data_file(name: &str, cwd_hit: bool, data: Option<&Path>) -> PathBuf {
    if cwd_hit {
        return PathBuf::from(name);
    }
    match data {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins_even_if_the_file_does_not_exist() {
        let source = pick_config(Some(PathBuf::from("/custom/path.toml")), true, None);
        assert_eq!(source.path(), Path::new("/custom/path.toml"));
        assert_eq!(source.origin(), "env:MUXA_CONFIG");
    }

    #[test]
    fn cwd_file_wins_over_xdg() {
        let source = pick_config(
            None,
            true,
            Some(PathBuf::from("/home/x/.config/katago-ws/muxa.toml")),
        );
        assert_eq!(source.path(), Path::new("muxa.toml"));
        assert_eq!(source.origin(), "cwd");
    }

    #[test]
    fn falls_back_to_xdg_when_nothing_local() {
        let xdg = PathBuf::from("/home/x/.config/katago-ws/muxa.toml");
        // Simulate the file existing at that path by using the sentinel the pure
        // function actually receives (an already-existence-checked Option).
        let source = pick_config(None, false, Some(xdg.clone()));
        assert_eq!(source.path(), xdg.as_path());
        assert_eq!(source.origin(), "xdg");
    }

    #[test]
    fn falls_back_to_bare_default_when_nothing_exists() {
        let source = pick_config(None, false, None);
        assert_eq!(source.path(), Path::new("muxa.toml"));
        assert_eq!(source.origin(), "default");
    }

    #[test]
    fn data_file_prefers_cwd_when_present() {
        let path = pick_data_file(
            "model.bin.gz",
            true,
            Some(Path::new("/home/x/.local/share/katago-ws")),
        );
        assert_eq!(path, PathBuf::from("model.bin.gz"));
    }

    #[test]
    fn data_file_falls_back_to_xdg_absolute_path() {
        let path = pick_data_file(
            "model.bin.gz",
            false,
            Some(Path::new("/home/x/.local/share/katago-ws")),
        );
        assert_eq!(
            path,
            PathBuf::from("/home/x/.local/share/katago-ws/model.bin.gz")
        );
    }

    #[test]
    fn data_file_falls_back_to_bare_name_with_no_home() {
        let path = pick_data_file("model.bin.gz", false, None);
        assert_eq!(path, PathBuf::from("model.bin.gz"));
    }
}
