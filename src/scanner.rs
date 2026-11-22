use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::config::{Dependency, Kind, Manifest, Os, Sources, Toolchain};
use crate::registry;
use crate::util::{absolute, glob, matches, rel_str};
use crate::Result;

pub struct Pkg {
    pub name: String,
    pub version: String,
    pub root: PathBuf,
    pub kind: Kind,
    pub std: String,
    pub sources: Vec<PathBuf>,
    pub include_public: Vec<PathBuf>,
    pub include_private: Vec<PathBuf>,
    pub defines: Vec<String>,
    pub public_defines: Vec<String>,
    pub flags: Vec<String>,
    pub pch: Option<PathBuf>,
    pub link_flags: Vec<String>,
    pub libs: Vec<String>,
    pub subsystem: Option<String>,
    pub generate: Vec<crate::config::Generate>,
    /// Indices into `Workspace::pkgs`; always lower than this package's own index.
    pub deps: Vec<usize>,
    /// The subset of `deps` marked `public = true`.
    pub reexports: Vec<usize>,
    /// Packages from the root project's dependencies that this package's headers and defines come from, without linking or depending on them.
    pub peers: Vec<usize>,
    pub toolchain: Toolchain,
    pub test_args: Vec<String>,
}

/// Packages in dependency order. `roots` are the packages that were asked for (one, or every workspace member).
pub struct Workspace {
    pub pkgs: Vec<Pkg>,
    pub roots: Vec<usize>,
    crts: Vec<(String, crate::config::Crt)>,
}

impl Workspace {
    /// The package a single-root build is about; in a workspace, the first member.
    pub fn root(&self) -> &Pkg {
        &self.pkgs[self.roots[0]]
    }

    /// The one C runtime mode the whole build uses; packages that name one must agree.
    pub fn crt(&self) -> Result<crate::config::Crt> {
        let mut chosen: Option<(&str, crate::config::Crt)> = None;
        for (name, mode) in &self.crts {
            match chosen {
                Some((first, other)) if other != *mode => bail!(
                    "crt: '{first}' wants {}, '{name}' wants {}",
                    other.name(),
                    mode.name()
                ),
                Some(_) => {}
                None => chosen = Some((name.as_str(), *mode)),
            }
        }
        Ok(chosen.map_or_else(Default::default, |(_, mode)| mode))
    }

    /// Keeps only `root_dirs` and their transitive dependencies/peers, remapping package indices.
    pub fn retain_roots(&mut self, root_dirs: &[PathBuf]) {
        let root_indices: Vec<usize> = root_dirs
            .iter()
            .filter_map(|dir| self.pkgs.iter().position(|pkg| pkg.root == *dir))
            .collect();
        let mut keep = vec![false; self.pkgs.len()];
        let mut stack = root_indices.clone();
        while let Some(idx) = stack.pop() {
            if !keep[idx] {
                keep[idx] = true;
                for &dep in self.pkgs[idx].deps.iter().chain(&self.pkgs[idx].peers) {
                    stack.push(dep);
                }
            }
        }
        let mut remap = vec![None; self.pkgs.len()];
        let mut next = 0;
        for (old, &kept) in keep.iter().enumerate() {
            if kept {
                remap[old] = Some(next);
                next += 1;
            }
        }
        let mut new_pkgs = Vec::with_capacity(next);
        for (old, mut pkg) in self.pkgs.drain(..).enumerate() {
            if keep[old] {
                pkg.deps = pkg.deps.into_iter().map(|d| remap[d].unwrap()).collect();
                pkg.reexports = pkg
                    .reexports
                    .into_iter()
                    .map(|d| remap[d].unwrap())
                    .collect();
                pkg.peers = pkg.peers.into_iter().map(|d| remap[d].unwrap()).collect();
                new_pkgs.push(pkg);
            }
        }
        self.pkgs = new_pkgs;
        self.roots = root_indices
            .into_iter()
            .map(|old| remap[old].unwrap())
            .collect();
    }
}

