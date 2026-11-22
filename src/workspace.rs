//! Project and `[workspace]` discovery: every project is a workspace of one or more packages sharing
//! `root_dir`'s `build/`, `nomake.lock`, `compile_commands.json` and `build/vs/`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{Kind, Manifest, Os, TestConfig, Toolchain, WorkspaceDef};
use crate::util::{self, absolute, rel_str};
use crate::Result;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceManifest {
    workspace: WorkspaceDef,
    #[serde(default)]
    toolchain: Toolchain,
}

enum Parsed {
    Package(Box<Manifest>),
    Workspace(WorkspaceManifest),
}

pub struct Member {
    pub dir: PathBuf,
    pub name: String,
    pub kind: Kind,
    /// Set for packages of type `test`.
    pub test: Option<TestConfig>,
}

pub struct Project {
    pub root_dir: PathBuf,
    pub is_workspace: bool,
    pub has_root_package: bool,
    pub toolchain: Toolchain,
    pub members: Vec<Member>,
    pub default_member: Option<usize>,
    pub primary: Option<usize>,
}

fn parse_file(path: &Path, os: Os) -> Result<Parsed> {
    let text = fs::read_to_string(path)?;
    let table: toml::Table =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if table.contains_key("package") {
        return Ok(Parsed::Package(Box::new(Manifest::load_resolved(
            path, os,
        )?)));
    }
    if table.contains_key("workspace") {
        for (key, value) in &table {
            if key != "workspace" && key != "toolchain" {
                let label = if value.is_table() {
                    format!("[{key}]")
                } else {
                    format!("'{key}'")
                };
                bail!(
                    "{}: workspace nomake.toml has no [package]; put {label} in a member's nomake.toml (only [workspace] and [toolchain] are allowed here)",
                    path.display()
                );
            }
        }
        let ws: WorkspaceManifest =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(Parsed::Workspace(ws));
    }
    bail!(
        "{}: expected a [package] or [workspace] table",
        path.display()
    );
}

fn member_from(dir: PathBuf, manifest: &Manifest) -> Member {
    Member {
        dir,
        name: manifest.package.name.clone(),
        kind: manifest.package.kind(),
        test: manifest.package.is_test().then(|| manifest.test.clone()),
    }
}

