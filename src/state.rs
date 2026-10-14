//! Everything baa keeps on disk, under `HERDR_PLUGIN_STATE_DIR`:
//!
//! ```text
//! baa.log                    what happened, for debugging
//! panes/<pane>               the last agent status seen in each pane
//! projects/<hash>/root       which project root this entry belongs to
//! projects/<hash>/pending    a check was requested
//! projects/<hash>/lock       locked while a worker runs this project's checks
//! projects/<hash>/last.txt   the last check's report
//! ```

use std::env;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Saved reports keep at most this much output; the first errors matter most.
const MAX_REPORT_BYTES: usize = 64 * 1024;
/// The log is rotated to `baa.log.old` past this size.
const MAX_LOG_BYTES: u64 = 256 * 1024;

pub struct State {
    dir: PathBuf,
}

impl State {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn from_env() -> Result<Self, String> {
        env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(Self::new)
            .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR is not set".into())
    }

    /// Records `status` as a pane's latest and returns the one before it.
    pub fn swap_pane_status(&self, pane_id: &str, status: &str) -> io::Result<Option<String>> {
        let path = self.dir.join("panes").join(file_name(pane_id));
        let previous = read_optional(&path)?;
        fs::create_dir_all(self.dir.join("panes"))?;
        fs::write(&path, status)?;
        Ok(previous)
    }

    pub fn project(&self, root: &Path) -> io::Result<Project> {
        let path = root.as_os_str().as_encoded_bytes();
        let dir = self
            .dir
            .join("projects")
            .join(format!("{:016x}", fnv1a(path)));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("root"), path)?;
        Ok(Project { dir })
    }

    /// Appends a line to `baa.log` and echoes it to stderr, which Herdr's plugin log shows.
    /// Logging must never break baa, so failures are ignored.
    pub fn log(&self, message: &str) {
        eprintln!("baa: {message}");
        let path = self.dir.join("baa.log");
        if fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
            let _ = fs::rename(&path, self.dir.join("baa.log.old"));
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let _ = fs::create_dir_all(&self.dir).and_then(|()| {
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            writeln!(file, "{secs} {message}")
        });
    }

    /// Deletes everything baa has saved.
    pub fn clear(&self) -> io::Result<()> {
        for dir in ["panes", "projects"] {
            ignore_missing(fs::remove_dir_all(self.dir.join(dir)))?;
        }
        for file in ["baa.log", "baa.log.old"] {
            ignore_missing(fs::remove_file(self.dir.join(file)))?;
        }
        Ok(())
    }
}

/// One project's queue, lock and last report.
pub struct Project {
    dir: PathBuf,
}

impl Project {
    /// Asks for a check. Any number of requests made during a check collapse into one more run.
    pub fn request(&self) -> io::Result<()> {
        fs::write(self.dir.join("pending"), "")
    }

    /// Clears the request flag, returning whether one was set.
    fn take_request(&self) -> bool {
        fs::remove_file(self.dir.join("pending")).is_ok()
    }

    fn has_request(&self) -> bool {
        self.dir.join("pending").exists()
    }

    pub fn save_report(&self, report: &str) -> io::Result<()> {
        fs::write(
            self.dir.join("last.txt"),
            truncate(report.trim(), MAX_REPORT_BYTES),
        )
    }

    pub fn last_report(&self) -> io::Result<Option<String>> {
        read_optional(&self.dir.join("last.txt"))
    }
}

/// Runs `check` once per pending request, unless another worker holds this project's lock.
///
/// The lock is a kernel file lock, so a worker that dies releases it automatically.
/// Requests made while a check runs leave `pending` set and get exactly one follow-up run.
pub fn run_pending(project: &Project, mut check: impl FnMut()) -> io::Result<()> {
    loop {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(project.dir.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            // Someone else is checking; they will see our request.
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Error(e)) => return Err(e),
        }
        while project.take_request() {
            check();
        }
        drop(lock);
        // A request made between our last look and the unlock found the lock taken
        // and left it to us, so look once more before leaving.
        if !project.has_request() {
            return Ok(());
        }
    }
}

