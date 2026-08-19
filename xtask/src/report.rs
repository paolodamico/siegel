//! Per-test-case reporting shared by the Kotlin and Swift suites.
//!
//! Both runners produce their own verbose logs; these helpers reduce them to a
//! single colored pass/fail table plus an actionable error when the run fails.

use std::io::IsTerminal;
use std::process::ExitStatus;

use anyhow::{Result, bail};

/// How many log lines to surface when a suite fails.
const FAILURE_DETAIL_LINES: usize = 20;

/// One executed test case.
pub struct TestCase {
    /// Fully-qualified test name, e.g. `siegel.SiegelGuardTests.testFoo`.
    pub name: String,
    /// Human-readable duration as reported by the runner, e.g. `0.01s`.
    pub duration: String,
    pub passed: bool,
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

/// Print the per-case table and fail unless every case passed and `runner` exited zero.
pub fn summarize(title: &str, cases: &[TestCase], runner: &Runner<'_>) -> Result<()> {
    let p = Palette::detect();
    let passed = cases.iter().filter(|case| case.passed).count();
    let failed = cases.len() - passed;

    println!();
    println!("{}===== {title} ====={}", p.bold, p.reset);
    if cases.is_empty() {
        println!("  {}(no test cases were executed){}", p.yellow, p.reset);
    }
    for case in cases {
        let (mark, color) = if case.passed {
            ("✓", p.green)
        } else {
            ("✗", p.red)
        };
        println!(
            "  {color}{mark}{reset} {name} {dim}({duration}){reset}",
            reset = p.reset,
            name = case.name,
            dim = p.dim,
            duration = case.duration,
        );
    }
    println!();
    println!(
        "{bold}Total:{reset}  {total}   {green}Passed:{reset} {passed}   {red}Failed:{reset} {failed}",
        bold = p.bold,
        reset = p.reset,
        total = cases.len(),
        green = p.green,
        red = p.red,
    );

    if cases.is_empty() {
        bail!(
            "{} did not execute any test cases ({})",
            runner.name,
            runner.status
        );
    }
    if failed > 0 {
        println!();
        println!("{}Failure detail:{}", p.bold, p.reset);
        for line in runner
            .log
            .lines()
            .filter(|line| (runner.detail)(line))
            .take(FAILURE_DETAIL_LINES)
        {
            println!("  {line}");
        }
        bail!("{failed} of {} test case(s) failed", cases.len());
    }
    if !runner.status.success() {
        bail!(
            "every test case passed but {} exited with {}",
            runner.name,
            runner.status
        );
    }
    println!(
        "{}PASS{} — all {} tests succeeded",
        p.green,
        p.reset,
        cases.len()
    );
    Ok(())
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
        if colored {
            Self {
                green: "\x1b[0;32m",
                red: "\x1b[0;31m",
                yellow: "\x1b[0;33m",
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                reset: "\x1b[0m",
            }
        } else {
            Self {
                green: "",
                red: "",
                yellow: "",
                bold: "",
                dim: "",
                reset: "",
            }
        }
    }
}
