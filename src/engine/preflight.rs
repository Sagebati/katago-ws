//! Preflight checks for the KataGo subprocess: resolve and validate the
//! binary/config/model *before* spawning, so a misconfiguration produces one
//! precise, actionable error at startup instead of a silent "engine ready"
//! followed by a cryptic failure on the first analysis request.

use std::path::{Path, PathBuf};

use crate::config::EngineConfig;

/// Resolved, validated launch parameters for `katago analysis`.
pub struct EngineLaunch {
    /// Absolute path to the `katago` executable.
    pub binary: PathBuf,
    /// Absolute path to the analysis config file.
    pub config: PathBuf,
    /// Absolute path to the model file.
    pub model: PathBuf,
    /// `-override-config key=value` pairs, filled in by the auto-tune step
    /// (empty when auto-tune is off or produced nothing usable).
    pub overrides: Vec<(String, String)>,
}

/// A preflight check failed. Every variant's message names the exact path
/// tried, the relevant `[engine].*` config key / `MUXA_ENGINE__*` env var, and
/// a copy-pasteable fix.
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error(
        "katago binary '{name}' was not found on $PATH.\n  \
         Install KataGo — https://github.com/lightvector/KataGo/releases — and put the \
         'katago' executable on your PATH,\n  \
         or set MUXA_ENGINE__BINARY=/full/path/to/katago ([engine].binary)."
    )]
    BinaryNotOnPath { name: String },

    #[error(
        "katago binary not found at {path} ([engine].binary / MUXA_ENGINE__BINARY).\n  \
         Check the path, or unset it to search $PATH instead."
    )]
    BinaryNotFound { path: PathBuf },

    #[error("katago binary at {path} is not executable.\n  Fix with: chmod +x {path}")]
    BinaryNotExecutable { path: PathBuf },

    #[error(
        "KataGo analysis config not found: {path}\n  \
         Fetch the stock one (matching your installed KataGo version):\n    \
         mkdir -p $(dirname {path}) && curl -fL -o {path} \
         https://raw.githubusercontent.com/lightvector/KataGo/master/cpp/configs/analysis_example.cfg\n  \
         Or point [engine].config / MUXA_ENGINE__CONFIG at an existing file."
    )]
    ConfigMissing { path: PathBuf },

    #[error("KataGo analysis config at {path} could not be read: {source}")]
    ConfigUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "KataGo model file not found: {path}\n  \
         Download a network:\n    \
         mkdir -p $(dirname {path}) && curl -fL -o {path} \
         https://github.com/lightvector/KataGo/releases/download/v1.4.5/g170e-b20c256x2-s5303129600-d1228401921.bin.gz\n  \
         (that's the network the official images bundle — see the Dockerfile's MODEL_URL)\n  \
         Or point [engine].model / MUXA_ENGINE__MODEL at an existing file."
    )]
    ModelMissing { path: PathBuf },

    #[error("KataGo model file at {path} could not be read: {source}")]
    ModelUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "KataGo model file {path} is empty (0 bytes) — an interrupted download?\n  Delete it and re-download."
    )]
    ModelEmpty { path: PathBuf },
}

/// Resolve and validate `cfg.binary`/`cfg.config`/`cfg.model`, producing a
/// ready-to-spawn [`EngineLaunch`] or a precise [`PreflightError`].
pub fn check(cfg: &EngineConfig) -> Result<EngineLaunch, PreflightError> {
    let binary = resolve_binary(&cfg.binary)?;
    let config = PathBuf::from(&cfg.config);
    check_config_file(&config)?;
    let model = PathBuf::from(&cfg.model);
    check_model_file(&model)?;
    Ok(EngineLaunch {
        binary,
        config,
        model,
        overrides: Vec::new(),
    })
}

