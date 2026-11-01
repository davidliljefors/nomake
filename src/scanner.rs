use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::config::{Dependency, Kind, Manifest, Sources, Toolchain};
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
    pub crt: Option<crate::config::Crt>,
    pub subsystem: Option<String>,
    pub generate: Vec<crate::config::Generate>,
    /// Indices into `Workspace::pkgs`; always lower than this package's own index.
    pub deps: Vec<usize>,
    /// The subset of `deps` marked `public = true`.
    pub reexports: Vec<usize>,
    /// Packages from the root project's dependencies that this package's headers and defines come from, without linking or depending on them.
    pub peers: Vec<usize>,
    pub toolchain: Toolchain,
}

/// Packages in dependency order. `roots` are the packages that were asked for (one, or every workspace member).
pub struct Workspace {
    pub pkgs: Vec<Pkg>,
    pub roots: Vec<usize>,
}

impl Workspace {
    /// The package a single-root build is about; in a workspace, the first member.
    pub fn root(&self) -> &Pkg {
        &self.pkgs[self.roots[0]]
    }

    /// The one C runtime mode the whole build uses; packages that name one must agree.
    pub fn crt(&self) -> Result<crate::config::Crt> {
        let mut chosen: Option<(&str, crate::config::Crt)> = None;
        for pkg in &self.pkgs {
            let Some(mode) = pkg.crt else { continue };
            match chosen {
                Some((first, other)) if other != mode => bail!(
                    "crt: '{first}' wants {} but '{}' wants {}; one build uses one runtime",
                    other.name(),
                    pkg.name,
                    mode.name()
                ),
                Some(_) => {}
                None => chosen = Some((&pkg.name, mode)),
            }
        }
        Ok(chosen.map_or_else(Default::default, |(_, mode)| mode))
    }
}

pub fn load(root: &Path) -> Result<Workspace> {
    load_roots(root, std::slice::from_ref(&root.to_path_buf()))
}

/// Loads several root packages into one set of packages, so a shared dependency exists once with the
/// features and link kind every member asks for. The lock file lives in `lock_dir`.
pub fn load_roots(lock_dir: &Path, roots: &[PathBuf]) -> Result<Workspace> {
    load_with(lock_dir, roots, None)
}

/// Re-resolves registry dependencies against the newest registry (all of them when `names` is empty) and rewrites `nomake.lock`.
pub fn update(root: &Path, names: &[String]) -> Result<()> {
    load_with(root, std::slice::from_ref(&root.to_path_buf()), Some(names)).map(|_| ())
}

fn load_with(lock_dir: &Path, roots: &[PathBuf], update: Option<&[String]>) -> Result<Workspace> {
    let mut resolver = registry::Resolver::new(lock_dir, update)?;
    let (manifests, links) = read_manifests(roots, &mut resolver)?;
    resolver.fetch()?;
    resolver.save()?;
    let mut loader = Loader {
        manifests,
        links: &links,
        pkgs: Vec::new(),
        cfgs: Vec::new(),
        done: HashMap::new(),
        stack: Vec::new(),
        roots: roots.to_vec(),
    };
    let mut root_indices = Vec::new();
    for root in roots {
        root_indices.push(loader.load(root)?);
    }
    let Loader { mut pkgs, cfgs, .. } = loader;
    pkgs.par_iter_mut()
        .zip(cfgs)
        .for_each(|(p, c)| p.sources = collect_sources(&p.root, &c));
    let mut names = HashSet::new();
    for pkg in &pkgs {
        if !names.insert(pkg.name.as_str()) {
            bail!("two packages are named '{}' (two versions of one registry dependency? pin it in the root nomake.toml)", pkg.name);
        }
    }
    Ok(Workspace {
        pkgs,
        roots: root_indices,
    })
}

/// (package that lists the dependency, dependency key) -> the dependency's package root.
type Links = HashMap<(PathBuf, String), PathBuf>;

/// Finds a dependency's package root and the manifest describing it. Registry dependencies are fetched on the way.
fn locate(
    dir: &Path,
    key: &str,
    dep: &Dependency,
    pins: &HashMap<String, String>,
    resolver: &mut registry::Resolver,
) -> Result<(PathBuf, PathBuf)> {
    match (&dep.path, &dep.version) {
        (Some(path), None) => {
            let root = absolute(&dir.join(path));
            let file = root.join("nomake.toml");
            Ok((root, file))
        }
        (None, Some(requirement)) => {
            let located = resolver.locate(key, pins.get(key).unwrap_or(requirement))?;
            Ok((located.root, located.recipe))
        }
        _ => bail!("dependency '{key}' needs exactly one of path and version"),
    }
}