pub(crate) fn expand(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern = pattern.replace('\\', "/");
    let pattern = pattern.trim_end_matches('/');
    if !pattern.contains(['*', '?']) {
        let dir = absolute(&root.join(pattern));
        if !dir.join("nomake.toml").is_file() {
            bail!("workspace member '{pattern}' has no nomake.toml");
        }
        return Ok(vec![dir]);
    }
    let allow_deps = pattern == "deps" || pattern.starts_with("deps/");
    let mut found = Vec::new();
    for manifest in util::glob(root, &format!("{pattern}/nomake.toml")) {
        let Some(parent) = manifest.parent() else {
            continue;
        };
        let dir = absolute(parent);
        if dir == root {
            continue;
        }
        if !allow_deps
            && dir
                .strip_prefix(root)
                .ok()
                .and_then(|rel| rel.components().next())
                .is_some_and(|comp| comp.as_os_str() == "deps" || comp.as_os_str() == "build")
        {
            continue;
        }
        found.push(dir);
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn collect_member_dirs(
    root_dir: &Path,
    has_root_package: bool,
    def: &WorkspaceDef,
) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    if has_root_package {
        dirs.push(root_dir.to_path_buf());
    }
    for pattern in &def.members {
        for member in expand(root_dir, pattern)? {
            if !dirs.contains(&member) {
                dirs.push(member);
            }
        }
    }
    Ok(dirs)
}

fn load_workspace(
    root_dir: PathBuf,
    root_pkg: Option<Manifest>,
    def: WorkspaceDef,
    toolchain: Toolchain,
    default_dir: Option<&Path>,
    os: Os,
) -> Result<Project> {
    let has_root_package = root_pkg.is_some();
    let dirs = collect_member_dirs(&root_dir, has_root_package, &def)?;
    let root_toml = root_dir.join("nomake.toml");
    let mut members: Vec<Member> = Vec::new();
    for dir in dirs {
        let manifest = match (&root_pkg, dir == root_dir) {
            (Some(pkg), true) => pkg.clone_for_member(),
            _ => match parse_file(&dir.join("nomake.toml"), os)? {
                Parsed::Package(pkg) => pkg.clone_for_member(),
                Parsed::Workspace(_) => {
                    let rel = rel_str(&root_dir, &dir);
                    let label = if rel.is_empty() { "." } else { &rel };
                    bail!(
                        "workspace member '{label}' is itself a [workspace]; list its member packages in {}",
                        root_toml.display()
                    );
                }
            },
        };
        if let Some(prev) = members.iter().find(|other| other.name == manifest.0) {
            bail!(
                "two packages are named '{}': {} and {}",
                manifest.0,
                prev.dir.display(),
                dir.display()
            );
        }
        members.push(Member {
            dir,
            name: manifest.0,
            kind: manifest.1,
            test: manifest.2,
        });
    }
    if members.is_empty() {
        bail!("[workspace] in {} lists no members", root_toml.display());
    }
    let primary = match &def.primary {
        Some(name) => {
            let Some(idx) = members.iter().position(|m| m.name == *name) else {
                let known: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
                bail!(
                    "{}: [workspace] primary '{name}' is not a workspace member (members: {})",
                    root_toml.display(),
                    known.join(", ")
                );
            };
            if !members[idx].kind.is_program() {
                bail!(
                    "{}: [workspace] primary '{name}' is a {} library, not an executable",
                    root_toml.display(),
                    members[idx].kind.name()
                );
            }
            Some(idx)
        }
        None => None,
    };
    let default_member =
        default_dir.and_then(|target| members.iter().position(|m| m.dir == target));
    Ok(Project {
        root_dir,
        is_workspace: true,
        has_root_package,
        toolchain,
        members,
        default_member,
        primary,
    })
}

impl Manifest {
    fn clone_for_member(&self) -> (String, Kind, Option<TestConfig>) {
        (
            self.package.name.clone(),
            self.package.kind(),
            self.package.is_test().then(|| self.test.clone()),
        )
    }
}

impl Project {
    /// Discovers the enclosing workspace or single-package project for `cwd`.
    pub fn discover(cwd: &Path, os: Os) -> Result<Project> {
        let cwd = absolute(cwd);
        let Some(near) = cwd
            .ancestors()
            .find(|dir| dir.join("nomake.toml").is_file())
            .map(Path::to_path_buf)
        else {
            bail!("no nomake.toml in {} (run `nomake init`)", cwd.display());
        };
        match parse_file(&near.join("nomake.toml"), os)? {
            Parsed::Workspace(ws) => {
                load_workspace(near, None, ws.workspace, ws.toolchain, None, os)
            }
            Parsed::Package(mut pkg) => {
                if let Some(def) = pkg.workspace.take() {
                    let toolchain = pkg.toolchain.clone();
                    return load_workspace(near, Some(*pkg), def, toolchain, None, os);
                }
                for anc in near.parent().into_iter().flat_map(Path::ancestors) {
                    let anc_toml = anc.join("nomake.toml");
                    if !anc_toml.is_file() {
                        continue;
                    }
                    let (anc_pkg, def, toolchain) = match parse_file(&anc_toml, os)? {
                        Parsed::Workspace(ws) => (None, ws.workspace, ws.toolchain),
                        Parsed::Package(mut m) => match m.workspace.take() {
                            Some(def) => {
                                let tc = m.toolchain.clone();
                                (Some(*m), def, tc)
                            }
                            None => continue,
                        },
                    };
                    let dirs = collect_member_dirs(anc, anc_pkg.is_some(), &def)?;
                    if dirs.contains(&near) {
                        return load_workspace(
                            anc.to_path_buf(),
                            anc_pkg,
                            def,
                            toolchain,
                            Some(&near),
                            os,
                        );
                    }
                    break;
                }
                let member = member_from(near.clone(), &pkg);
                Ok(Project {
                    root_dir: near,
                    is_workspace: false,
                    has_root_package: true,
                    toolchain: pkg.toolchain,
                    members: vec![member],
                    default_member: Some(0),
                    primary: None,
                })
            }
        }
    }

    /// Resolves `selectors` (package names, paths, or unique folder leaf names) to member indices,
    /// or returns all member indices when `selectors` is empty.
    pub fn select<'a>(&'a self, cwd: &Path, selectors: &[String]) -> Result<Vec<&'a Member>> {
        if selectors.is_empty() {
            return Ok(self.members.iter().collect());
        }
        let items: Vec<(&str, &Path)> = self
            .members
            .iter()
            .map(|m| (m.name.as_str(), m.dir.as_path()))
            .collect();
        let indices = select_items(&items, &self.root_dir, self.is_workspace, cwd, selectors)?;
        Ok(indices.into_iter().map(|i| &self.members[i]).collect())
    }
}

