//! `nomake test`: builds the packages of type `test` and runs each one. A test passes when it exits with 0.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::commands::{self, BuildOpts};
use crate::config::{TestConfig, DEFAULT_TEST_TIMEOUT};
use crate::Result;

pub struct Test {
    pub name: String,
    pub dir: PathBuf,
    pub config: TestConfig,
}

enum Outcome {
    Passed,
    Failed(Option<i32>),
    TimedOut(u64),
}

type Sink = Arc<Mutex<Vec<u8>>>;

fn drain<R: Read + Send + 'static>(mut from: R, sink: Sink) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(count) = from.read(&mut chunk) {
            if count == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..count]);
        }
    })
}

/// Runs `exe` in the test's folder with stdout and stderr captured, and kills it after the timeout.
fn run_one(exe: &PathBuf, test: &Test, extra: &[String]) -> Result<(Outcome, String)> {
    let mut command = Command::new(exe);
    command
        .args(&test.config.args)
        .args(extra)
        .current_dir(&test.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let sink: Sink = Arc::default();
    let readers = [
        drain(child.stdout.take().unwrap(), sink.clone()),
        drain(child.stderr.take().unwrap(), sink.clone()),
    ];
    let limit = test.config.timeout.unwrap_or(DEFAULT_TEST_TIMEOUT);
    let deadline = Instant::now() + Duration::from_secs(limit);
    let outcome = loop {
        if let Some(status) = child.try_wait()? {
            break if status.success() {
                Outcome::Passed
            } else {
                Outcome::Failed(status.code())
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Outcome::TimedOut(limit);
        }
        thread::sleep(Duration::from_millis(10));
    };
    // A killed test may have left children holding the pipes open, so only a finished one is waited for.
    if !matches!(outcome, Outcome::TimedOut(_)) {
        for reader in readers {
            let _ = reader.join();
        }
    }
    let text = String::from_utf8_lossy(&sink.lock().unwrap()).into_owned();
    Ok((outcome, text))
}

/// Builds `tests` as one graph and runs them in order; `extra` goes to every test after its own `args`.
/// The result is the exit code: 0 when every test passed.
pub fn run(mut opts: BuildOpts, tests: &[Test], extra: &[String]) -> Result<i32> {
    if tests.is_empty() {
        bail!("no test packages (a package with type = \"test\" is one)");
    }
    opts.refresh_compdb = false;
    opts.roots = tests.iter().map(|test| test.dir.clone()).collect();
    let verbose = opts.verbose;
    let built = commands::build(&opts)?;
    if !built.ok {
        return Ok(1);
    }
    let mut failed = Vec::new();
    println!();
    for test in tests {
        let Some((_, exe)) = built.targets.iter().find(|(dir, _)| *dir == test.dir) else {
            bail!("internal: no build output for test '{}'", test.name);
        };
        print!("test {} ... ", test.name);
        std::io::stdout().flush()?;
        let start = Instant::now();
        let (outcome, output) = run_one(exe, test, extra)?;
        let took = start.elapsed();
        match &outcome {
            Outcome::Passed => println!("ok ({took:.2?})"),
            Outcome::Failed(Some(code)) => println!("FAILED (exit code {code}, {took:.2?})"),
            Outcome::Failed(None) => println!("FAILED (terminated abnormally, {took:.2?})"),
            Outcome::TimedOut(limit) => println!("FAILED (timed out after {limit}s)"),
        }
        let passed = matches!(outcome, Outcome::Passed);
        if (!passed || verbose) && !output.is_empty() {
            let text = output.trim_end();
            println!("---- {} output ----\n{text}\n----", test.name);
        }
        if !passed {
            failed.push(test.name.as_str());
        }
    }
    println!();
    if failed.is_empty() {
        println!("nomake: {} test(s) passed", tests.len());
        Ok(0)
    } else {
        eprintln!(
            "nomake: {} of {} test(s) failed: {}",
            failed.len(),
            tests.len(),
            failed.join(", ")
        );
        Ok(1)
    }
}