/// Resolve `spec` to an absolute, executable binary path: treated as a literal
/// path if it contains a `/`, otherwise searched on `$PATH`.
fn resolve_binary(spec: &str) -> Result<PathBuf, PreflightError> {
    if spec.contains('/') {
        let path = PathBuf::from(spec);
        if !path.is_file() {
            return Err(PreflightError::BinaryNotFound { path });
        }
        if !is_executable(&path) {
            return Err(PreflightError::BinaryNotExecutable { path });
        }
        return Ok(path);
    }

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let path_var = path_var.to_string_lossy();
    match find_on_path(spec, &path_var, &is_executable) {
        Some(path) => Ok(path),
        None => Err(PreflightError::BinaryNotOnPath {
            name: spec.to_owned(),
        }),
    }
}

/// Search `$PATH` (colon-separated `path_var`) for an executable file named
/// `name`. `probe` decides both existence and executability — injected so
/// this stays testable without touching the real filesystem (in production
/// it's [`is_executable`], which already returns `false` for a nonexistent
/// path since `std::fs::metadata` fails on it).
fn find_on_path(name: &str, path_var: &str, probe: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| probe(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn check_config_file(path: &Path) -> Result<(), PreflightError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            std::fs::File::open(path).map_err(|source| PreflightError::ConfigUnreadable {
                path: path.to_owned(),
                source,
            })?;
            Ok(())
        }
        _ => Err(PreflightError::ConfigMissing {
            path: path.to_owned(),
        }),
    }
}

fn check_model_file(path: &Path) -> Result<(), PreflightError> {
    let meta = std::fs::metadata(path).map_err(|_err| PreflightError::ModelMissing {
        path: path.to_owned(),
    })?;
    if !meta.is_file() {
        return Err(PreflightError::ModelMissing {
            path: path.to_owned(),
        });
    }
    std::fs::File::open(path).map_err(|source| PreflightError::ModelUnreadable {
        path: path.to_owned(),
        source,
    })?;
    if meta.len() == 0 {
        return Err(PreflightError::ModelEmpty {
            path: path.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_returns_the_first_match() {
        let found = find_on_path("katago", "/usr/local/bin:/usr/bin:/bin", &|path| {
            path == Path::new("/usr/bin/katago")
        });
        assert_eq!(found, Some(PathBuf::from("/usr/bin/katago")));
    }

    #[test]
    fn find_on_path_returns_none_when_absent() {
        let found = find_on_path("katago", "/usr/local/bin:/usr/bin", &|_| false);
        assert_eq!(found, None);
    }

    #[test]
    fn find_on_path_handles_an_empty_path_var() {
        // `std::env::split_paths("")` yields one empty component, so this
        // isn't "no entries" — it's "probe every candidate and find none".
        let found = find_on_path("katago", "", &|_| false);
        assert_eq!(found, None);
    }

    #[test]
    fn binary_not_on_path_names_the_binary_and_the_env_var() {
        let err = PreflightError::BinaryNotOnPath {
            name: "katago".to_owned(),
        };
        let text = err.to_string();
        assert!(text.contains("katago"));
        assert!(text.contains("MUXA_ENGINE__BINARY"));
    }

    #[test]
    fn binary_not_executable_suggests_chmod() {
        let err = PreflightError::BinaryNotExecutable {
            path: PathBuf::from("/opt/katago/katago"),
        };
        let text = err.to_string();
        assert!(text.contains("/opt/katago/katago"));
        assert!(text.contains("chmod +x"));
    }

    #[test]
    fn config_missing_names_the_path_and_env_var() {
        let err = PreflightError::ConfigMissing {
            path: PathBuf::from("/data/analysis.cfg"),
        };
        let text = err.to_string();
        assert!(text.contains("/data/analysis.cfg"));
        assert!(text.contains("MUXA_ENGINE__CONFIG"));
        assert!(text.contains("curl"));
    }

    #[test]
    fn model_missing_names_the_path_and_env_var() {
        let err = PreflightError::ModelMissing {
            path: PathBuf::from("/data/model.bin.gz"),
        };
        let text = err.to_string();
        assert!(text.contains("/data/model.bin.gz"));
        assert!(text.contains("MUXA_ENGINE__MODEL"));
    }

    #[test]
    fn model_empty_suggests_redownload() {
        let err = PreflightError::ModelEmpty {
            path: PathBuf::from("/data/model.bin.gz"),
        };
        assert!(err.to_string().contains("re-download"));
    }
}