/// Resolves `selectors` against `(name, dir)` pairs, deduplicating while preserving order.
pub(crate) fn select_items(
    items: &[(&str, &Path)],
    root_dir: &Path,
    is_workspace: bool,
    cwd: &Path,
    selectors: &[String],
) -> Result<Vec<usize>> {
    let cwd = absolute(cwd);
    let mut chosen = Vec::new();
    for sel in selectors {
        let idx = if let Some(pos) = items.iter().position(|(name, _)| *name == sel) {
            pos
        } else {
            let from_cwd = absolute(&cwd.join(sel));
            let from_root = absolute(&root_dir.join(sel));
            if let Some(pos) = items
                .iter()
                .position(|(_, dir)| *dir == from_cwd)
                .or_else(|| items.iter().position(|(_, dir)| *dir == from_root))
            {
                pos
            } else {
                let by_leaf: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, dir))| {
                        dir.file_name()
                            .is_some_and(|leaf| leaf.to_string_lossy() == sel.as_str())
                    })
                    .map(|(i, _)| i)
                    .collect();
                match by_leaf.as_slice() {
                    [only] => *only,
                    [_, _, ..] => {
                        let paths: Vec<String> = by_leaf
                            .iter()
                            .map(|&i| rel_str(root_dir, items[i].1))
                            .collect();
                        bail!(
                            "ambiguous package selector '{sel}' matches multiple folders ({}); pass a package name or path",
                            paths.join(", ")
                        );
                    }
                    [] => {
                        if is_workspace {
                            let known: Vec<&str> = items.iter().map(|(name, _)| *name).collect();
                            bail!(
                                "no workspace member '{sel}' (members: {})",
                                known.join(", ")
                            );
                        }
                        bail!("unknown package '{sel}' (package is '{}')", items[0].0);
                    }
                }
            }
        };
        if !chosen.contains(&idx) {
            chosen.push(idx);
        }
    }
    Ok(chosen)
}

impl Project {
    pub fn member_dirs(&self) -> Vec<PathBuf> {
        self.members.iter().map(|m| m.dir.clone()).collect()
    }

    pub fn effective_toolchain(&self, pkg_tc: &Toolchain) -> Toolchain {
        Toolchain {
            cxx: pkg_tc.cxx.clone().or_else(|| self.toolchain.cxx.clone()),
            ar: pkg_tc.ar.clone().or_else(|| self.toolchain.ar.clone()),
        }
    }
}

/// Finds the nearest enclosing `[workspace]` for `target_dir` (skipping `deps/`, `build/`, and hidden folders)
/// and reports `(workspace_dir, already_matched_by_members)`.
pub(crate) fn find_enclosing_workspace(target_dir: &Path) -> Option<(PathBuf, bool)> {
    let target_dir = absolute(target_dir);
    let mut cur = target_dir.parent();
    while let Some(dir) = cur {
        let rel_path = target_dir.strip_prefix(dir).ok()?;
        if rel_path.components().any(|comp| {
            let name = comp.as_os_str().to_string_lossy();
            name == "deps" || name == "build" || name.starts_with('.')
        }) {
            return None;
        }
        let toml_path = dir.join("nomake.toml");
        if let Ok(text) = fs::read_to_string(&toml_path) {
            if let Ok(doc) = text.parse::<toml_edit::DocumentMut>() {
                if let Some(ws) = doc.get("workspace").and_then(|item| item.as_table_like()) {
                    let rel = rel_str(dir, &target_dir);
                    let covered = ws
                        .get("members")
                        .and_then(|item| item.as_array())
                        .is_some_and(|arr| {
                            arr.iter().filter_map(|v| v.as_str()).any(|entry| {
                                let pat = entry.replace('\\', "/");
                                let pat = pat.trim_end_matches('/');
                                if pat.contains('*') || pat.contains('?') {
                                    util::matches(pat, &rel)
                                } else {
                                    absolute(&dir.join(pat)) == target_dir
                                }
                            })
                        });
                    return Some((dir.to_path_buf(), covered));
                }
            }
        }
        cur = dir.parent();
    }
    None
}

