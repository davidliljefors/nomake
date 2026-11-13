//! `[workspace]`: one nomake.toml at the top that builds, runs, cleans and indexes a set of packages.
//! All members load into one package set and build as one graph into the root's build/, so a shared
//! dependency compiles once.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::commands::{self, BuildOpts};
use crate::config::{Kind, Manifest, Os, TestConfig, WorkspaceDef};
use crate::testrun::{self, Test};
use crate::toolchain::Arch;
use crate::{vs, Result};

/// Lenient on purpose: ordinary manifests also pass through here, and only `[workspace]` matters.
#[derive(Deserialize)]
struct RootFile {
    workspace: Option<WorkspaceDef>,
    package: Option<toml::Value>,
}

pub struct Member {
    pub dir: PathBuf,
    pub name: String,
    pub kind: Kind,
    /// Set for packages of type `test`.
    pub test: Option<TestConfig>,
}

pub struct Root {
    pub dir: PathBuf,
    pub members: Vec<Member>,
}

/// The workspace rooted at `dir`, if its nomake.toml has a `[workspace]` table.
pub fn find(dir: &Path, os: Os) -> Result<Option<Root>> {
    let Ok(text) = fs::read_to_string(dir.join("nomake.toml")) else {
        return Ok(None);
    };
    let Ok(file) = toml::from_str::<RootFile>(&text) else {
        return Ok(None);
    };
    let Some(def) = file.workspace else {
        return Ok(None);
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if file.package.is_some() {
        dirs.push(dir.to_path_buf());
    }
    for pattern in &def.members {
        for member in expand(dir, pattern)? {
            if !dirs.contains(&member) {
                dirs.push(member);
            }
        }
    }
    let mut members: Vec<Member> = Vec::new();
    for member in dirs {
        let manifest = Manifest::load_resolved(&member.join("nomake.toml"), os)?;
        let name = manifest.package.name.clone();
        if members.iter().any(|other| other.name == name) {
            bail!("workspace has two packages named '{name}'");
        }
        members.push(Member {
            dir: member,
            name,
            kind: manifest.package.kind(),
            test: manifest.package.is_test().then(|| manifest.test.clone()),
        });
    }
    if members.is_empty() {
        bail!("[workspace] in {} lists no members", dir.display());
    }
    Ok(Some(Root {
        dir: dir.to_path_buf(),
        members,
    }))
}

fn expand(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern = pattern.replace('\\', "/");
    let Some(parent) = pattern.strip_suffix("/*") else {
        let dir = root.join(pattern.trim_end_matches('/'));
        if !dir.join("nomake.toml").is_file() {
            bail!("workspace member '{pattern}' has no nomake.toml");
        }
        return Ok(vec![dir]);
    };
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join(parent)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("nomake.toml").is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Members named by `names` (package name or folder name), or all of them when none are given.
fn select<'a>(root: &'a Root, names: &[String]) -> Result<Vec<&'a Member>> {
    if names.is_empty() {
        return Ok(root.members.iter().collect());
    }
    let mut chosen = Vec::new();
    for name in names {
        let found = root.members.iter().find(|member| {
            member.name == *name
                || member
                    .dir
                    .file_name()
                    .is_some_and(|dir| dir.to_string_lossy() == name.as_str())
        });
        match found {
            Some(member) => chosen.push(member),
            None => {
                let known: Vec<&str> = root.members.iter().map(|m| m.name.as_str()).collect();
                bail!(
                    "no workspace member '{name}' (members: {})",
                    known.join(", ")
                );
            }
        }
    }
    Ok(chosen)
}

fn dirs(members: &[&Member]) -> Vec<PathBuf> {
    members.iter().map(|member| member.dir.clone()).collect()
}

/// One build of the chosen members (all by default, except tests): a single graph, so shared dependencies compile once.
pub fn build(root: &Root, names: &[String], mut opts: BuildOpts) -> Result<i32> {
    let mut members = select(root, names)?;
    if names.is_empty() {
        members.retain(|member| member.test.is_none());
    }
    opts.refresh_compdb = names.is_empty();
    opts.roots = dirs(&members);
    Ok(if commands::build(&opts)?.ok { 0 } else { 1 })
}

/// Builds and runs the test members named by `names`, or every test member when none are given.
pub fn test(root: &Root, names: &[String], opts: BuildOpts, extra: &[String]) -> Result<i32> {
    let mut tests = Vec::new();
    for member in select(root, names)? {
        match &member.test {
            Some(config) => tests.push(Test {
                name: member.name.clone(),
                dir: member.dir.clone(),
                config: config.clone(),
            }),
            None if !names.is_empty() => {
                bail!("'{}' is not a test (set type = \"test\")", member.name)
            }
            None => {}
        }
    }
    testrun::run(opts, &tests, extra)
}

pub fn run(root: &Root, names: &[String], mut opts: BuildOpts, args: &[String]) -> Result<i32> {
    let programs: Vec<&Member> = select(root, names)?
        .into_iter()
        .filter(|member| {
            member.kind == Kind::Executable && (member.test.is_none() || !names.is_empty())
        })
        .collect();
    let Some(member) = programs.first() else {
        bail!("no executable to run in this workspace");
    };
    opts.refresh_compdb = false;
    opts.roots = vec![member.dir.clone()];
    commands::run(&opts, args, Some(&member.dir))
}

pub fn compdb(root: &Root, arch: Arch, release: bool) -> Result<()> {
    let all: Vec<&Member> = root.members.iter().collect();
    commands::compdb(arch, release, &dirs(&all))
}

pub fn solution(root: &Root, archs: &[Arch]) -> Result<()> {
    vs::generate_for(
        &root.dir,
        &dirs(&root.members.iter().collect::<Vec<_>>()),
        archs,
    )
}
