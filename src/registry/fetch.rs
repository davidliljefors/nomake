//! Fetching a recipe's sources into the shared cache: `<cache>/src/<name>-<version>-<id>/`, never modified afterwards.

use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::io::{BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

use super::run;
use crate::config::Source;
use crate::util::hash;
use crate::Result;

pub struct Job {
    name: String,
    version: String,
    source: Source,
    /// Where the sources end up; known before they are fetched.
    pub dest: PathBuf,
}

impl Job {
    pub fn new(src: &Path, name: &str, version: &str, source: &Source) -> Job {
        let id = format!(
            "{:016x}",
            hash(format!("{}\0{}", source.git, source.rev).as_bytes())
        );
        Job {
            name: name.into(),
            version: version.into(),
            source: source.clone(),
            dest: src.join(format!("{name}-{version}-{id}")),
        }
    }

    fn run(&self, board: &Board, row: usize) -> Result<()> {
        if self.dest.is_dir() {
            return Ok(());
        }
        let (name, version) = (&self.name, &self.version);
        let src = self.dest.parent().unwrap();
        fs::create_dir_all(src)?;
        if !board.live {
            println!("nomake: fetching {name} {version}");
        }
        let temp = src.join(format!(".tmp-{name}-{version}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp)?;
        if let Err(err) = fetch_git(&self.source, &temp, &mut |percent| board.set(row, percent)) {
            let _ = fs::remove_dir_all(&temp);
            return Err(format!("fetching {name} {version}: {err}").into());
        }
        // Another process may have finished first; either copy is identical.
        if fs::rename(&temp, &self.dest).is_err() {
            let _ = fs::remove_dir_all(&temp);
            if !self.dest.is_dir() {
                bail!(
                    "cannot move fetched sources of {name} {version} into {}",
                    self.dest.display()
                );
            }
        }
        board.set(row, 100);
        Ok(())
    }
}

/// The percentage of every running fetch, redrawn as a single line when stdout is a terminal.
struct Board {
    rows: Mutex<Vec<(String, u32)>>,
    drawn: AtomicUsize,
    live: bool,
}

impl Board {
    fn new(jobs: &[&Job]) -> Board {
        Board {
            rows: Mutex::new(
                jobs.iter()
                    .map(|job| (format!("{} {}", job.name, job.version), 0))
                    .collect(),
            ),
            drawn: AtomicUsize::new(0),
            live: std::io::stdout().is_terminal(),
        }
    }

    fn set(&self, row: usize, percent: u32) {
        let mut rows = self.rows.lock().unwrap();
        if rows[row].1 == percent {
            return;
        }
        rows[row].1 = percent;
        if self.live {
            let parts: Vec<String> = rows
                .iter()
                .filter(|(_, percent)| *percent < 100)
                .map(|(name, percent)| format!("{name} {percent}%"))
                .collect();
            let line = format!("nomake: fetching {}", parts.join(", "));
            let stale = self
                .drawn
                .swap(line.len(), SeqCst)
                .saturating_sub(line.len());
            print!("\r{line}{:stale$}", "");
            let _ = std::io::stdout().flush();
        }
    }

    fn finish(&self) {
        if self.live && self.drawn.load(SeqCst) > 0 {
            println!();
        }
    }
}

/// Fetches whatever is missing, in parallel.
pub fn fetch_all(jobs: &[Job]) -> Result<()> {
    let mut seen = HashSet::new();
    let missing: Vec<&Job> = jobs
        .iter()
        .filter(|job| !job.dest.is_dir() && seen.insert(&job.dest))
        .collect();
    let board = Board::new(&missing);
    let result = missing
        .par_iter()
        .enumerate()
        .map(|(row, job)| job.run(&board, row))
        .collect::<Result<Vec<_>>>()
        .map(|_| ());
    board.finish();
    result
}

fn receiving_percent(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("Receiving objects:")?;
    rest.trim_start().split('%').next()?.parse().ok()
}

/// `git fetch` with its progress read from stderr, where git ends each update with `\r`.
fn git_fetch(source: &Source, dir: &Path, on_percent: &mut dyn FnMut(u32)) -> Result<()> {
    let mut child = Command::new("git")
        .args([
            "fetch",
            "--progress",
            "--depth",
            "1",
            &source.git,
            &source.rev,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let (mut line, mut errors) = (Vec::new(), String::new());
    for byte in BufReader::new(child.stderr.take().unwrap()).bytes() {
        let byte = byte?;
        if byte != b'\r' && byte != b'\n' {
            line.push(byte);
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(percent) = receiving_percent(&text) {
            on_percent(percent);
        } else if !text.is_empty() && !text.contains('%') {
            errors.push_str(&text);
            errors.push('\n');
        }
        line.clear();
    }
    if child.wait()?.success() {
        return Ok(());
    }
    Err(format!("git fetch failed: {}", errors.trim()).into())
}

fn fetch_git(source: &Source, dir: &Path, on_percent: &mut dyn FnMut(u32)) -> Result<()> {
    let git = |args: &[&str]| run(Command::new("git").args(args).current_dir(dir));
    git(&["init", "-q"])?;
    git_fetch(source, dir, on_percent)?;
    git(&[
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "-q",
        "FETCH_HEAD",
    ])?;
    fs::remove_dir_all(dir.join(".git"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn percent_is_read_from_the_receiving_line_only() {
        assert_eq!(
            receiving_percent("Receiving objects:  43% (86/200), 1.2 MiB"),
            Some(43)
        );
        assert_eq!(
            receiving_percent("Receiving objects: 100% (200/200), done."),
            Some(100)
        );
        assert_eq!(receiving_percent("Resolving deltas:  43% (3/7)"), None);
        assert_eq!(receiving_percent("fatal: repository not found"), None);
    }

    #[test]
    fn missing_sources_are_fetched_once_into_the_cache() {
        let root = env::temp_dir().join(format!("nomake_fetch_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repo, rev) = super::super::tests::repo_with_commit(&root);
        let source = Source {
            git: repo.display().to_string(),
            rev,
        };
        let job = Job::new(&root.join("src"), "lib", "1.0.0", &source);
        // The same job twice must not race against itself.
        fetch_all(&[
            Job::new(&root.join("src"), "lib", "1.0.0", &source),
            Job::new(&root.join("src"), "lib", "1.0.0", &source),
        ])
        .unwrap();
        assert!(job.dest.join("a.c").is_file() && !job.dest.join(".git").exists());
        fetch_all(&[job]).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
