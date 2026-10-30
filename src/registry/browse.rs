//! What the `configure` interface shows of the registry: the libraries, their versions and features.

use super::index::Registry;
use super::version;
use crate::config::Manifest;
use crate::Result;

pub struct Entry {
    pub name: String,
    /// Newest first.
    pub versions: Vec<String>,
}

impl Entry {
    /// `5.4`, `5.3`, ...: what a dependency can ask for, newest first.
    pub fn series(&self) -> Vec<String> {
        version::series_of(&self.versions)
    }
}

pub struct Details {
    pub version: String,
    pub git: String,
    /// Feature name and whether it is on by default.
    pub features: Vec<(String, bool)>,
}

/// Every library in the newest registry.
pub fn catalog() -> Result<Vec<Entry>> {
    let mut registry = Registry::open()?;
    registry.refresh()?;
    let mut entries = Vec::new();
    for name in registry.names()? {
        let mut versions = registry.versions(&name)?;
        versions.reverse();
        if !versions.is_empty() {
            entries.push(Entry { name, versions });
        }
    }
    Ok(entries)
}

/// The recipe `requirement` resolves to in the registry as it is on disk.
pub fn details(name: &str, requirement: &str) -> Result<Details> {
    let recipe = Registry::open()?.newest(name, requirement)?;
    let manifest = Manifest::load(&recipe.path)?;
    let features = manifest
        .feature
        .keys()
        .map(|feature| (feature.clone(), manifest.features.default.contains(feature)))
        .collect();
    Ok(Details {
        version: recipe.version,
        git: manifest.source.map(|source| source.git).unwrap_or_default(),
        features,
    })
}
