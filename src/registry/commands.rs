//! Registry commands: `add`, `update`, `list`, `search`, `info` and `cache clean`.

use std::fs;
use std::path::Path;

use super::cache_dir;
use super::index::Registry;
use super::lock::Lock;
use super::version;
use crate::config::Manifest;
use crate::init::{Dep, Model};
use crate::Result;

const USAGE: &str = "usage: nomake <add <name>[@version] | update [name...] | list | search [text] | info <name> | cache clean>";

pub fn run(args: &[String], root: &Path) -> Result<()> {
    let rest = &args[args.len().min(1)..];
    match (args.first().map(String::as_str), rest) {
        (Some("add"), [spec]) => add(root, spec),
        (Some("update"), names) => crate::scanner::update(root, names),
        (Some("list"), []) => list(root),
        (Some("search"), text) => search(&text.join(" ")),
        (Some("info"), [name]) => info(name),
        (Some("cache"), [action]) if action == "clean" => clean(),
        _ => bail!("{USAGE}"),
    }
}

/// Adds the library to `nomake.toml` and locks it. Without `@version` the newest version is used, written as
/// a requirement that still allows its patch releases.
fn add(root: &Path, spec: &str) -> Result<()> {
    if !root.join("nomake.toml").is_file() {
        bail!("no nomake.toml here (run `nomake init` first)");
    }
    let (name, requested) = match spec.split_once('@') {
        Some((name, requirement)) => (name, Some(requirement)),
        None => (spec, None),
    };
    let mut registry = Registry::open()?;
    registry.refresh()?;
    let requirement = match requested {
        Some(requirement) => requirement.to_string(),
        None => match registry.versions(name)?.pop() {
            Some(newest) => version::series(&newest),
            None => bail!("'{name}' has no versions"),
        },
    };
    registry.newest(name, &requirement)?;

    let mut model = Model::open(root, None)?;
    if model.deps.iter().any(|dep| dep.key == name) {
        bail!("'{name}' is already a dependency");
    }
    model.deps.push(Dep {
        key: name.into(),
        path: String::new(),
        version: Some(requirement.clone()),
        on: true,
        configured: true,
        public: false,
        features: Vec::new(),
        default_features: true,
        link: None,
    });
    model.write()?;
    println!("nomake: added {name} = {{ version = \"{requirement}\" }}");
    // Resolving the project locks the new dependency and fetches its sources.
    crate::scanner::load(root, crate::config::Os::host()).map(|_| ())
}

fn list(root: &Path) -> Result<()> {
    let lock = Lock::load(root)?;
    if lock.on_disk().is_empty() {
        println!("no registry dependencies locked");
    }
    for entry in lock.on_disk() {
        println!(
            "{} {}  ({})",
            entry.name,
            entry.version,
            &entry.rev[..entry.rev.len().min(10)]
        );
    }
    Ok(())
}

fn search(text: &str) -> Result<()> {
    let mut registry = Registry::open()?;
    registry.refresh()?;
    let text = text.to_lowercase();
    for name in registry
        .names()?
        .into_iter()
        .filter(|name| name.to_lowercase().contains(&text))
    {
        let versions = registry.versions(&name)?;
        println!(
            "{name} {}",
            versions.last().map(String::as_str).unwrap_or("-")
        );
    }
    Ok(())
}

fn info(name: &str) -> Result<()> {
    let mut registry = Registry::open()?;
    registry.refresh()?;
    let mut versions = registry.versions(name)?;
    versions.reverse();
    let Some(newest) = versions.first() else {
        bail!("'{name}' has no versions")
    };
    let newest = registry.exact(name, newest)?;
    let manifest = Manifest::load(&newest.path)?;
    println!("{name} {}", newest.version);
    println!("versions: {}", versions.join(", "));
    if let Some(source) = &manifest.source {
        println!("source:   {}", source.git);
    }
    let defaults = &manifest.features.default;
    let features: Vec<String> = manifest
        .feature
        .keys()
        .map(|feature| {
            if defaults.contains(feature) {
                format!("{feature} (default)")
            } else {
                feature.clone()
            }
        })
        .collect();
    if !features.is_empty() {
        println!("features: {}", features.join(", "));
    }
    Ok(())
}

/// Deletes the shared source cache; the next build fetches what it needs again.
fn clean() -> Result<()> {
    let src = cache_dir()?.join("src");
    if src.is_dir() {
        fs::remove_dir_all(&src)?;
    }
    println!("nomake: removed {}", src.display());
    Ok(())
}
