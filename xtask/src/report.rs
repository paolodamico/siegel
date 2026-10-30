//! Per-test-case reporting shared by the Kotlin and Swift suites.
//!
//! Both runners produce their own verbose logs; these helpers reduce them to a
//! single colored table plus an actionable error when the run fails.

use std::io::IsTerminal;
use std::process::ExitStatus;

use anyhow::{Result, bail};

/// How many log lines to surface when a suite fails.
const FAILURE_DETAIL_LINES: usize = 20;

/// What a runner reported for one test case.
///
/// `Skipped` is tracked separately rather than folded into `Passed`: an
/// `@Ignore`d or `XCTSkip`ped test did not run, and reporting it as a pass would
/// let a silently-disabled suite claim success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
}

/// One test case as reported by the runner.
pub struct TestCase {
    /// Fully-qualified test name, e.g. `siegel.SiegelGuardTests.testFoo`.
    pub name: String,
    /// Human-readable duration as reported by the runner, e.g. `0.01s`.
    pub duration: String,
    pub outcome: Outcome,
}

/// The tool that executed the suite, along with its captured output.
pub struct Runner<'a> {
    /// Display name used in error messages, e.g. `gradle`.
    pub name: &'a str,
    pub status: ExitStatus,
    /// Merged stdout+stderr of the run.
    pub log: &'a str,
    /// Selects the log lines worth printing when a test case fails.
    pub detail: fn(&str) -> bool,
}

/// Print the per-case table, then fail unless at least one case actually ran,
/// none failed, and `runner` exited zero.
pub fn summarize(title: &str, cases: &[TestCase], runner: &Runner<'_>) -> Result<()> {
    let palette = Palette::detect();
    let count = |wanted: Outcome| cases.iter().filter(|case| case.outcome == wanted).count();
    let (passed, failed, skipped) = (
        count(Outcome::Passed),
        count(Outcome::Failed),
        count(Outcome::Skipped),
    );

    print_table(title, cases, &palette);
    print_totals(cases.len(), passed, failed, skipped, &palette);

    // Every failing path prints the detail block, not just the "a case failed"
    // one: a run that executed nothing, or that reported all-green while the
    // runner exited non-zero, is exactly when there is nothing else to triage
    // from — the whitelist filter may have surfaced none of it live.
    if passed + failed == 0 {
        print_failure_detail(runner, &palette);
        bail!(
            "{} did not execute any test cases ({skipped} skipped, {})",
            runner.name,
            runner.status
        );
    }
    if failed > 0 {
        print_failure_detail(runner, &palette);
        bail!("{failed} of {} test case(s) failed", cases.len());
    }
    if !runner.status.success() {
        print_failure_detail(runner, &palette);
        bail!(
            "every test case passed but {} exited with {}",
            runner.name,
            runner.status
        );
    }
    println!(
        "{}PASS{} — {passed} test(s) succeeded{}",
        palette.green,
        palette.reset,
        if skipped > 0 {
            format!(", {skipped} skipped")
        } else {
            String::new()
        }
    );
    Ok(())
}

fn print_table(title: &str, cases: &[TestCase], palette: &Palette) {
    println!();
    println!("{}===== {title} ====={}", palette.bold, palette.reset);
    if cases.is_empty() {
        println!(
            "  {}(no test cases were executed){}",
            palette.yellow, palette.reset
        );
    }
    for case in cases {
        let (mark, color) = match case.outcome {
            Outcome::Passed => ("✓", palette.green),
            Outcome::Failed => ("✗", palette.red),
            Outcome::Skipped => ("-", palette.yellow),
        };
        println!(
            "  {color}{mark}{reset} {name} {dim}({duration}){reset}",
            reset = palette.reset,
            name = case.name,
            dim = palette.dim,
            duration = case.duration,
        );
    }
}

fn print_totals(total: usize, passed: usize, failed: usize, skipped: usize, palette: &Palette) {
    println!();
    let (bold, reset) = (palette.bold, palette.reset);
    print!("{bold}Total:{reset}  {total}   ");
    print!("{}Passed:{reset} {passed}   ", palette.green);
    print!("{}Failed:{reset} {failed}", palette.red);
    if skipped > 0 {
        print!("   {}Skipped:{} {skipped}", palette.yellow, palette.reset);
    }
    println!();
}