/// Pane ids look like `w8:p1`; keep file names to a safe alphabet.
fn file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// FNV-1a: a tiny hash that, unlike std's `DefaultHasher`, is stable across Rust releases.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn truncate(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let state = State::new(dir.path());
        (dir, state)
    }

    #[test]
    fn remembers_the_previous_pane_status() {
        let (_dir, state) = state();
        assert_eq!(state.swap_pane_status("w1:p1", "working").unwrap(), None);
        assert_eq!(
            state.swap_pane_status("w1:p1", "idle").unwrap().as_deref(),
            Some("working")
        );
        assert_eq!(state.swap_pane_status("w1:p2", "done").unwrap(), None);
    }

    #[test]
    fn projects_are_keyed_by_root() {
        let (_dir, state) = state();
        let a = state.project(Path::new("/code/a")).unwrap();
        let again = state.project(Path::new("/code/a")).unwrap();
        let b = state.project(Path::new("/code/b")).unwrap();
        assert_eq!(a.dir, again.dir);
        assert_ne!(a.dir, b.dir);
        assert_eq!(fs::read_to_string(a.dir.join("root")).unwrap(), "/code/a");
    }

    #[test]
    fn one_request_runs_once() {
        let (_dir, state) = state();
        let project = state.project(Path::new("/code/a")).unwrap();
        project.request().unwrap();
        project.request().unwrap();
        let mut runs = 0;
        run_pending(&project, || runs += 1).unwrap();
        assert_eq!(runs, 1);
        run_pending(&project, || runs += 1).unwrap();
        assert_eq!(runs, 1, "nothing pending, nothing to run");
    }

    #[test]
    fn requests_during_a_check_collapse_into_one_more_run() {
        let (_dir, state) = state();
        let project = state.project(Path::new("/code/a")).unwrap();
        project.request().unwrap();
        let mut runs = 0;
        run_pending(&project, || {
            runs += 1;
            if runs == 1 {
                // Three agents finish while the first check runs.
                for _ in 0..3 {
                    project.request().unwrap();
                }
            }
        })
        .unwrap();
        assert_eq!(runs, 2);
    }

    #[test]
    fn a_busy_project_is_left_to_its_worker() {
        let (_dir, state) = state();
        let project = state.project(Path::new("/code/a")).unwrap();
        let other_worker = File::create(project.dir.join("lock")).unwrap();
        other_worker.lock().unwrap();

        project.request().unwrap();
        let mut runs = 0;
        run_pending(&project, || runs += 1).unwrap();
        assert_eq!(runs, 0);
        assert!(
            project.has_request(),
            "the request waits for the running worker"
        );

        drop(other_worker);
        // Tests run in parallel and some spawn processes; a child forked at the wrong moment
        // shares our descriptors, lock included, until it execs. Retrying is safe: a busy
        // lock leaves the request pending.
        for _ in 0..200 {
            run_pending(&project, || runs += 1).unwrap();
            if runs > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(runs, 1);
    }

    #[test]
    fn reports_are_trimmed_and_capped() {
        let (_dir, state) = state();
        let project = state.project(Path::new("/code/a")).unwrap();
        assert_eq!(project.last_report().unwrap(), None);
        project.save_report("\n  ok  \n\n").unwrap();
        assert_eq!(project.last_report().unwrap().as_deref(), Some("ok"));

        project.save_report(&"é".repeat(MAX_REPORT_BYTES)).unwrap();
        let saved = project.last_report().unwrap().unwrap();
        assert_eq!(saved.len(), MAX_REPORT_BYTES);
    }

    #[test]
    fn clear_removes_everything() {
        let (dir, state) = state();
        state.swap_pane_status("w1:p1", "done").unwrap();
        state
            .project(Path::new("/code/a"))
            .unwrap()
            .request()
            .unwrap();
        state.log("hello");
        assert!(dir.path().join("baa.log").exists());

        state.clear().unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        state.clear().unwrap();
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("w8:p1"), "w8_p1");
        assert_eq!(file_name("../x"), "___x");
    }
}