/// Appends `rel` to `[workspace] members` in `ws_toml` via `toml_edit`, preserving formatting and comments.
pub(crate) fn add_workspace_member(ws_toml: &Path, rel: &str) -> Result<()> {
    let text = fs::read_to_string(ws_toml)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let ws = doc
        .entry("workspace")
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .ok_or("[workspace] is not a table")?;
    let members = ws
        .entry("members")
        .or_insert_with(|| toml_edit::value(toml_edit::Array::new()));
    let array = members
        .as_array_mut()
        .ok_or("[workspace] members is not an array")?;
    if !array.iter().any(|v| v.as_str() == Some(rel)) {
        array.push(rel);
    }
    fs::write(ws_toml, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nomake_ws_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        absolute(&dir)
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn discovers_pure_hybrid_and_single_projects_and_walks_up_from_members() {
        let root = temp("discover");
        write(
            &root,
            "pure/nomake.toml",
            "[workspace]\nmembers = [\"*\"]\nprimary = \"app\"\n\n[toolchain]\ncxx = \"clang++\"\n",
        );
        write(
            &root,
            "pure/app/nomake.toml",
            "[package]\nname = \"app\"\ntype = \"executable\"\n",
        );
        write(&root, "pure/app/src/main.cpp", "");
        write(
            &root,
            "pure/math/nomake.toml",
            "[package]\nname = \"math\"\ntype = \"static\"\n",
        );
        // Non-member vendored dep and build dir must be skipped by `*` and by walk-up.
        write(
            &root,
            "pure/deps/vendored/nomake.toml",
            "[package]\nname = \"vendored\"\ntype = \"static\"\n",
        );
        write(
            &root,
            "pure/build/stale/nomake.toml",
            "[package]\nname = \"stale\"\n",
        );

        let at_root = Project::discover(&root.join("pure"), Os::host()).unwrap();
        assert!(at_root.is_workspace && !at_root.has_root_package);
        assert_eq!(at_root.root_dir, root.join("pure"));
        assert_eq!(at_root.toolchain.cxx.as_deref(), Some("clang++"));
        let names: Vec<&str> = at_root.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["app", "math"]);
        assert_eq!(at_root.default_member, None);
        assert_eq!(at_root.primary, Some(0));

        let in_member = Project::discover(&root.join("pure/app/src"), Os::host()).unwrap();
        assert!(in_member.is_workspace);
        assert_eq!(in_member.root_dir, root.join("pure"));
        assert_eq!(in_member.default_member, Some(0));

        let vendored = Project::discover(&root.join("pure/deps/vendored"), Os::host()).unwrap();
        assert!(!vendored.is_workspace && vendored.has_root_package);
        assert_eq!(vendored.root_dir, root.join("pure/deps/vendored"));
        assert_eq!(vendored.default_member, Some(0));

        // Hybrid workspace: `[package]` + `[workspace]`.
        write(
            &root,
            "hybrid/nomake.toml",
            "[package]\nname = \"root_app\"\n\n[workspace]\nmembers = [\"sub\"]\n",
        );
        write(&root, "hybrid/src/main.cpp", "");
        write(
            &root,
            "hybrid/sub/nomake.toml",
            "[package]\nname = \"sub\"\ntype = \"static\"\n",
        );
        let hybrid = Project::discover(&root.join("hybrid/src"), Os::host()).unwrap();
        assert!(hybrid.is_workspace && hybrid.has_root_package);
        assert_eq!(hybrid.default_member, None);
        assert_eq!(hybrid.members.len(), 2);

        let hybrid_sub = Project::discover(&root.join("hybrid/sub"), Os::host()).unwrap();
        assert_eq!(hybrid_sub.root_dir, root.join("hybrid"));
        assert_eq!(hybrid_sub.default_member, Some(1));

        // Single package project & its `src/` subdirectory.
        write(&root, "solo/nomake.toml", "[package]\nname = \"solo\"\n");
        fs::create_dir_all(root.join("solo/src")).unwrap();
        let solo = Project::discover(&root.join("solo/src"), Os::host()).unwrap();
        assert!(!solo.is_workspace && solo.has_root_package);
        assert_eq!(solo.root_dir, root.join("solo"));
        assert_eq!(solo.default_member, Some(0));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn globs_path_normalization_and_selectors_work() {
        let root = temp("globs");
        write(
            &root,
            "nomake.toml",
            "[workspace]\nmembers = [\"pkgs/b/../a\", \"pkgs/b\", \"crates/*/tests\"]\n",
        );
        write(&root, "pkgs/a/nomake.toml", "[package]\nname = \"alpha\"\n");
        write(&root, "pkgs/b/nomake.toml", "[package]\nname = \"beta\"\n");
        write(
            &root,
            "crates/core/tests/nomake.toml",
            "[package]\nname = \"core_tests\"\ntype = \"test\"\n",
        );
        write(
            &root,
            "crates/net/tests/nomake.toml",
            "[package]\nname = \"net_tests\"\ntype = \"test\"\n",
        );

        let proj = Project::discover(&root, Os::host()).unwrap();
        let names: Vec<&str> = proj.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "core_tests", "net_tests"]);
        assert_eq!(proj.members[0].dir, root.join("pkgs").join("a"));

        // Tab-completed paths with leading `.\` and trailing `\`, folder leaf name, and deduplication.
        let sel = |cwd: &Path, items: &[&str]| {
            let owned: Vec<String> = items.iter().map(|s| s.to_string()).collect();
            proj.select(cwd, &owned)
                .map(|list| list.into_iter().map(|m| m.name.clone()).collect::<Vec<_>>())
        };
        assert_eq!(
            sel(&root, &[r".\pkgs\a\", "a", "alpha", "crates/core/tests"]).unwrap(),
            ["alpha", "core_tests"]
        );
        assert_eq!(sel(&root.join("pkgs/b"), &["."]).unwrap(), ["beta"]);

        // Ambiguous leaf folder name `tests` must error instead of picking the first.
        let err = sel(&root, &["tests"]).err().unwrap().to_string();
        assert!(err.contains("ambiguous"), "{err}");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_pure_workspace_tables_nested_workspaces_and_bad_primary() {
        let root = temp("errors");

        // Stray `[dependencies]` in a pure workspace root.
        write(
            &root,
            "stray/nomake.toml",
            "[workspace]\nmembers = [\"a\"]\n[dependencies]\nx = { path = \"x\" }\n",
        );
        write(&root, "stray/a/nomake.toml", "[package]\nname = \"a\"\n");
        let err = Project::discover(&root.join("stray"), Os::host())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("[dependencies]") && err.contains("no [package]"),
            "{err}"
        );

        // Syntax error is never swallowed.
        write(&root, "broken/nomake.toml", "[workspace\n");
        assert!(Project::discover(&root.join("broken"), Os::host()).is_err());

        // Nested pure `[workspace]` listed as a member.
        write(
            &root,
            "outer/nomake.toml",
            "[workspace]\nmembers = [\"inner\"]\n",
        );
        write(
            &root,
            "outer/inner/nomake.toml",
            "[workspace]\nmembers = [\"leaf\"]\n",
        );
        let err = Project::discover(&root.join("outer"), Os::host())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("inner") && err.contains("itself a [workspace]"),
            "{err}"
        );

        // `primary` naming a missing member or a static library must fail clearly.
        write(
            &root,
            "bad_primary/nomake.toml",
            "[workspace]\nmembers = [\"lib\"]\nprimary = \"nope\"\n",
        );
        write(
            &root,
            "bad_primary/lib/nomake.toml",
            "[package]\nname = \"lib\"\ntype = \"static\"\n",
        );
        let err = Project::discover(&root.join("bad_primary"), Os::host())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("primary 'nope'"), "{err}");

        write(
            &root,
            "bad_primary/nomake.toml",
            "[workspace]\nmembers = [\"lib\"]\nprimary = \"lib\"\n",
        );
        let err = Project::discover(&root.join("bad_primary"), Os::host())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("primary 'lib' is a static library"), "{err}");

        fs::remove_dir_all(root).unwrap();
    }
}
