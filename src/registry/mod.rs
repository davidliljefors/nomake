//! Registry dependencies: libraries described by recipes in the registry, built from sources fetched into a shared cache.
//!
//! A [`Resolver`] turns a dependency like `lua = { version = "5.4" }` into a recipe and a source directory.
//! `nomake.lock` decides the version, so the registry is only read for dependencies it doesn't list yet, or when
//! `nomake update` asks for newer ones. Fetching is a separate step so it can run in parallel.

mod browse;
mod commands;
mod fetch;
mod index;
mod lock;
mod version;

pub use browse::{catalog, details, Details, Entry};
pub use commands::run as command;

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Manifest;
use crate::Result;
use index::Registry;
use lock::{Lock, Locked};

pub struct Located {
    /// Where the sources will be once fetched; the package root.
    pub root: PathBuf,
    /// The recipe's `nomake.toml`.
    pub recipe: PathBuf,
}

/// `NOMAKE_HOME`, else the per-user cache directory.
fn cache_dir() -> Result<PathBuf> {
    if let Some(home) = env::var_os("NOMAKE_HOME") {
        return Ok(PathBuf::from(home));
    }
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    match base {
        Some(base) => Ok(base.join("nomake")),
        None => bail!("cannot find a cache directory (set NOMAKE_HOME)"),
    }
}

fn run(command: &mut Command) -> Result<()> {
    // A repository that needs credentials must fail, not wait for a prompt.
    command.env("GIT_TERMINAL_PROMPT", "0");
    let output = command
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", command.get_program()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{:?} failed: {}",
        command.get_program(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

/// Resolves registry dependencies for one build against the root project's lock.
pub struct Resolver {
    registry: Registry,
    lock: Lock,
    src: PathBuf,
    jobs: Vec<fetch::Job>,
}

impl Resolver {
    /// `update` lists the dependencies to re-resolve against the newest registry; an empty list means all of them.
    pub fn new(root: &Path, update: Option<&[String]>) -> Result<Resolver> {
        let mut registry = Registry::open()?;
        let mut lock = Lock::load(root)?;
        if let Some(names) = update {
            registry.refresh()?;
            lock.forget(names);
        }
        Ok(Resolver {
            registry,
            lock,
            src: cache_dir()?.join("src"),
            jobs: Vec::new(),
        })
    }

    /// Picks the version (the locked one when it satisfies `requirement`) and queues its sources for fetching.
    pub fn locate(&mut self, name: &str, requirement: &str) -> Result<Located> {
        let locked = self.lock.find(name, requirement).cloned();
        let recipe = match &locked {
            Some(entry) => self.registry.exact(name, &entry.version)?,
            None => self.registry.newest(name, requirement)?,
        };
        let manifest = Manifest::load(&recipe.path)?;
        let Some(source) = &manifest.source else {
            bail!("{} has no [source] table", recipe.path.display());
        };
        if locked.is_some_and(|entry| entry.rev != source.rev) {
            bail!(
                "the recipe for {name} {} changed since it was locked (run `nomake update {name}`)",
                recipe.version
            );
        }
        self.lock.record(Locked {
            name: name.into(),
            version: recipe.version.clone(),
            rev: source.rev.clone(),
        });
        let job = fetch::Job::new(&self.src, name, &recipe.version, source);
        let root = job.dest.clone();
        self.jobs.push(job);
        Ok(Located {
            root,
            recipe: recipe.path,
        })
    }

    /// Fetches the sources of every located dependency that isn't in the cache yet, in parallel.
    pub fn fetch(&mut self) -> Result<()> {
        fetch::fetch_all(&std::mem::take(&mut self.jobs))
    }

    /// Writes `nomake.lock`.
    pub fn save(&self) -> Result<()> {
        self.lock.save()
    }
}

#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A repository with one commit; returns its path and full commit hash.
    pub fn repo_with_commit(root: &Path) -> (PathBuf, String) {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("a.c"), "int a;").unwrap();
        let git = |args: &[&str]| run(Command::new("git").args(args).current_dir(&repo)).unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "x",
        ]);
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        (
            repo,
            String::from_utf8(head.stdout).unwrap().trim().to_string(),
        )
    }

    #[test]
    fn registry_dependencies_are_locked_added_and_updated() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = env::temp_dir().join(format!("nomake_lock_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repo, rev) = repo_with_commit(&root);
        let registry = root.join("registry");
        let publish = |version: &str| {
            let dir = registry.join("lua").join(version);
            fs::create_dir_all(&dir).unwrap();
            let git = repo.display().to_string().replace('\\', "/");
            fs::write(
                dir.join("nomake.toml"),
                format!("[package]\nname = \"lua\"\n[source]\ngit = \"{git}\"\nrev = \"{rev}\"\n"),
            )
            .unwrap();
        };
        env::set_var("NOMAKE_HOME", root.join("home"));
        env::set_var("NOMAKE_REGISTRY", &registry);
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let version_of = |update: Option<&[String]>| {
            let mut resolver = Resolver::new(&project, update).unwrap();
            let located = resolver.locate("lua", "5.4").unwrap();
            resolver.save().unwrap();
            located
                .recipe
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };

        publish("5.4.7");
        assert_eq!(version_of(None), "5.4.7");
        assert!(fs::read_to_string(project.join("nomake.lock"))
            .unwrap()
            .contains("5.4.7"));
        publish("5.4.8");
        assert_eq!(version_of(None), "5.4.7");
        assert_eq!(version_of(Some(&[])), "5.4.8");
        assert!(fs::read_to_string(project.join("nomake.lock"))
            .unwrap()
            .contains("5.4.8"));

        fs::write(project.join("nomake.toml"), "[package]\nname = \"app\"\n").unwrap();
        let run = |args: &[&str]| {
            command(
                &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
                &project,
            )
        };
        run(&["add", "lua"]).unwrap();
        assert!(fs::read_to_string(project.join("nomake.toml"))
            .unwrap()
            .contains("lua = { version = \"5.4\" }"));
        assert!(run(&["add", "lua"]).is_err() && run(&["add", "nope"]).is_err());
        run(&["list"]).unwrap();
        run(&["search", "lu"]).unwrap();
        run(&["info", "lua"]).unwrap();
        run(&["update"]).unwrap();
        run(&["cache", "clean"]).unwrap();
        assert!(!root.join("home").join("src").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