#[cfg(test)]
pub fn load(root: &Path, os: Os) -> Result<Workspace> {
    let roots = [root.to_path_buf()];
    load_project(root, &roots, &roots, true, None, os)
}

/// Resolves and locks the full workspace (`all_roots`) across every target OS so `nomake.lock` never
/// churns on partial or test builds, then fetches and loads only the closure of `active_roots`.
pub fn load_project(
    lock_dir: &Path,
    all_roots: &[PathBuf],
    active_roots: &[PathBuf],
    has_root_package: bool,
    update: Option<&[String]>,
    os: Os,
) -> Result<Workspace> {
    for root in all_roots {
        if root != lock_dir && root.join("nomake.lock").is_file() {
            eprintln!(
                "nomake: warning: ignoring {} (workspace lockfile is {})",
                root.join("nomake.lock").display(),
                lock_dir.join("nomake.lock").display()
            );
        }
    }
    let mut resolver = registry::Resolver::new(lock_dir, update)?;
    // Lock every member and target OS first; recipes stay unfetched unless one references a nested path manifest.
    let (manifests, links) = read_manifests(all_roots, has_root_package, &mut resolver, os)?;
    for other in Os::ALL.into_iter().filter(|&other| other != os) {
        read_manifests(all_roots, has_root_package, &mut resolver, other)?;
    }
    resolver.save()?;
    let reachable = reachable_dirs(active_roots, all_roots, &manifests, &links);
    resolver.fetch_dirs(&reachable)?;
    finish_workspace(manifests, &links, all_roots, active_roots, has_root_package)
}

/// Loads host-executable generator tools against the already-resolved lockfile without rewriting `nomake.lock`.
pub fn load_host_tools(lock_dir: &Path, roots: &[PathBuf]) -> Result<Workspace> {
    let mut resolver = registry::Resolver::new(lock_dir, None)?;
    let (manifests, links) = read_manifests(roots, true, &mut resolver, Os::host())?;
    let reachable = reachable_dirs(roots, roots, &manifests, &links);
    resolver.fetch_dirs(&reachable)?;
    finish_workspace(manifests, &links, roots, roots, true)
}

/// Re-resolves registry dependencies against the newest registry (all of them when `names` is empty) and rewrites `nomake.lock`.
pub fn update(
    lock_dir: &Path,
    all_roots: &[PathBuf],
    has_root_package: bool,
    names: &[String],
) -> Result<()> {
    load_project(
        lock_dir,
        all_roots,
        all_roots,
        has_root_package,
        Some(names),
        Os::host(),
    )
    .map(|_| ())
}

fn reachable_dirs(
    active_roots: &[PathBuf],
    all_roots: &[PathBuf],
    manifests: &HashMap<PathBuf, Manifest>,
    links: &Links,
) -> HashSet<PathBuf> {
    let mut visited = HashSet::new();
    let mut stack: Vec<PathBuf> = active_roots.to_vec();
    while let Some(dir) = stack.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        let Some(manifest) = manifests.get(&dir) else {
            continue;
        };
        for key in manifest.dependencies.keys() {
            if let Some(dep_dir) = links.get(&(dir.clone(), key.clone())) {
                stack.push(dep_dir.clone());
            }
        }
        for key in &manifest.peers {
            if let Some(peer_dir) = all_roots
                .iter()
                .find_map(|root| links.get(&(root.clone(), key.clone())))
            {
                stack.push(peer_dir.clone());
            }
        }
    }
    visited
}

