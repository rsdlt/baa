//! Turn a finished check into a verdict and its sidebar token. Pure functions only.

use crate::detect::Counter;

/// Shown while a check is running.
pub const CHECKING: &str = "…";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    /// Failed, with the error count when the output could be read.
    Failed(Option<usize>),
    TimedOut,
}

impl Verdict {
    /// The `$baa` token value. Always far below Herdr's 80-character limit.
    pub fn token(self) -> String {
        match self {
            Verdict::Passed => "✓".into(),
            Verdict::Failed(Some(count)) => format!("✗ {count}"),
            Verdict::Failed(None) => "✗".into(),
            Verdict::TimedOut => "⏱".into(),
        }
    }
}

/// Judges a check that ran to completion.
pub fn verdict(counter: &Counter, success: bool, output: &str) -> Verdict {
    if success {
        Verdict::Passed
    } else {
        Verdict::Failed(count_errors(counter, output))
    }
}

/// Counts error lines in a failed check's output.
///
/// Zero matches means the output wasn't understood (a broken manifest, say),
/// so that is reported as an unknown count rather than as "0 errors".
pub fn count_errors(counter: &Counter, output: &str) -> Option<usize> {
    let count = match counter {
        // `src/main.rs:2:17: error[E0308]: …`; the closing "could not compile" summary
        // has no location, so it isn't counted twice.
        Counter::Cargo => count_lines(output, |line| {
            line.contains(": error[") || line.contains(": error:")
        }),
        // `src/index.ts(2,1): error TS2304: …`. Piped tsc prints no "Found N errors".
        Counter::Tsc => count_lines(output, |line| line.contains(": error TS")),
        Counter::Contains(marker) => count_lines(output, |line| line.contains(marker.as_str())),
        Counter::ExitCode => 0,
    };
    (count > 0).then_some(count)
}

fn count_lines(output: &str, is_error: impl Fn(&str) -> bool) -> usize {
    output.lines().filter(|line| is_error(line)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../tests/fixtures/", $name, ".txt"))
        };
    }

    #[test]
    fn tokens() {
        assert_eq!(Verdict::Passed.token(), "✓");
        assert_eq!(Verdict::Failed(Some(3)).token(), "✗ 3");
        assert_eq!(Verdict::Failed(None).token(), "✗");
        assert_eq!(Verdict::TimedOut.token(), "⏱");
    }

    #[test]
    fn success_passes_whatever_the_output() {
        let warnings = fixture!("cargo_ok");
        assert_eq!(verdict(&Counter::Cargo, true, warnings), Verdict::Passed);
        assert_eq!(
            verdict(&Counter::Tsc, true, fixture!("tsc_ok")),
            Verdict::Passed
        );
    }

    #[test]
    fn cargo_counts() {
        let cargo = |output| verdict(&Counter::Cargo, false, output);
        assert_eq!(cargo(fixture!("cargo_errors")), Verdict::Failed(Some(3)));
        assert_eq!(cargo(fixture!("cargo_one")), Verdict::Failed(Some(1)));
        assert_eq!(cargo(fixture!("cargo_workspace")), Verdict::Failed(Some(3)));
        assert_eq!(cargo(fixture!("cargo_manifest")), Verdict::Failed(None));
    }

    #[test]
    fn tsc_counts() {
        let tsc = |output| verdict(&Counter::Tsc, false, output);
        assert_eq!(tsc(fixture!("tsc_errors")), Verdict::Failed(Some(3)));
        assert_eq!(tsc(fixture!("tsc_badconfig")), Verdict::Failed(Some(2)));
    }

    #[test]
    fn marker_counts_matching_lines() {
        // eslint and mypy style: the marker sits after a location.
        let output = "src/a.js\n  1:5  error  no-undef\nsrc/b.py:3: error: bad\nwarning: x\n";
        let counter = Counter::Contains("error".into());
        assert_eq!(count_errors(&counter, output), Some(2));
    }

    #[test]
    fn exit_code_has_no_count() {
        let output = fixture!("cargo_errors");
        assert_eq!(count_errors(&Counter::ExitCode, output), None);
        assert_eq!(count_errors(&Counter::Cargo, ""), None);
    }
}
