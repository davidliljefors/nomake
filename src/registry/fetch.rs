//! Fetching a recipe's sources into the shared cache: `<cache>/src/<name>-<version>-<id>/`, never modified afterwards.

use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    fn run(&self) -> Result<()> {
        if self.dest.is_dir() {
            return Ok(());
        }
        let (name, version) = (&self.name, &self.version);
        let src = self.dest.parent().unwrap();
        fs::create_dir_all(src)?;
        println!("nomake: fetching {name} {version}");
        let temp = src.join(format!(".tmp-{name}-{version}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp)?;
        if let Err(err) = fetch_git(&self.source, &temp) {
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
        Ok(())
    }
}

/// Fetches whatever is missing, in parallel.
pub fn fetch_all(jobs: &[Job]) -> Result<()> {
    let mut seen = HashSet::new();
    let missing: Vec<&Job> = jobs
        .iter()
        .filter(|job| !job.dest.is_dir() && seen.insert(&job.dest))
        .collect();
    missing
        .par_iter()
        .map(|job| job.run())
        .collect::<Result<Vec<_>>>()
        .map(|_| ())
}

fn fetch_git(source: &Source, dir: &Path) -> Result<()> {
    let git = |args: &[&str]| run(Command::new("git").args(args).current_dir(dir));
    git(&["init", "-q"])?;
    git(&["fetch", "-q", "--depth", "1", &source.git, &source.rev])?;
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