fn finish_workspace(
    manifests: HashMap<PathBuf, Manifest>,
    links: &Links,
    all_roots: &[PathBuf],
    active_roots: &[PathBuf],
    has_root_package: bool,
) -> Result<Workspace> {
    let mut crt_entries: Vec<(usize, String, crate::config::Crt)> = manifests
        .iter()
        .filter_map(|(dir, m)| {
            let rank = all_roots
                .iter()
                .position(|r| r == dir)
                .unwrap_or(usize::MAX);
            Some((rank, m.package.name.clone(), m.build.crt?))
        })
        .collect();
    crt_entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let crts = crt_entries
        .into_iter()
        .map(|(_, name, crt)| (name, crt))
        .collect();

    let mut loader = Loader {
        manifests,
        links,
        pkgs: Vec::new(),
        cfgs: Vec::new(),
        done: HashMap::new(),
        stack: Vec::new(),
        roots: all_roots.to_vec(),
        has_root_package,
    };
    let mut root_indices = Vec::new();
    for root in active_roots {
        root_indices.push(loader.load(root)?);
    }
    let Loader { mut pkgs, cfgs, .. } = loader;
    pkgs.par_iter_mut()
        .zip(cfgs)
        .for_each(|(p, c)| p.sources = collect_sources(&p.root, &c));
    Ok(Workspace {
        pkgs,
        roots: root_indices,
        crts,
    })
}

/// (package that lists the dependency, dependency key) -> the dependency's package root.
type Links = HashMap<(PathBuf, String), PathBuf>;

/// Finds a dependency's package root and the manifest describing it.
fn locate(
    dir: &Path,
    key: &str,
    dep: &Dependency,
    pins: &HashMap<String, (String, String)>,
    resolver: &mut registry::Resolver,
) -> Result<(PathBuf, PathBuf)> {
    match (&dep.path, &dep.version) {
        (Some(path), None) => {
            let root = absolute(&dir.join(path));
            let file = root.join("nomake.toml");
            Ok((root, file))
        }
        (None, Some(requirement)) => {
            let pinned = pins.get(key).map(|(_, req)| req).unwrap_or(requirement);
            let located = resolver.locate(key, pinned)?;
            Ok((located.root, located.recipe))
        }
        _ => bail!("dependency '{key}' needs exactly one of path and version"),
    }
}