fn print_failure_detail(runner: &Runner<'_>, palette: &Palette) {
    let selected: Vec<_> = runner
        .log
        .lines()
        .filter(|line| (runner.detail)(line))
        .take(FAILURE_DETAIL_LINES)
        .collect();
    // Errors the filter does not recognise (a simulator that refused to install
    // the bundle, a crashed runner) would otherwise leave nothing to triage.
    let (heading, lines) = if selected.is_empty() {
        ("Last lines of the log:", log_tail(runner.log))
    } else {
        ("Failure detail:", selected)
    };

    println!();
    println!("{}{heading}{}", palette.bold, palette.reset);
    if lines.is_empty() {
        println!(
            "  {}(the runner produced no output){}",
            palette.dim, palette.reset
        );
    }
    for line in lines {
        println!("  {line}");
    }
}

/// The last non-blank lines of `log`, oldest first.
fn log_tail(log: &str) -> Vec<&str> {
    let mut tail: Vec<_> = log
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(FAILURE_DETAIL_LINES)
        .collect();
    tail.reverse();
    tail
}

/// ANSI escapes, blanked out when stdout is not a terminal or `NO_COLOR` is set.
struct Palette {
    green: &'static str,
    red: &'static str,
    yellow: &'static str,
    bold: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Palette {
    fn detect() -> Self {
        let colored = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        let ansi = |code: &'static str| if colored { code } else { "" };
        Self {
            green: ansi("\x1b[0;32m"),
            red: ansi("\x1b[0;31m"),
            yellow: ansi("\x1b[0;33m"),
            bold: ansi("\x1b[1m"),
            dim: ansi("\x1b[2m"),
            reset: ansi("\x1b[0m"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::{Outcome, Runner, TestCase, log_tail, summarize};

    fn case(name: &str, outcome: Outcome) -> TestCase {
        TestCase {
            name: name.to_owned(),
            duration: "0.0s".to_owned(),
            outcome,
        }
    }

    fn runner(code: i32, log: &str) -> Runner<'_> {
        Runner {
            name: "runner",
            status: ExitStatus::from_raw(code << 8),
            log,
            detail: |line| line.contains("BOOM"),
        }
    }

    #[test]
    fn all_passing_and_a_zero_exit_succeeds() {
        let cases = [case("a", Outcome::Passed), case("b", Outcome::Passed)];
        assert!(summarize("t", &cases, &runner(0, "")).is_ok());
    }

    #[test]
    fn a_failed_case_fails_even_when_the_runner_exits_zero() {
        let cases = [case("a", Outcome::Passed), case("b", Outcome::Failed)];
        let error = summarize("t", &cases, &runner(0, "BOOM")).unwrap_err();
        assert!(error.to_string().contains("1 of 2 test case(s) failed"));
    }

    #[test]
    fn a_nonzero_exit_fails_even_when_every_case_passed() {
        let cases = [case("a", Outcome::Passed)];
        let error = summarize("t", &cases, &runner(65, "")).unwrap_err();
        assert!(error.to_string().contains("exited with"));
    }

    #[test]
    fn zero_executed_cases_fails() {
        let error = summarize("t", &[], &runner(0, "")).unwrap_err();
        assert!(error.to_string().contains("did not execute any test cases"));
    }

    #[test]
    fn an_all_skipped_run_fails_rather_than_claiming_success() {
        let cases = [case("a", Outcome::Skipped), case("b", Outcome::Skipped)];
        let error = summarize("t", &cases, &runner(0, "")).unwrap_err();
        assert!(error.to_string().contains("2 skipped"));
    }

    #[test]
    fn skipped_cases_do_not_block_success_when_something_ran() {
        let cases = [case("a", Outcome::Passed), case("b", Outcome::Skipped)];
        assert!(summarize("t", &cases, &runner(0, "")).is_ok());
    }

    #[test]
    fn log_tail_keeps_source_order_and_drops_blank_lines() {
        assert_eq!(log_tail("one\n\ntwo\nthree\n"), ["one", "two", "three"]);
    }
}