/// Reads every manifest reachable from `roots`, one dependency level at a time, each level in parallel.
/// Versions listed in a root manifest override what other packages ask for.
fn read_manifests(
    roots: &[PathBuf],
    resolver: &mut registry::Resolver,
) -> Result<(HashMap<PathBuf, Manifest>, Links)> {
    let mut all = HashMap::new();
    let mut links = Links::new();
    let mut wanted: HashMap<PathBuf, Wanted> = HashMap::new();
    let mut pins: HashMap<String, String> = HashMap::new();
    let mut seen: HashSet<PathBuf> = roots.iter().cloned().collect();
    let mut level: Vec<(PathBuf, PathBuf)> = roots
        .iter()
        .map(|root| (root.clone(), root.join("nomake.toml")))
        .collect();
    while !level.is_empty() {
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
                Ok((dir, Manifest::load_resolved(&file)?))
            })
            .collect::<Result<Vec<_>>>()?;
        level = Vec::new();
        // Every root's pins have to be known before any dependency is located.
        for (dir, manifest) in &read {
            if roots.contains(dir) {
                pins.extend(
                    manifest
                        .dependencies
                        .iter()
                        .filter_map(|(key, dep)| Some((key.clone(), dep.version.clone()?))),
                );
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
                wanted.entry(dep_dir).or_default().add(dep, from_root);
            }
            all.insert(dir, manifest);
        }
    }
    // Features only add files and flags, never dependencies, so they can be switched on once every
    // dependent has been seen. A package's features are the union of what its dependents ask for.
    for root in roots {
        wanted.entry(root.clone()).or_default().defaults = true;
    }
    for (dir, manifest) in &mut all {
        let features = wanted.remove(dir).unwrap_or_default();
        manifest.enable(&features.names, features.defaults)?;
        let link = features
            .link()
            .map_err(|e| format!("{}: {e}", manifest.package.name))?;
        manifest.link(link)?;
    }
    Ok((all, links))
}

/// What one package's dependents ask for: features to switch on and how to link it.
#[derive(Default)]
struct Wanted {
    names: BTreeSet<String>,
    /// Default features stay on unless every dependent opts out.
    defaults: bool,
    /// Link kinds asked for by the root project, and by other packages.
    root_link: Option<Kind>,
    links: BTreeSet<String>,
}

impl Wanted {
    fn add(&mut self, dep: &Dependency, from_root: bool) {
        self.names.extend(dep.features.iter().cloned());
        self.defaults |= dep.default_features;
        if let Some(kind) = dep.link {
            if from_root {
                self.root_link = Some(kind);
            }
            self.links.insert(format!("{kind:?}").to_lowercase());
        }
    }

    /// The root's request wins; otherwise every dependent that asks must ask for the same.
    fn link(&self) -> Result<Option<Kind>> {
        if self.root_link.is_some() {
            return Ok(self.root_link);
        }
        match self.links.len() {
            0 => Ok(None),
            1 => Ok(Some(if self.links.contains("shared") { Kind::Shared } else { Kind::Static })),
            _ => bail!("dependents ask for it to be linked both static and shared (set `link` in the root nomake.toml to choose)"),
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
                bail!(
                    "package '{}' needs peer dependency '{key}'; add it to [dependencies] of the root nomake.toml",
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
        let (include_public, include_private) = match &manifest.include {
            Some(include) => (resolve(&include.public), resolve(&include.private)),
            None => (existing(&["include"]), existing(&["src"])),
        };

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
            crt: manifest.build.crt,
            subsystem: manifest.build.subsystem,
            generate: manifest.generate,
            deps,
            reexports,
            peers,
            toolchain: manifest.toolchain,
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
            let ws = load(&root.join("app")).unwrap();
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
        assert!(load(&root.join("app")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn feature_peers_come_from_the_root_dependencies() {
        let root = std::env::temp_dir().join(format!("nomake_peers_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root, "sdl/nomake.toml", "[package]\nname = \"sdl\"\ntype = \"static\"\n[include]\npublic = [\"include\"]\n");
        write(&root, "sdl/a.c", "");
        write(&root, "ui/nomake.toml", "[package]\nname = \"ui\"\ntype = \"static\"\n[features]\ndefault = []\n[feature.sdl]\npeer = [\"sdl\"]\nsources.files = [\"b.c\"]\n");
        write(&root, "ui/b.c", "");
        write(&root, "app/main.c", "");
        let app = |dependencies: &str| {
            write(&root, "app/nomake.toml", &format!("[package]\nname = \"app\"\n[dependencies]\n{dependencies}"));
            load(&root.join("app"))
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
}
