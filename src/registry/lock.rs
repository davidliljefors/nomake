//! `nomake.lock`: the version and source commit of every registry dependency the root project uses.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use super::version;
use crate::Result;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Locked {
    pub name: String,
    pub version: String,
    pub rev: String,
}

#[derive(Serialize, Deserialize, Default)]
struct LockFile {
    #[serde(default)]
    dependency: Vec<Locked>,
}

pub struct Lock {
    file: PathBuf,
    /// What is on disk; used to report what changed.
    before: Vec<Locked>,
    /// What lookups see: `before`, minus forgotten dependencies, plus new ones.
    entries: Vec<Locked>,
    /// What this build used; only these are written back.
    used: Vec<Locked>,
}

impl Lock {
    pub fn load(root: &Path) -> Result<Lock> {
        let file = root.join("nomake.lock");
        let before = match fs::read_to_string(&file) {
            Ok(text) => {
                toml::from_str::<LockFile>(&text)
                    .map_err(|e| format!("{}: {e}", file.display()))?
                    .dependency
            }
            Err(_) => Vec::new(),
        };
        Ok(Lock {
            file,
            entries: before.clone(),
            before,
            used: Vec::new(),
        })
    }

    /// What this lock records, as read from disk.
    pub fn on_disk(&self) -> &[Locked] {
        &self.before
    }

    /// The locked entry for `name`, if its version satisfies `requirement`.
    pub fn find(&self, name: &str, requirement: &str) -> Option<&Locked> {
        self.entries
            .iter()
            .find(|entry| entry.name == name && version::matches(&entry.version, requirement))
    }

    /// Drops the named dependencies so they resolve afresh; no names means all of them.
    pub fn forget(&mut self, names: &[String]) {
        self.entries
            .retain(|entry| !names.is_empty() && !names.contains(&entry.name));
    }

    /// Marks `entry` as used by this build, adding it if it is new.
    pub fn record(&mut self, entry: Locked) {
        if !self.entries.contains(&entry) {
            self.entries.push(entry.clone());
        }
        if !self.used.contains(&entry) {
            self.used.push(entry);
        }
    }

    /// Writes the file when it changed, and reports what moved.
    pub fn save(&self) -> Result<()> {
        if self.used.is_empty() {
            if self.file.is_file() {
                fs::remove_file(&self.file)?;
            }
            return Ok(());
        }
        let mut dependency = self.used.clone();
        dependency.sort_by(|left, right| left.name.cmp(&right.name));
        for entry in &dependency {
            match self.before.iter().find(|old| old.name == entry.name) {
                Some(old) if old.version != entry.version => println!(
                    "nomake: {} {} -> {}",
                    entry.name, old.version, entry.version
                ),
                Some(_) => {}
                None => println!("nomake: locked {} {}", entry.name, entry.version),
            }
        }
        if dependency != self.before {
            let text = toml::to_string(&LockFile { dependency }).map_err(|e| e.to_string())?;
            fs::write(&self.file, text)?;
        }
        Ok(())
    }
}