/// Reads every manifest reachable from `roots`, one dependency level at a time, each level in parallel.
/// Versions listed in a root manifest override what other packages ask for; conflicting root pins error.
fn read_manifests(
    roots: &[PathBuf],
    has_root_package: bool,
    resolver: &mut registry::Resolver,
    os: Os,
) -> Result<(HashMap<PathBuf, Manifest>, Links)> {
    let mut all = HashMap::new();
    let mut links = Links::new();
    let mut wanted: HashMap<PathBuf, Wanted> = HashMap::new();
    let mut pins: HashMap<String, (String, String)> = HashMap::new();
    let mut seen: HashSet<PathBuf> = roots.iter().cloned().collect();
    let mut level: Vec<(PathBuf, PathBuf)> = roots
        .iter()
        .map(|root| (root.clone(), root.join("nomake.toml")))
        .collect();
    while !level.is_empty() {
        resolver.fetch_for_paths(&level)?;
        let read = level
            .into_par_iter()
            .map(|(dir, file)| -> Result<(PathBuf, Manifest)> {
                if !file.is_file() {
                    bail!(
                        "{} not found (run `nomake init` in {})",
                        file.display(),
                        dir.display()
                    );
                }
                Ok((dir, Manifest::load_resolved(&file, os)?))
            })
            .collect::<Result<Vec<_>>>()?;
        level = Vec::new();
        // Every root's pins must be known and mutually compatible before any dependency is located.
        for (dir, manifest) in &read {
            if roots.contains(dir) {
                for (key, dep) in &manifest.dependencies {
                    let Some(req) = &dep.version else { continue };
                    match pins.get(key) {
                        None => {
                            pins.insert(key.clone(), (manifest.package.name.clone(), req.clone()));
                        }
                        Some((prev_pkg, prev_req)) => {
                            if registry::version_matches(req, prev_req) {
                                pins.insert(
                                    key.clone(),
                                    (manifest.package.name.clone(), req.clone()),
                                );
                            } else if !registry::version_matches(prev_req, req) {
                                bail!(
                                    "conflicting versions for '{key}': '{prev_pkg}' wants '{prev_req}', '{}' wants '{req}'",
                                    manifest.package.name
                                );
                            }
                        }
                    }
                }
            }
        }
        for (dir, manifest) in read {
            let from_root = roots.contains(&dir);
            for (key, dep) in &manifest.dependencies {
                let (dep_dir, file) = locate(&dir, key, dep, &pins, resolver)?;
                if seen.insert(dep_dir.clone()) {
                    level.push((dep_dir.clone(), file));
                }
                links.insert((dir.clone(), key.clone()), dep_dir.clone());
                wanted
                    .entry(dep_dir)
                    .or_default()
                    .add(&manifest.package.name, dep, from_root);
            }
            all.insert(dir, manifest);
        }
    }
    // Check package name uniqueness across the full workspace and dependency graph.
    let mut dirs_sorted: Vec<PathBuf> = all.keys().cloned().collect();
    dirs_sorted.sort();
    let mut by_name: HashMap<String, PathBuf> = HashMap::new();
    for dir in &dirs_sorted {
        let name = &all[dir].package.name;
        if let Some(prev) = by_name.insert(name.clone(), dir.clone()) {
            let hint = if has_root_package {
                "in the root nomake.toml"
            } else {
                "in a workspace member's nomake.toml"
            };
            bail!(
                "duplicate package name '{name}' in {} and {} (pin one version {hint})",
                prev.display(),
                dir.display()
            );
        }
    }
    // Features only add files and flags, never dependencies, so they can be switched on once every
    // dependent has been seen. A package's features are the union of what its dependents ask for.
    for root in roots {
        wanted.entry(root.clone()).or_default().defaults = true;
    }
    for dir in &dirs_sorted {
        let manifest = all.get_mut(dir).unwrap();
        let features = wanted.remove(dir).unwrap_or_default();
        manifest.enable(&features.names, features.defaults)?;
        let link = features
            .link(has_root_package)
            .map_err(|e| format!("{}: {e}", manifest.package.name))?;
        manifest.link(link, os)?;
    }
    Ok((all, links))
}

/// What one package's dependents ask for: features to switch on and how to link it.
#[derive(Default)]
struct Wanted {
    names: BTreeSet<String>,
    /// Default features stay on unless every dependent opts out.
    defaults: bool,
    /// `(member_name, kind)` asked for by root/workspace-member manifests.
    root_links: Vec<(String, Kind)>,
    links: BTreeSet<String>,
}

impl Wanted {
    fn add(&mut self, dependent: &str, dep: &Dependency, from_root: bool) {
        self.names.extend(dep.features.iter().cloned());
        self.defaults |= dep.default_features;
        if let Some(kind) = dep.link {
            if from_root {
                self.root_links.push((dependent.to_string(), kind));
            }
            self.links.insert(kind.name().to_string());
        }
    }

    /// Root/member requests win when they agree; conflicting member requests error.
    fn link(&self, has_root_package: bool) -> Result<Option<Kind>> {
        if let Some((first_pkg, first_kind)) = self.root_links.first() {
            for (other_pkg, other_kind) in &self.root_links[1..] {
                if other_kind != first_kind {
                    bail!(
                        "conflicting link modes: '{first_pkg}' wants {}, '{other_pkg}' wants {}",
                        first_kind.name(),
                        other_kind.name()
                    );
                }
            }
            return Ok(Some(*first_kind));
        }
        let hint = if has_root_package {
            "in the root nomake.toml"
        } else {
            "in a workspace member's nomake.toml"
        };
        match self.links.len() {
            0 => Ok(None),
            1 => Ok(Some(if self.links.contains("shared") {
                Kind::Shared
            } else {
                Kind::Static
            })),
            _ => bail!("dependents want both static and shared (set `link` {hint})"),
        }
    }
}

