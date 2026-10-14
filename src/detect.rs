//! Find the project root above a directory and decide how to check it.
//!
//! Detection only reads the filesystem; it never runs anything.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Per-project override file. It wins over every built-in toolchain.
pub const OVERRIDE_FILE: &str = ".baa.toml";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How to read an error count out of a failed check's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Counter {
    Cargo,
    Tsc,
    /// Count lines starting with this text, ignoring leading whitespace.
    Prefix(String),
    /// No count: the exit code alone decides.
    ExitCode,
}

/// A ready-to-run check for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub root: PathBuf,
    pub command: Vec<String>,
    pub timeout: Duration,
    pub counter: Counter,
}

/// A built-in toolchain: the marker file that identifies it and how to check it.
struct Toolchain {
    marker: &'static str,
    /// Builds the command, or `None` when the tool isn't available for this project.
    command: fn(&Path) -> Option<Vec<String>>,
    counter: Counter,
}

/// Checked in order; supporting a new toolchain means adding a row here and a counter.
const TOOLCHAINS: &[Toolchain] = &[
    Toolchain {
        marker: "Cargo.toml",
        command: cargo,
        counter: Counter::Cargo,
    },
    Toolchain {
        marker: "tsconfig.json",
        command: tsc,
        counter: Counter::Tsc,
    },
];

fn cargo(_root: &Path) -> Option<Vec<String>> {
    Some(strings(&["cargo", "check", "--message-format=short"]))
}

/// Uses the project's own `tsc` (as Node would resolve it) and never installs one.
fn tsc(root: &Path) -> Option<Vec<String>> {
    let tsc = root
        .ancestors()
        .map(|dir| dir.join("node_modules/.bin/tsc"))
        .find(|path| path.is_file())?;
    let mut command = vec![tsc.to_string_lossy().into_owned()];
    command.extend(strings(&["--noEmit", "--pretty", "false"]));
    Some(command)
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// The contents of `.baa.toml`. Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Override {
    /// Replaces the built-in command, e.g. `["just", "check"]`.
    command: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    /// Counts output lines starting with this text as errors.
    count_prefix: Option<String>,
}

/// The nearest directory, from `start` upwards, that holds an override or a toolchain marker.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            dir.join(OVERRIDE_FILE).is_file()
                || TOOLCHAINS.iter().any(|t| dir.join(t.marker).is_file())
        })
        .map(Path::to_path_buf)
}

/// Works out how to check the project that contains `start`.
///
/// `Ok(None)` means there is nothing baa knows how to check, so it stays silent.
/// `Err` means `.baa.toml` exists but is invalid.
pub fn detect(start: &Path) -> Result<Option<Check>, String> {
    let Some(root) = find_root(start) else {
        return Ok(None);
    };
    let config = read_override(&root).map_err(|e| format!("{OVERRIDE_FILE}: {e}"))?;
    let timeout = match config.timeout_secs {
        None => DEFAULT_TIMEOUT,
        Some(0) => return Err(format!("{OVERRIDE_FILE}: timeout_secs must be at least 1")),
        Some(secs) => Duration::from_secs(secs),
    };

    let builtin = TOOLCHAINS
        .iter()
        .filter(|t| root.join(t.marker).is_file())
        .find_map(|t| Some(((t.command)(&root)?, t.counter.clone())));
    let (command, default_counter) = match (config.command, builtin) {
        (Some(command), _) => (command, Counter::ExitCode),
        (None, Some(found)) => found,
        (None, None) => return Ok(None),
    };
    if command.is_empty() {
        return Err(format!("{OVERRIDE_FILE}: command must not be empty"));
    }
    let counter = config.count_prefix.map_or(default_counter, Counter::Prefix);

    Ok(Some(Check {
        root,
        command,
        timeout,
        counter,
    }))
}

