//! Run a check command with a timeout and capture its output.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The command ran to the end. `output` is stdout followed by stderr.
    Exited { success: bool, output: String },
    /// The command and everything it started were killed at the timeout.
    TimedOut { output: String },
    /// The program isn't installed.
    NotFound,
}

/// Runs `command` in `dir`, killing it once `timeout` has passed.
pub fn run(command: &[String], dir: &Path, timeout: Duration) -> io::Result<Outcome> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let spawned = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout also kills what it started (cargo runs rustc).
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Outcome::NotFound),
        Err(e) => return Err(e),
    };

    // Read both pipes on their own threads: a command that fills one pipe while
    // nobody reads it would otherwise block forever.
    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            kill_group(&mut child);
            child.wait()?;
            break None;
        }
        thread::sleep(POLL_INTERVAL);
    };

    let mut output = stdout.join().unwrap_or_default();
    output.push_str(&stderr.join().unwrap_or_default());
    Ok(match status {
        Some(status) => Outcome::Exited {
            success: status.success(),
            output,
        },
        None => Outcome::TimedOut { output },
    })
}

fn read_all(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            // Whatever was read before an error is still worth keeping.
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn kill_group(child: &mut Child) {
    // The child leads its own group (see `process_group(0)`), so its pid is the group id.
    match libc::pid_t::try_from(child.id()) {
        // SAFETY: `killpg` only sends a signal; it has no memory-safety requirements.
        Ok(group) => unsafe {
            libc::killpg(group, libc::SIGKILL);
        },
        Err(_) => {
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Outcome {
        let command = ["sh", "-c", script].map(String::from);
        run(&command, Path::new("."), timeout).unwrap()
    }

    #[test]
    fn captures_stdout_then_stderr_and_exit_status() {
        let outcome = sh("echo out; echo err >&2; exit 3", Duration::from_secs(5));
        assert_eq!(
            outcome,
            Outcome::Exited {
                success: false,
                output: "out\nerr\n".into()
            }
        );
        let ok = sh("true", Duration::from_secs(5));
        assert!(matches!(ok, Outcome::Exited { success: true, .. }));
    }

    #[test]
    fn runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        let command = ["pwd"].map(String::from);
        let Outcome::Exited { output, .. } =
            run(&command, dir.path(), Duration::from_secs(5)).unwrap()
        else {
            panic!("pwd did not exit");
        };
        let expected = dir.path().canonicalize().unwrap();
        assert_eq!(Path::new(output.trim()).canonicalize().unwrap(), expected);
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let started = Instant::now();
        // `sleep` holds the output pipe open; if only `sh` died we would wait for it.
        let outcome = sh(
            "echo started; sleep 30; echo never",
            Duration::from_millis(300),
        );
        assert_eq!(
            outcome,
            Outcome::TimedOut {
                output: "started\n".into()
            }
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn missing_program_is_not_found() {
        let command = ["baa-no-such-program"].map(String::from);
        let outcome = run(&command, Path::new("."), Duration::from_secs(1)).unwrap();
        assert_eq!(outcome, Outcome::NotFound);
    }

    #[test]
    fn empty_command_is_an_error() {
        assert!(run(&[], Path::new("."), Duration::from_secs(1)).is_err());
    }
}