/// Turns manifests into packages, dependencies first, so a package's index is known before its dependents need it.
struct Loader<'a> {
    manifests: HashMap<PathBuf, Manifest>,
    links: &'a Links,
    pkgs: Vec<Pkg>,
    cfgs: Vec<Sources>,
    done: HashMap<PathBuf, usize>,
    stack: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    has_root_package: bool,
}

impl Loader<'_> {
    fn load(&mut self, dir: &Path) -> Result<usize> {
        if let Some(&i) = self.done.get(dir) {
            return Ok(i);
        }
        if self.stack.iter().any(|visiting| visiting == dir) {
            bail!("dependency cycle through {}", dir.display());
        }
        let manifest = self.manifests.remove(dir).unwrap();

        self.stack.push(dir.to_path_buf());
        let mut deps = Vec::new();
        let mut reexports = Vec::new();
        for (key, dep) in &manifest.dependencies {
            let dep_dir = self.links[&(dir.to_path_buf(), key.clone())].clone();
            let idx = self.load(&dep_dir)?;
            deps.push(idx);
            if dep.public {
                reexports.push(idx);
            }
        }
        let mut peers = Vec::new();
        for key in &manifest.peers {
            let Some(peer_dir) = self
                .roots
                .iter()
                .find_map(|root| self.links.get(&(root.clone(), key.clone())))
                .cloned()
            else {
                let hint = if self.has_root_package {
                    "the root nomake.toml"
                } else {
                    "a workspace member's nomake.toml"
                };
                bail!(
                    "package '{}' needs peer '{key}' (add it to [dependencies] in {hint})",
                    manifest.package.name
                );
            };
            peers.push(self.load(&peer_dir)?);
        }
        self.stack.pop();

        let existing = |names: &[&str]| -> Vec<PathBuf> {
            names
                .iter()
                .map(|name| dir.join(name))
                .filter(|path| path.is_dir())
                .collect()
        };
        let resolve = |paths: &[String]| -> Vec<PathBuf> {
            paths.iter().map(|path| absolute(&dir.join(path))).collect()
        };
        let (mut include_public, mut include_private) = if manifest.include_defaults {
            (existing(&["include"]), existing(&["src"]))
        } else {
            (Vec::new(), Vec::new())
        };
        if let Some(include) = &manifest.include {
            for path in resolve(&include.public) {
                if !include_public.contains(&path) {
                    include_public.push(path);
                }
            }
            for path in resolve(&include.private) {
                if !include_private.contains(&path) {
                    include_private.push(path);
                }
            }
        }

        let kind = manifest.package.kind();
        let idx = self.pkgs.len();
        self.cfgs.push(manifest.sources);
        self.pkgs.push(Pkg {
            name: manifest.package.name,
            version: manifest.package.version,
            root: dir.to_path_buf(),
            kind,
            std: manifest.package.cxx_standard.get(),
            sources: Vec::new(),
            include_public,
            include_private,
            defines: manifest.build.defines,
            public_defines: manifest.build.public_defines,
            flags: manifest.build.flags,
            pch: manifest.build.pch.map(|path| dir.join(path)),
            link_flags: manifest.build.link_flags,
            libs: manifest.build.libs,
            subsystem: manifest.build.subsystem,
            generate: manifest.generate,
            deps,
            reexports,
            peers,
            toolchain: manifest.toolchain,
            test_args: manifest.test.args,
        });
        self.done.insert(dir.to_path_buf(), idx);
        Ok(idx)
    }
}

pub const SOURCE_EXTS: [&str; 4] = ["cpp", "cc", "cxx", "c"];

pub fn is_source(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        SOURCE_EXTS
            .iter()
            .any(|source| ext.eq_ignore_ascii_case(source))
    })
}

pub const HEADER_EXTS: [&str; 5] = ["h", "hpp", "hh", "hxx", "inl"];