fn read_override(root: &Path) -> Result<Override, String> {
    match fs::read_to_string(root.join(OVERRIDE_FILE)) {
        Ok(text) => toml::from_str(&text).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Override::default()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Creates `files` (relative path, contents) inside a fresh temporary directory.
    fn project(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        dir
    }

    #[test]
    fn unknown_project_is_silent() {
        let dir = project(&[("README.md", "")]);
        assert_eq!(find_root(dir.path()), None);
        assert_eq!(detect(dir.path()), Ok(None));
    }

    #[test]
    fn cargo_project_is_found_from_a_subdirectory() {
        let dir = project(&[("Cargo.toml", ""), ("src/bin/main.rs", "")]);
        let check = detect(&dir.path().join("src/bin")).unwrap().unwrap();
        assert_eq!(check.root, dir.path());
        assert_eq!(check.command, ["cargo", "check", "--message-format=short"]);
        assert_eq!(check.counter, Counter::Cargo);
        assert_eq!(check.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn nearest_root_wins() {
        let dir = project(&[("tsconfig.json", ""), ("crates/core/Cargo.toml", "")]);
        let inner = dir.path().join("crates/core");
        assert_eq!(find_root(&inner), Some(inner.clone()));
        assert_eq!(find_root(dir.path()), Some(dir.path().to_path_buf()));
    }

    #[test]
    fn typescript_uses_the_local_tsc() {
        let dir = project(&[("tsconfig.json", ""), ("node_modules/.bin/tsc", "")]);
        let check = detect(dir.path()).unwrap().unwrap();
        let tsc = dir.path().join("node_modules/.bin/tsc");
        assert_eq!(
            check.command,
            [tsc.to_str().unwrap(), "--noEmit", "--pretty", "false"]
        );
        assert_eq!(check.counter, Counter::Tsc);
    }

    #[test]
    fn typescript_finds_a_hoisted_tsc() {
        let dir = project(&[
            ("node_modules/.bin/tsc", ""),
            ("packages/web/tsconfig.json", ""),
        ]);
        let check = detect(&dir.path().join("packages/web")).unwrap().unwrap();
        assert_eq!(check.root, dir.path().join("packages/web"));
        assert_eq!(check.counter, Counter::Tsc);
    }

    #[test]
    fn typescript_without_tsc_is_silent() {
        let dir = project(&[("tsconfig.json", "")]);
        assert_eq!(detect(dir.path()), Ok(None));
    }

    #[test]
    fn override_command_wins_over_toolchain() {
        let dir = project(&[
            ("Cargo.toml", ""),
            (
                OVERRIDE_FILE,
                "command = [\"just\", \"check\"]\ntimeout_secs = 120\n",
            ),
        ]);
        let check = detect(dir.path()).unwrap().unwrap();
        assert_eq!(check.command, ["just", "check"]);
        assert_eq!(check.timeout, Duration::from_secs(120));
        assert_eq!(check.counter, Counter::ExitCode);
    }

    #[test]
    fn override_can_add_a_count_prefix() {
        let dir = project(&[(
            OVERRIDE_FILE,
            "command = [\"ruff\", \"check\"]\ncount_prefix = \"error\"\n",
        )]);
        let check = detect(dir.path()).unwrap().unwrap();
        assert_eq!(check.counter, Counter::Prefix("error".into()));
    }

    #[test]
    fn override_without_command_tunes_the_toolchain() {
        let dir = project(&[("Cargo.toml", ""), (OVERRIDE_FILE, "timeout_secs = 5\n")]);
        let check = detect(dir.path()).unwrap().unwrap();
        assert_eq!(check.counter, Counter::Cargo);
        assert_eq!(check.timeout, Duration::from_secs(5));
    }

    #[test]
    fn override_alone_without_command_is_silent() {
        let dir = project(&[(OVERRIDE_FILE, "timeout_secs = 5\n")]);
        assert_eq!(detect(dir.path()), Ok(None));
    }

    #[test]
    fn invalid_overrides_are_errors() {
        for bad in [
            "command = \"just check\"",
            "command = []",
            "timeout_secs = 0",
            "comand = [\"typo\"]",
            "not toml",
        ] {
            let dir = project(&[(OVERRIDE_FILE, bad)]);
            assert!(detect(dir.path()).is_err(), "accepted: {bad}");
        }
    }
}
