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
pub(crate) use version::matches as version_matches;

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
pub(crate) fn cache_dir() -> Result<PathBuf> {
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
    /// Located jobs held back until needed (`fetch_for_paths` for nested `path` manifests, `fetch_dirs` for active packages).
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
                "recipe for {name} {} changed since locking (run `nomake update {name}`)",
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

    /// Fetches only queued packages whose cached source tree is needed to read a nested `path` manifest in `level`.
    pub fn fetch_for_paths(&mut self, level: &[(PathBuf, PathBuf)]) -> Result<()> {
        let missing: Vec<&Path> = level
            .iter()
            .filter_map(|(_, file)| (!file.is_file()).then_some(file.as_path()))
            .collect();
        if missing.is_empty() || self.jobs.is_empty() {
            return Ok(());
        }
        let mut needed = Vec::new();
        let mut kept = Vec::new();
        for job in std::mem::take(&mut self.jobs) {
            if missing.iter().any(|file| file.starts_with(&job.dest)) {
                needed.push(job);
            } else {
                kept.push(job);
            }
        }
        self.jobs = kept;
        if needed.is_empty() {
            return Ok(());
        }
        fetch::fetch_all(&needed)
    }

    /// Fetches queued packages whose destination directory is in `dirs`.
    pub fn fetch_dirs(&mut self, dirs: &std::collections::HashSet<PathBuf>) -> Result<()> {
        let mut needed = Vec::new();
        let mut kept = Vec::new();
        for job in std::mem::take(&mut self.jobs) {
            if dirs.contains(&job.dest) {
                needed.push(job);
            } else {
                kept.push(job);
            }
        }
        self.jobs = kept;
        if needed.is_empty() {
            return Ok(());
        }
        fetch::fetch_all(&needed)
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
    use crate::config::Os;
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
                &[],
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

    #[test]
    fn switching_targets_keeps_every_targets_lock_entries() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = env::temp_dir().join(format!("nomake_lock_targets_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repo, rev) = repo_with_commit(&root);
        let registry = root.join("registry");
        let publish = |name: &str, version: &str| {
            let dir = registry.join(name).join(version);
            fs::create_dir_all(&dir).unwrap();
            let git = repo.display().to_string().replace('\\', "/");
            fs::write(
                dir.join("nomake.toml"),
                format!(
                    "[package]\nname = \"{name}\"\n[source]\ngit = \"{git}\"\nrev = \"{rev}\"\n"
                ),
            )
            .unwrap();
        };
        env::set_var("NOMAKE_HOME", root.join("home"));
        env::set_var("NOMAKE_REGISTRY", &registry);
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("nomake.toml"),
            "[package]\nname = \"app\"\n[dependencies]\nbase = { version = \"1\" }\n\
             [target.emscripten.dependencies]\nweb = { version = \"5.4\" }\n",
        )
        .unwrap();
        publish("base", "1.0.0");
        publish("web", "5.4.7");
        let lock = || fs::read_to_string(project.join("nomake.lock")).unwrap();

        crate::scanner::load(&project, Os::Linux).unwrap();
        assert!(lock().contains("base") && lock().contains("5.4.7"));

        publish("web", "5.4.8");
        crate::scanner::load(&project, Os::Emscripten).unwrap();
        crate::scanner::load(&project, Os::Linux).unwrap();
        assert!(lock().contains("5.4.7") && !lock().contains("5.4.8"));

        crate::scanner::update(&project, std::slice::from_ref(&project), true, &[]).unwrap();
        assert!(lock().contains("5.4.8") && lock().contains("base"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_covers_all_targets_and_fetches_only_nested_path_sources_on_demand() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = env::temp_dir().join(format!("nomake_lock_other_path_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let make_repo = |name: &str, files: &[(&str, &str)]| {
            let repo = root.join(format!("{name}-source"));
            fs::create_dir_all(&repo).unwrap();
            for (file, text) in files {
                let path = repo.join(file);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
            let git = |args: &[&str]| {
                run(Command::new("git").args(args).current_dir(&repo)).unwrap();
            };
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
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap();
            (
                repo,
                String::from_utf8(output.stdout).unwrap().trim().to_string(),
            )
        };
        let (web_repo, web_rev) = make_repo(
            "web",
            &[
                (
                    "nomake.toml",
                    "[package]\nname = \"web\"\ntype = \"static\"\n[dependencies]\nlocal = { path = \"local\" }\n",
                ),
                (
                    "local/nomake.toml",
                    "[package]\nname = \"local\"\ntype = \"static\"\n[dependencies]\nnested = { version = \"1\" }\n",
                ),
            ],
        );
        let (leaf_repo, leaf_rev) = make_repo(
            "leaf",
            &[(
                "nomake.toml",
                "[package]\nname = \"leaf\"\ntype = \"static\"\n",
            )],
        );

        env::set_var("NOMAKE_HOME", root.join("home"));
        let registry = root.join("registry");
        env::set_var("NOMAKE_REGISTRY", &registry);
        let publish = |name: &str, repo: &Path, rev: &str, dependencies: &str| {
            let dir = registry.join(name).join("1.0.0");
            fs::create_dir_all(&dir).unwrap();
            let git = repo.display().to_string().replace('\\', "/");
            fs::write(
                dir.join("nomake.toml"),
                format!(
                    "[package]\nname = \"{name}\"\ntype = \"static\"\n{dependencies}[source]\ngit = \"{git}\"\nrev = \"{rev}\"\n"
                ),
            )
            .unwrap();
        };
        publish("winonly", &leaf_repo, &leaf_rev, "");
        publish("maconly", &leaf_repo, &leaf_rev, "");
        publish(
            "web",
            &web_repo,
            &web_rev,
            "[dependencies]\nlocal = { path = \"local\" }\n",
        );
        publish("nested", &leaf_repo, &leaf_rev, "");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("nomake.toml"),
            "[package]\nname = \"app\"\n\
             [target.windows.dependencies]\nwinonly = { version = \"1\" }\n\
             [target.macos.dependencies]\nmaconly = { version = \"1\" }\n\
             [target.emscripten.dependencies]\nweb = { version = \"1\" }\n",
        )
        .unwrap();
        let lock_file = project.join("nomake.lock");
        let sources = root.join("home/src");

        crate::scanner::load(&project, Os::Linux).unwrap();
        let lock = fs::read_to_string(&lock_file).unwrap();
        for expected in ["winonly", "maconly", "web", "nested"] {
            assert!(lock.contains(expected), "missing {expected} in:\n{lock}");
        }
        let fetched: Vec<String> = fs::read_dir(&sources)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            fetched.len(),
            1,
            "only `web` needed a source fetch for its nested path manifest, got {fetched:?}"
        );
        assert!(fetched[0].starts_with("web-1.0.0-"));

        publish(
            "web",
            &web_repo,
            &web_rev,
            "[dependencies]\nbroken = { path = \"missing\" }\n",
        );
        let error = crate::scanner::update(&project, std::slice::from_ref(&project), true, &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("missing") && error.contains("not found"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_workspace_loads_keep_all_member_locks_and_check_version_pins() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = env::temp_dir().join(format!("nomake_ws_lock_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repo, rev) = repo_with_commit(&root);
        let registry = root.join("registry");
        let publish = |name: &str, version: &str| {
            let dir = registry.join(name).join(version);
            fs::create_dir_all(&dir).unwrap();
            let git = repo.display().to_string().replace('\\', "/");
            fs::write(
                dir.join("nomake.toml"),
                format!(
                    "[package]\nname = \"{name}\"\ntype = \"static\"\n[source]\ngit = \"{git}\"\nrev = \"{rev}\"\n"
                ),
            )
            .unwrap();
        };
        env::set_var("NOMAKE_HOME", root.join("home"));
        env::set_var("NOMAKE_REGISTRY", &registry);
        publish("lz4", "1.10.0");
        publish("lz4", "1.10.1");
        publish("xxhash", "0.8.2");
        publish("doctest", "2.4.11");

        let ws_dir = crate::util::absolute(&root.join("ws"));
        let app = ws_dir.join("app");
        let tool = ws_dir.join("tool");
        let suite = ws_dir.join("suite");
        for dir in [&app, &tool, &suite] {
            fs::create_dir_all(dir).unwrap();
        }
        // Compatible member pins: "1" + "1.10.0" -> resolves to 1.10.0 (not 1.10.1).
        fs::write(
            app.join("nomake.toml"),
            "[package]\nname = \"app\"\n[dependencies]\nlz4 = { version = \"1\" }\nxxhash = { version = \"0.8\" }\n",
        )
        .unwrap();
        fs::write(
            tool.join("nomake.toml"),
            "[package]\nname = \"tool\"\n[dependencies]\nlz4 = { version = \"1.10.0\" }\n",
        )
        .unwrap();
        fs::write(
            suite.join("nomake.toml"),
            "[package]\nname = \"suite\"\ntype = \"test\"\n[dependencies]\ndoctest = { version = \"2.4\" }\n",
        )
        .unwrap();

        let all = [app.clone(), tool.clone(), suite.clone()];
        // Load ONLY `suite` (`nomake test` equivalent): all three registry deps must be locked,
        // `lz4` pinned to 1.10.0, and only `doctest` fetched to disk.
        let loaded = crate::scanner::load_project(
            &ws_dir,
            &all,
            std::slice::from_ref(&suite),
            false,
            None,
            Os::host(),
        )
        .unwrap();
        assert_eq!(
            loaded
                .pkgs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["doctest", "suite"]
        );
        let lock = fs::read_to_string(ws_dir.join("nomake.lock")).unwrap();
        assert!(
            lock.contains("name = \"lz4\"\nversion = \"1.10.0\""),
            "{lock}"
        );
        assert!(lock.contains("name = \"xxhash\""), "{lock}");
        assert!(lock.contains("name = \"doctest\""), "{lock}");
        let fetched: Vec<String> = fs::read_dir(root.join("home/src"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(fetched.len(), 1);
        assert!(fetched[0].starts_with("doctest-2.4.11-"));

        // Incompatible member version pins ("1.9" vs "1.10") must fail naming both members.
        publish("lz4", "1.9.4");
        fs::write(
            app.join("nomake.toml"),
            "[package]\nname = \"app\"\n[dependencies]\nlz4 = { version = \"1.9\" }\n",
        )
        .unwrap();
        let err = crate::scanner::load_project(&ws_dir, &all, &all, false, None, Os::host())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("conflicting versions for 'lz4'")
                && err.contains("'app'")
                && err.contains("'tool'"),
            "{err}"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