pub fn is_header(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        HEADER_EXTS
            .iter()
            .any(|header| ext.eq_ignore_ascii_case(header))
    })
}

/// Headers in `dir`, and in its subfolders when `recursive`.
pub fn collect_headers(dir: &Path, recursive: bool, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') || name == "build" {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if recursive {
                collect_headers(&path, true, out);
            }
        } else if is_header(&path) {
            out.insert(path);
        }
    }
}

pub fn collect_sources(root: &Path, cfg: &crate::config::Sources) -> Vec<PathBuf> {
    let dirs = match &cfg.dirs {
        Some(dirs) => dirs.clone(),
        None if cfg.files.is_empty() => vec!["src".to_string()],
        None => vec![],
    };
    let mut out = Vec::new();
    for dir in &dirs {
        walk(root, &absolute(&root.join(dir)), &cfg.exclude, &mut out);
    }
    for pattern in &cfg.files {
        for path in glob(root, pattern) {
            let rel = rel_str(root, &path);
            if is_source(&path) && !cfg.exclude.iter().any(|exclude| matches(exclude, &rel)) {
                out.push(path);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk(root: &Path, dir: &Path, exclude: &[String], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let top_deps = dir == root && name == "deps";
        let excluded = exclude
            .iter()
            .any(|pattern| matches(pattern, &rel_str(root, &path)));
        if name.starts_with('.') || name == "build" || top_deps || excluded {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            walk(root, &path, exclude, out);
        } else if is_source(&path) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn source_names(pkg: &Pkg) -> Vec<String> {
        let mut names: Vec<String> = pkg
            .sources
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn dependents_choose_features_and_their_choices_add_up() {
        let root = std::env::temp_dir().join(format!("nomake_features_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root, "lib/nomake.toml", "[package]\nname = \"lib\"\ntype = \"static\"\n[sources]\nfiles = [\"base.c\"]\n[features]\ndefault = [\"a\"]\n[feature.a]\nsources.files = [\"a.c\"]\n[feature.b]\nsources.files = [\"b.c\"]\n[feature.c]\nsources.files = [\"c.c\"]\n");
        for file in ["base", "a", "b", "c"] {
            write(&root, &format!("lib/{file}.c"), "");
        }
        write(&root, "mid/nomake.toml", "[package]\nname = \"mid\"\ntype = \"static\"\n[dependencies]\nlib = { path = \"../lib\", features = [\"c\"], default-features = false }\n");
        write(&root, "mid/m.c", "");
        let app = |dependencies: &str| {
            write(&root, "app/nomake.toml", &format!("[package]\nname = \"app\"\n[dependencies]\nmid = {{ path = \"../mid\" }}\n{dependencies}"));
            write(&root, "app/main.c", "");
            let ws = load(&root.join("app"), Os::host()).unwrap();
            source_names(&ws.pkgs[0])
        };
        // Only mid asks for lib: no defaults, just `c`.
        assert_eq!(app(""), ["base.c", "c.c"]);
        // The app also asks for `b` and the defaults, and everything is added up.
        assert_eq!(
            app("lib = { path = \"../lib\", features = [\"b\"] }\n"),
            ["a.c", "b.c", "base.c", "c.c"]
        );
        write(&root, "app/nomake.toml", "[package]\nname = \"app\"\n[dependencies]\nlib = { path = \"../lib\", features = [\"nope\"] }\n");
        assert!(load(&root.join("app"), Os::host()).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn feature_peers_come_from_the_root_dependencies() {
        let root = std::env::temp_dir().join(format!("nomake_peers_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(
            &root,
            "sdl/nomake.toml",
            "[package]\nname = \"sdl\"\ntype = \"static\"\n[include]\npublic = [\"include\"]\n",
        );
        write(&root, "sdl/a.c", "");
        write(&root, "ui/nomake.toml", "[package]\nname = \"ui\"\ntype = \"static\"\n[features]\ndefault = []\n[feature.sdl]\npeer = [\"sdl\"]\nsources.files = [\"b.c\"]\n");
        write(&root, "ui/b.c", "");
        write(&root, "app/main.c", "");
        let app = |dependencies: &str| {
            write(
                &root,
                "app/nomake.toml",
                &format!("[package]\nname = \"app\"\n[dependencies]\n{dependencies}"),
            );
            load(&root.join("app"), Os::host())
        };
        let on = "ui = { path = \"../ui\", features = [\"sdl\"] }\n";
        assert!(app(on).is_err());
        let ws = app(&format!("sdl = {{ path = \"../sdl\" }}\n{on}")).unwrap();
        let ui = ws.pkgs.iter().position(|p| p.name == "ui").unwrap();
        let sdl = ws.pkgs.iter().position(|p| p.name == "sdl").unwrap();
        assert!(sdl < ui && ws.pkgs[ui].peers == [sdl] && ws.pkgs[ui].deps.is_empty());
        let ws = app("sdl = { path = \"../sdl\" }\nui = { path = \"../ui\" }\n").unwrap();
        assert!(ws.pkgs.iter().all(|p| p.peers.is_empty()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_workspace_load_shares_features_crt_and_checks_link_and_name_conflicts() {
        let root = std::env::temp_dir().join(format!("nomake_ws_scan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(
            &root,
            "dual/nomake.toml",
            "[package]\nname = \"dual\"\ntype = [\"shared\", \"static\"]\n[sources]\nfiles = [\"base.c\"]\n[features]\ndefault = []\n[feature.extra]\nsources.files = [\"extra.c\"]\n",
        );
        write(&root, "dual/base.c", "");
        write(&root, "dual/extra.c", "");

        write(
            &root,
            "app/nomake.toml",
            "[package]\nname = \"app\"\n[build]\ncrt = \"static\"\n[dependencies]\ndual = { path = \"../dual\", link = \"static\", features = [\"extra\"] }\n",
        );
        write(&root, "app/main.c", "");
        write(
            &root,
            "suite/nomake.toml",
            "[package]\nname = \"suite\"\ntype = \"test\"\n[dependencies]\ndual = { path = \"../dual\" }\n",
        );
        write(&root, "suite/t.c", "");

        let all = [root.join("app"), root.join("suite")];
        // Loading ONLY `suite` still enables `dual`'s `extra` feature, links `dual` static, and uses `app`'s static CRT.
        let ws = load_project(
            &root,
            &all,
            std::slice::from_ref(&all[1]),
            false,
            None,
            Os::host(),
        )
        .unwrap();
        assert_eq!(ws.pkgs.len(), 2);
        let dual = &ws.pkgs[0];
        assert_eq!(dual.kind, Kind::Static);
        assert_eq!(source_names(dual), ["base.c", "extra.c"]);
        assert_eq!(ws.crt().unwrap(), crate::config::Crt::Static);

        // Conflicting root `link` modes across workspace members must fail naming both members.
        write(
            &root,
            "suite/nomake.toml",
            "[package]\nname = \"suite\"\ntype = \"test\"\n[dependencies]\ndual = { path = \"../dual\", link = \"shared\" }\n",
        );
        let err = load_project(&root, &all, &all, false, None, Os::host())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("conflicting link modes")
                && err.contains("'app'")
                && err.contains("'suite'"),
            "{err}"
        );

        // Duplicate package names across workspace members fail with pure-workspace wording.
        write(
            &root,
            "suite/nomake.toml",
            "[package]\nname = \"app\"\ntype = \"test\"\n",
        );
        let err = load_project(
            &root,
            &all,
            std::slice::from_ref(&all[1]),
            false,
            None,
            Os::host(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            err.contains("duplicate package name 'app'")
                && err.contains("workspace member's nomake.toml"),
            "{err}"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
