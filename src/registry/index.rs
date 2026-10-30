//! The registry: a git repository of recipes at `<name>/<version>/nomake.toml`, cloned into the cache.
//! Published versions stay in the tree, so the newest matching one is found by listing a directory.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{cache_dir, run, version};
use crate::Result;

/// The registry repository. Set `NOMAKE_REGISTRY` to use a local folder instead.
const REGISTRY_URL: &str = "https://github.com/davidliljefors/nomake-registry.git";

pub struct Recipe {
    pub version: String,
    pub path: PathBuf,
}

pub struct Registry {
    dir: PathBuf,
    /// A folder given by `NOMAKE_REGISTRY`; never cloned or refreshed.
    local: bool,
    refreshed: bool,
}

impl Registry {
    pub fn open() -> Result<Registry> {
        Ok(match env::var_os("NOMAKE_REGISTRY") {
            Some(dir) => Registry {
                dir: dir.into(),
                local: true,
                refreshed: false,
            },
            None => Registry {
                dir: cache_dir()?.join("registry"),
                local: false,
                refreshed: false,
            },
        })
    }

    /// The highest version of `name` matching `requirement`.
    pub fn newest(&mut self, name: &str, requirement: &str) -> Result<Recipe> {
        self.lookup(|dir| newest(dir, name, requirement))
    }

    /// Exactly `version` of `name`.
    pub fn exact(&mut self, name: &str, version: &str) -> Result<Recipe> {
        self.lookup(|dir| exact(dir, name, version))
    }

    /// Every published version of `name`, lowest first.
    pub fn versions(&mut self, name: &str) -> Result<Vec<String>> {
        self.ensure()?;
        versions_in(&self.dir, name)
    }

    /// Every library in the registry, sorted.
    pub fn names(&mut self) -> Result<Vec<String>> {
        self.ensure()?;
        let entries = fs::read_dir(&self.dir)
            .map_err(|e| format!("cannot read the registry at {}: {e}", self.dir.display()))?;
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        Ok(names)
    }

    fn ensure(&mut self) -> Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        self.refresh()
    }

    /// Clones the newest registry state into the cache, once per run.
    pub fn refresh(&mut self) -> Result<()> {
        if self.local || self.refreshed {
            return Ok(());
        }
        self.refreshed = true;
        let temp = self.dir.with_extension("tmp");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(self.dir.parent().unwrap())?;
        println!("nomake: fetching the registry");
        run(Command::new("git")
            .args(["clone", "-q", "--depth", "1", REGISTRY_URL])
            .arg(&temp))?;
        let _ = fs::remove_dir_all(temp.join(".git"));
        let _ = fs::remove_dir_all(&self.dir);
        fs::rename(&temp, &self.dir)?;
        Ok(())
    }

    /// Looks in the cached registry first. A miss may only mean the cache is old, so it refreshes once and retries.
    fn lookup(&mut self, find: impl Fn(&Path) -> Result<Recipe>) -> Result<Recipe> {
        self.ensure()?;
        match find(&self.dir) {
            Err(_) if !self.local && !self.refreshed => {
                self.refresh()?;
                find(&self.dir)
            }
            found => found,
        }
    }
}

fn safe_component(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

fn check_names(name: &str, version: &str) -> Result<()> {
    if !safe_component(name) || !safe_component(version) {
        bail!("invalid library name or version: '{name}' '{version}'");
    }
    Ok(())
}

fn exact(registry: &Path, name: &str, version: &str) -> Result<Recipe> {
    check_names(name, version)?;
    let path = registry.join(name).join(version).join("nomake.toml");
    if !path.is_file() {
        bail!(
            "'{name}' has no version {version} in the registry at {}",
            registry.display()
        );
    }
    Ok(Recipe {
        version: version.into(),
        path,
    })
}

fn newest(registry: &Path, name: &str, requirement: &str) -> Result<Recipe> {
    check_names(name, requirement)?;
    let dir = registry.join(name);
    let mut versions = versions_in(registry, name)?;
    versions.retain(|found| version::matches(found, requirement));
    match versions.pop() {
        Some(version) => Ok(Recipe {
            path: dir.join(&version).join("nomake.toml"),
            version,
        }),
        None => bail!(
            "'{name}' has no version matching '{requirement}' in {}",
            dir.display()
        ),
    }
}

/// Every published version of `name`, lowest first.
fn versions_in(registry: &Path, name: &str) -> Result<Vec<String>> {
    check_names(name, "0")?;
    let Ok(entries) = fs::read_dir(registry.join(name)) else {
        bail!(
            "no library named '{name}' in the registry at {}",
            registry.display()
        );
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().join("nomake.toml").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    versions.sort_by(|left, right| version::compare(left, right));
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_matching_version_wins() {
        let registry = env::temp_dir().join(format!("nomake_registry_{}", std::process::id()));
        for version in ["2.9.1", "2.13.0", "2.13.2", "2.130.0", "3.0.0"] {
            let dir = registry.join("freetype").join(version);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("nomake.toml"), "").unwrap();
        }
        let pick = |req: &str| {
            newest(&registry, "freetype", req)
                .map(|recipe| recipe.version)
                .unwrap_or_default()
        };
        assert_eq!(pick("2.13"), "2.13.2");
        assert_eq!(pick("2"), "2.130.0");
        assert_eq!(pick("2.9.1"), "2.9.1");
        assert_eq!(pick("4"), "");
        assert!(newest(&registry, "../x", "1").is_err());
        fs::remove_dir_all(registry).unwrap();
    }
}
