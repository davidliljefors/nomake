//! `nomake init`: inspects the current directory and writes a reviewable `nomake.toml`.

use std::collections::BTreeSet;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use toml_edit::{DocumentMut, Item};

mod registry;
mod tui;

use crate::config::{Kind, Manifest};
use crate::scanner::{collect_sources, is_header, is_source};
use crate::util::{absolute, rel_str};
use crate::Result;

/// Manifest keys the editor owns; an empty list means every key of the table.
const MANAGED: [(&str, &[&str]); 5] = [
    ("package", &["name", "type", "cxx_standard"]),
    ("sources", &["dirs", "files", "exclude"]),
    ("include", &["public", "private"]),
    ("build", &["defines", "libs"]),
    ("dependencies", &[]),
];

const SKIP_NAMES: [&str; 17] = [
    "example",
    "examples",
    "test",
    "tests",
    "sample",
    "samples",
    "demo",
    "demos",
    "bench",
    "benchmark",
    "benchmarks",
    "doc",
    "docs",
    "tool",
    "tools",
    "misc",
    "backends",
];

fn is_skip_name(name: &str) -> bool {
    SKIP_NAMES.contains(&name.to_lowercase().as_str())
}

fn is_locked_dir(name: &str, in_root: bool) -> bool {
    name.starts_with('.')
        || name == "build"
        || name == "target"
        || name == "node_modules"
        || (in_root && name == "deps")
}

fn has_main(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes);
    ["int main(", "int main (", "wmain(", "WinMain("]
        .iter()
        .any(|marker| text.contains(marker))
}

pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Executable => "executable",
        Kind::Static => "static",
        Kind::Shared => "shared",
    }
}

fn toml_list(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| format!("{value:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn toml_key(key: &str) -> String {
    if key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        key.to_string()
    } else {
        format!("{key:?}")
    }
}

fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn ext_of(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    Source,
    Header,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    In,
    Out,
    Partial,
}

/// One directory or source/header file; nodes are stored in preorder so a subtree is `index..end`.
pub struct Node {
    pub name: String,
    pub rel: String,
    pub kind: NodeKind,
    pub parent: usize,
    pub depth: usize,
    pub children: Vec<usize>,
    pub end: usize,
    /// Directories the scanner never walks; shown but not descended into.
    pub locked: bool,
    pub has_main: bool,
    pub src_total: usize,
    pub hdr_total: usize,
}

pub struct Computed {
    pub effective: Vec<bool>,
    pub in_count: Vec<usize>,
}

struct Selection {
    dirs: Vec<String>,
    files: Vec<String>,
    exclude: Vec<String>,
}

pub struct Dep {
    pub key: String,
    /// Relative to the package, with forward slashes; empty for a registry dependency.
    pub path: String,
    /// The version requirement when this is a registry package.
    pub version: Option<String>,
    pub on: bool,
    pub configured: bool,
    pub public: bool,
    pub features: Vec<String>,
    pub default_features: bool,
    pub link: Option<Kind>,
}

impl Dep {
    pub fn label(&self) -> String {
        match &self.version {
            Some(version) => format!("{} (registry {version})", self.key),
            None => self.path.clone(),
        }
    }

    fn is_folder(&self, dir: &Path, root: &Path) -> bool {
        self.version.is_none() && absolute(&root.join(&self.path)) == dir
    }
}

pub struct Model {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    /// Explicit include/exclude per node; `None` inherits from the parent (the root defaults to out).
    pub choice: Vec<Option<bool>>,
    pub include: Vec<bool>,
    pub name: String,
    pub kind: Kind,
    pub std: String,
    pub defines: String,
    pub libs: String,
    pub export: bool,
    pub deps: Vec<Dep>,
    pub parent_toml: Option<PathBuf>,
    pub register: bool,
    /// Manifest text rendered right after loading an existing `nomake.toml`; lets writes touch only what changed.
    pub baseline: Option<String>,
}

fn scan_dir(root: &Path, dir: &Path, index: usize, nodes: &mut Vec<Node>, seen: &AtomicUsize) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut entries: Vec<(bool, fs::DirEntry)> = read
        .flatten()
        .map(|entry| (entry.file_type().is_ok_and(|kind| kind.is_dir()), entry))
        .collect();
    entries.sort_by_key(|(is_dir, entry)| (!*is_dir, entry.file_name()));
    let depth = nodes[index].depth + 1;
    for (is_dir, entry) in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = if is_dir {
            NodeKind::Dir
        } else if is_source(&path) {
            NodeKind::Source
        } else if is_header(&path) {
            NodeKind::Header
        } else {
            continue;
        };
        let child = nodes.len();
        let locked = is_dir && is_locked_dir(&name, index == 0);
        nodes.push(Node {
            name,
            rel: rel_str(root, &path),
            kind,
            parent: index,
            depth,
            children: Vec::new(),
            end: child + 1,
            locked,
            has_main: false,
            src_total: (kind == NodeKind::Source) as usize,
            hdr_total: (kind == NodeKind::Header) as usize,
        });
        nodes[index].children.push(child);
        seen.store(nodes.len(), Relaxed);
        if is_dir && !locked {
            scan_dir(root, &path, child, nodes, seen);
            nodes[child].end = nodes.len();
        }
    }
}

/// Shows progress on stderr once a scan has taken a noticeable time, so a big folder doesn't look like a hang.
fn spin(seen: &AtomicUsize, done: &AtomicBool) {
    const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    let start = Instant::now();
    let mut shown = false;
    let mut frame = 0;
    while !done.load(Relaxed) {
        std::thread::park_timeout(Duration::from_millis(100));
        if done.load(Relaxed) || start.elapsed() < Duration::from_millis(200) {
            continue;
        }
        eprint!(
            "\r{} Scanning directories... {} entries (Ctrl+C to cancel)",
            FRAMES[frame % FRAMES.len()],
            seen.load(Relaxed)
        );
        frame += 1;
        shown = true;
    }
    if shown {
        eprint!("\r{:70}\r", "");
    }
}

fn scan_tree(root: &Path) -> Vec<Node> {
    let mut nodes = vec![Node {
        name: ".".into(),
        rel: ".".into(),
        kind: NodeKind::Dir,
        parent: 0,
        depth: 0,
        children: Vec::new(),
        end: 0,
        locked: false,
        has_main: false,
        src_total: 0,
        hdr_total: 0,
    }];
    let seen = AtomicUsize::new(0);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let spinner = std::io::stderr()
            .is_terminal()
            .then(|| scope.spawn(|| spin(&seen, &done)));
        scan_dir(root, root, 0, &mut nodes, &seen);
        done.store(true, Relaxed);
        if let Some(spinner) = &spinner {
            spinner.thread().unpark();
        }
    });
    nodes[0].end = nodes.len();
    for index in (1..nodes.len()).rev() {
        let parent = nodes[index].parent;
        let (sources, headers) = (nodes[index].src_total, nodes[index].hdr_total);
        nodes[parent].src_total += sources;
        nodes[parent].hdr_total += headers;
    }
    nodes
}

fn find_deps(root: &Path) -> Vec<Dep> {
    let Ok(read) = fs::read_dir(root.join("deps")) else {
        return Vec::new();
    };
    let mut deps: Vec<Dep> = read
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .map(|name| {
            let configured = root.join("deps").join(&name).join("nomake.toml").is_file();
            Dep {
                path: format!("deps/{name}"),
                key: name,
                version: None,
                on: configured,
                configured,
                public: false,
                features: Vec::new(),
                default_features: true,
                link: None,
            }
        })
        .collect();
    deps.sort_by(|left, right| left.key.cmp(&right.key));
    deps
}

/// Dependencies the manifest lists, followed by the unlisted folders in `deps/`.
fn load_deps(root: &Path, manifest: &Manifest) -> Vec<Dep> {
    let mut deps: Vec<Dep> = manifest
        .dependencies
        .iter()
        .map(|(key, dep)| match (&dep.path, &dep.version) {
            (Some(path), _) => Dep {
                key: key.clone(),
                path: path.replace('\\', "/"),
                version: None,
                on: true,
                configured: root.join(path).join("nomake.toml").is_file(),
                public: dep.public,
                features: dep.features.clone(),
                default_features: dep.default_features,
                link: dep.link,
            },
            (None, version) => Dep {
                key: key.clone(),
                path: String::new(),
                version: version.clone(),
                on: true,
                configured: true,
                public: dep.public,
                features: dep.features.clone(),
                default_features: dep.default_features,
                link: dep.link,
            },
        })
        .collect();
    for found in find_deps(root) {
        let listed = deps
            .iter()
            .any(|dep| dep.is_folder(&absolute(&root.join(&found.path)), root));
        if !listed {
            deps.push(Dep { on: false, ..found });
        }
    }
    deps
}

/// The parent project's manifest when `root` is `deps/<dir>` and not registered there yet.
fn registrable_parent(root: &Path) -> Option<PathBuf> {
    let deps_dir = root.parent()?;
    if deps_dir.file_name()? != "deps" {
        return None;
    }
    let parent_toml = deps_dir.parent()?.join("nomake.toml");
    let text = fs::read_to_string(&parent_toml).ok()?;
    let dir = root.file_name()?.to_string_lossy().into_owned();
    (!text.contains(&format!("deps/{dir}"))).then_some(parent_toml)
}

impl Model {
    pub fn scan(root: &Path, kind_flag: Option<Kind>) -> Model {
        let nodes = scan_tree(root);
        let count = nodes.len();
        let in_deps = root
            .parent()
            .and_then(|dir| dir.file_name())
            .is_some_and(|name| name == "deps");
        let parent_toml = registrable_parent(root);
        let mut model = Model {
            root: root.to_path_buf(),
            nodes,
            choice: vec![None; count],
            include: vec![false; count],
            name: root.file_name().unwrap().to_string_lossy().into_owned(),
            kind: Kind::Static,
            std: "20".into(),
            defines: String::new(),
            libs: String::new(),
            export: true,
            deps: find_deps(root),
            register: parent_toml.is_some(),
            parent_toml,
            baseline: None,
        };
        model.apply_source_defaults(kind_flag, in_deps);
        model.export = model.kind != Kind::Executable;
        model.apply_include_defaults();
        model
    }

    /// Opens the editor model for `root`: the existing `nomake.toml` when there is one, detected defaults otherwise.
    pub fn open(root: &Path, kind_flag: Option<Kind>) -> Result<Model> {
        let manifest_path = root.join("nomake.toml");
        if manifest_path.is_file() {
            Ok(Model::load(root, &Manifest::load(&manifest_path)?))
        } else {
            Ok(Model::scan(root, kind_flag))
        }
    }

    fn load(root: &Path, manifest: &Manifest) -> Model {
        let nodes = scan_tree(root);
        let count = nodes.len();
        let parent_toml = registrable_parent(root);
        let mut model = Model {
            root: root.to_path_buf(),
            nodes,
            choice: vec![None; count],
            include: vec![false; count],
            name: manifest.package.name.clone(),
            kind: manifest.package.kind(),
            std: manifest.package.cxx_standard.get(),
            defines: manifest.build.defines.join(", "),
            libs: manifest.build.libs.join(", "),
            export: false,
            deps: load_deps(root, manifest),
            register: parent_toml.is_some(),
            parent_toml,
            baseline: None,
        };
        model.apply_loaded_sources(&collect_sources(root, &manifest.sources));
        model.apply_loaded_includes(manifest);
        model.baseline = Some(model.manifest());
        model
    }

    /// Marks the tree from the files the manifest selects, using the fewest explicit choices.
    fn apply_loaded_sources(&mut self, selected: &[PathBuf]) {
        let chosen: BTreeSet<String> = selected
            .iter()
            .map(|path| rel_str(&self.root, path))
            .collect();
        let count = self.nodes.len();
        let mut in_count = vec![0usize; count];
        for index in (1..count).rev() {
            if self.nodes[index].kind == NodeKind::Source && chosen.contains(&self.nodes[index].rel)
            {
                in_count[index] = 1;
            }
            in_count[self.nodes[index].parent] += in_count[index];
        }
        self.assign_loaded(0, false, &in_count);
    }

    fn assign_loaded(&mut self, index: usize, parent_effective: bool, in_count: &[usize]) {
        let total = self.nodes[index].src_total;
        let included = in_count[index];
        if total == 0 {
            return;
        }
        let effective = included * 2 >= total;
        if effective != parent_effective {
            self.choice[index] = Some(effective);
        }
        if included == total || included == 0 {
            return;
        }
        for position in 0..self.nodes[index].children.len() {
            let child = self.nodes[index].children[position];
            match self.nodes[child].kind {
                NodeKind::Dir => self.assign_loaded(child, effective, in_count),
                NodeKind::Source => {
                    let child_included = in_count[child] == 1;
                    if child_included != effective {
                        self.choice[child] = Some(child_included);
                    }
                }
                NodeKind::Header => {}
            }
        }
    }

    fn apply_loaded_includes(&mut self, manifest: &Manifest) {
        let (public, private) = match &manifest.include {
            Some(include) => (include.public.clone(), include.private.clone()),
            None => {
                let existing = |name: &str| {
                    if self.root.join(name).is_dir() {
                        vec![name.to_string()]
                    } else {
                        Vec::new()
                    }
                };
                (existing("include"), existing("src"))
            }
        };
        self.export = !public.is_empty();
        for dir in public.iter().chain(&private) {
            let mut rel = rel_str(&self.root, &absolute(&self.root.join(dir)));
            if rel.is_empty() {
                rel = ".".into();
            }
            let found = self
                .nodes
                .iter()
                .position(|node| node.kind == NodeKind::Dir && node.rel == rel);
            if let Some(index) = found {
                self.include[index] = true;
            }
        }
    }

    pub fn verb(&self) -> &'static str {
        if self.baseline.is_some() {
            "configure"
        } else {
            "init"
        }
    }

    /// Lists the folder as a dependency and returns its index; a folder that is already listed is reused.
    pub fn add_dep(&mut self, text: &str) -> Result<usize> {
        let dir = absolute(&self.root.join(text));
        if !dir.is_dir() {
            bail!("{} is not a folder", dir.display());
        }
        if let Some(index) = self
            .deps
            .iter()
            .position(|dep| dep.is_folder(&dir, &self.root))
        {
            return Ok(index);
        }
        let key = dir.file_name().unwrap().to_string_lossy().into_owned();
        if self.deps.iter().any(|dep| dep.key == key) {
            bail!("a dependency named '{key}' is already listed");
        }
        let path = if dir.starts_with(&self.root) {
            rel_str(&self.root, &dir)
        } else {
            text.replace('\\', "/")
        };
        let configured = dir.join("nomake.toml").is_file();
        self.deps.push(Dep {
            key,
            path,
            version: None,
            on: configured,
            configured,
            public: false,
            features: Vec::new(),
            default_features: true,
            link: None,
        });
        Ok(self.deps.len() - 1)
    }

    fn apply_source_defaults(&mut self, kind_flag: Option<Kind>, in_deps: bool) {
        let top_level = self.nodes[0].children.clone();
        for &index in &top_level {
            let node = &self.nodes[index];
            match node.kind {
                NodeKind::Dir if node.locked => {}
                NodeKind::Dir => self.choice[index] = Some(!is_skip_name(&node.name)),
                NodeKind::Source => self.choice[index] = Some(true),
                NodeKind::Header => {}
            }
        }
        for &index in &top_level {
            if self.choice[index] == Some(true) && self.nodes[index].kind == NodeKind::Dir {
                self.exclude_nested_skipped(index);
            }
        }
        self.detect_mains(0);

        let any_main = self.nodes.iter().any(|node| node.has_main);
        self.kind = match kind_flag {
            Some(kind) => kind,
            None if !in_deps && any_main => Kind::Executable,
            None => Kind::Static,
        };
        if self.kind != Kind::Executable {
            for index in 0..self.nodes.len() {
                if self.nodes[index].has_main {
                    self.choice[index] = Some(false);
                }
            }
        }
    }

    fn exclude_nested_skipped(&mut self, index: usize) {
        for position in 0..self.nodes[index].children.len() {
            let child = self.nodes[index].children[position];
            let node = &self.nodes[child];
            if node.kind != NodeKind::Dir || node.locked {
                continue;
            }
            if is_skip_name(&node.name) {
                self.choice[child] = Some(false);
            } else {
                self.exclude_nested_skipped(child);
            }
        }
    }

    fn detect_mains(&mut self, index: usize) {
        for position in 0..self.nodes[index].children.len() {
            let child = self.nodes[index].children[position];
            if self.choice[child] == Some(false) || self.nodes[child].locked {
                continue;
            }
            match self.nodes[child].kind {
                NodeKind::Source => {
                    self.nodes[child].has_main = has_main(&self.root.join(&self.nodes[child].rel))
                }
                NodeKind::Dir => self.detect_mains(child),
                NodeKind::Header => {}
            }
        }
    }

    fn apply_include_defaults(&mut self) {
        let preferred = ["include", ".", "src"]
            .iter()
            .find_map(|label| self.include_candidate(label));
        if let Some(index) = preferred {
            self.include[index] = true;
        }
    }

    fn include_candidate(&self, label: &str) -> Option<usize> {
        if label == "." {
            let direct = self.nodes[0]
                .children
                .iter()
                .any(|&child| self.nodes[child].kind == NodeKind::Header);
            return direct.then_some(0);
        }
        self.nodes[0].children.iter().copied().find(|&child| {
            let node = &self.nodes[child];
            node.kind == NodeKind::Dir
                && !node.locked
                && !is_skip_name(&node.name)
                && node.name == label
                && node.hdr_total > 0
        })
    }

    pub fn compute(&self) -> Computed {
        let count = self.nodes.len();
        let mut effective = vec![false; count];
        for index in 0..count {
            effective[index] = match self.choice[index] {
                Some(value) => value,
                None if index == 0 => false,
                None => effective[self.nodes[index].parent],
            };
        }
        let mut in_count = vec![0; count];
        for index in (1..count).rev() {
            if self.nodes[index].kind == NodeKind::Source && effective[index] {
                in_count[index] = 1;
            }
            in_count[self.nodes[index].parent] += in_count[index];
        }
        Computed {
            effective,
            in_count,
        }
    }

    pub fn state(&self, computed: &Computed, index: usize) -> State {
        let included = computed.in_count[index];
        if included == 0 {
            State::Out
        } else if included == self.nodes[index].src_total {
            State::In
        } else {
            State::Partial
        }
    }

    pub fn toggle_source(&mut self, index: usize) {
        if self.nodes[index].locked {
            return;
        }
        let computed = self.compute();
        let fully_in = computed.in_count[index] == self.nodes[index].src_total;
        for descendant in index + 1..self.nodes[index].end {
            self.choice[descendant] = None;
        }
        self.choice[index] = Some(!fully_in);
    }

    pub fn toggle_include(&mut self, index: usize) {
        self.include[index] = !self.include[index];
    }

    fn selection(&self, computed: &Computed) -> Selection {
        let mut selection = Selection {
            dirs: Vec::new(),
            files: Vec::new(),
            exclude: Vec::new(),
        };
        if computed.in_count[0] == 0 {
            return selection;
        }
        if computed.effective[0] {
            selection.dirs.push(".".into());
            self.collect_excludes(computed, 0, &mut selection);
        } else {
            self.collect_tops(computed, 0, &mut selection);
        }
        selection
    }

    /// Finds the outermost included directories and files below an excluded directory.
    fn collect_tops(&self, computed: &Computed, index: usize, selection: &mut Selection) {
        for &child in &self.nodes[index].children {
            let node = &self.nodes[child];
            if node.kind != NodeKind::Dir || node.locked || computed.in_count[child] == 0 {
                continue;
            }
            if computed.effective[child] {
                selection.dirs.push(node.rel.clone());
                self.collect_excludes(computed, child, selection);
            } else {
                self.collect_tops(computed, child, selection);
            }
        }
        self.collect_loose_files(computed, index, selection);
    }

    fn collect_loose_files(&self, computed: &Computed, index: usize, selection: &mut Selection) {
        let prefix = if index == 0 {
            String::new()
        } else {
            format!("{}/", self.nodes[index].rel)
        };
        let sources: Vec<usize> = self.nodes[index]
            .children
            .iter()
            .copied()
            .filter(|&child| self.nodes[child].kind == NodeKind::Source)
            .collect();
        let exts: BTreeSet<&str> = sources
            .iter()
            .map(|&child| ext_of(&self.nodes[child].name))
            .collect();
        for ext in exts {
            let same_ext: Vec<usize> = sources
                .iter()
                .copied()
                .filter(|&child| ext_of(&self.nodes[child].name) == ext)
                .collect();
            if same_ext.iter().all(|&child| computed.effective[child]) {
                selection.files.push(format!("{prefix}*.{ext}"));
            } else {
                let picked = same_ext.iter().filter(|&&child| computed.effective[child]);
                selection
                    .files
                    .extend(picked.map(|&child| self.nodes[child].rel.clone()));
            }
        }
    }

    /// Emits the smallest set of exclude globs that carve the excluded sources out of an included directory.
    fn collect_excludes(&self, computed: &Computed, index: usize, selection: &mut Selection) {
        for &child in &self.nodes[index].children {
            let node = &self.nodes[child];
            match node.kind {
                NodeKind::Header => {}
                NodeKind::Source => {
                    if !computed.effective[child] {
                        selection.exclude.push(node.rel.clone());
                    }
                }
                NodeKind::Dir => {
                    if node.locked || computed.in_count[child] == node.src_total {
                        continue;
                    }
                    if computed.in_count[child] == 0 {
                        selection.exclude.push(format!("{}/**", node.rel));
                    } else {
                        self.collect_excludes(computed, child, selection);
                    }
                }
            }
        }
    }

    fn package_section(&self) -> String {
        format!(
            "[package]\nname = {:?}\nversion = \"0.1.0\"\ntype = \"{}\"\ncxx_standard = {:?}\n",
            self.name,
            kind_name(self.kind),
            self.std
        )
    }

    pub fn sources_section(&self) -> String {
        let selection = self.selection(&self.compute());
        let mut out = String::from("[sources]\n");
        if !selection.dirs.is_empty() || selection.files.is_empty() {
            out += &format!("dirs = {}\n", toml_list(&selection.dirs));
        }
        if !selection.files.is_empty() {
            out += &format!("files = {}\n", toml_list(&selection.files));
        }
        if !selection.exclude.is_empty() {
            out += &format!("exclude = {}\n", toml_list(&selection.exclude));
        }
        out
    }

    pub fn include_section(&self) -> String {
        let dirs: Vec<String> = (0..self.nodes.len())
            .filter(|&index| self.include[index])
            .map(|index| self.nodes[index].rel.clone())
            .collect();
        let (public, private) = if self.export {
            (dirs, Vec::new())
        } else {
            (Vec::new(), dirs)
        };
        format!(
            "[include]\npublic = {}\nprivate = {}\n",
            toml_list(&public),
            toml_list(&private)
        )
    }

    fn build_section(&self) -> String {
        let defines = split_list(&self.defines);
        let libs = split_list(&self.libs);
        if defines.is_empty() && libs.is_empty() {
            return String::new();
        }
        let mut out = String::from("[build]\n");
        if !defines.is_empty() {
            out += &format!("defines = {}\n", toml_list(&defines));
        }
        if !libs.is_empty() {
            out += &format!("libs = {}\n", toml_list(&libs));
        }
        out
    }

    fn deps_section(&self) -> String {
        let picked: Vec<&Dep> = self.deps.iter().filter(|dep| dep.on).collect();
        if picked.is_empty() {
            return String::new();
        }
        let mut out = String::from("[dependencies]\n");
        for dep in picked {
            let mut extras = String::new();
            if dep.public {
                extras += ", public = true";
            }
            if !dep.features.is_empty() {
                extras += &format!(", features = {}", toml_list(&dep.features));
            }
            if !dep.default_features {
                extras += ", default-features = false";
            }
            if let Some(link) = dep.link {
                extras += &format!(", link = {:?}", kind_name(link));
            }
            let spec = match &dep.version {
                Some(version) => format!("version = {version:?}"),
                None => format!("path = {:?}", dep.path),
            };
            out += &format!("{} = {{ {spec}{extras} }}\n", toml_key(&dep.key));
        }
        out
    }

    pub fn manifest(&self) -> String {
        let sections = [
            self.package_section(),
            self.sources_section(),
            self.include_section(),
            self.build_section(),
            self.deps_section(),
        ];
        sections
            .into_iter()
            .filter(|section| !section.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What `write` would put in `nomake.toml`.
    pub fn result_text(&self) -> String {
        let fresh = self.manifest();
        match self.existing_manifest() {
            Some((baseline, existing)) => {
                merge_manifest(&existing, baseline, &fresh).unwrap_or(fresh)
            }
            None => fresh,
        }
    }

    fn existing_manifest(&self) -> Option<(&str, String)> {
        let baseline = self.baseline.as_deref()?;
        let existing = fs::read_to_string(self.root.join("nomake.toml")).ok()?;
        Some((baseline, existing))
    }

    /// Returns one line per file written.
    pub fn write(&self) -> Result<Vec<String>> {
        let manifest_path = self.root.join("nomake.toml");
        let fresh = self.manifest();
        let text = match self.existing_manifest() {
            Some((baseline, existing)) => merge_manifest(&existing, baseline, &fresh)?,
            None => fresh,
        };
        fs::write(&manifest_path, text)?;
        let mut written = vec![format!("wrote {}", manifest_path.display())];
        if let (true, Some(parent_toml)) = (self.register, &self.parent_toml) {
            self.register_in_parent(parent_toml)?;
            written.push(format!("updated {}", parent_toml.display()));
        }
        Ok(written)
    }

    /// Adds `deps/<dir>` to the parent project's `[dependencies]`.
    fn register_in_parent(&self, parent_toml: &Path) -> Result<()> {
        let dir = self.root.file_name().unwrap().to_string_lossy();
        let text = fs::read_to_string(parent_toml)?;
        let line = format!("{} = {{ path = \"deps/{dir}\" }}", toml_key(&self.name));
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        match lines
            .iter()
            .position(|existing| existing.trim() == "[dependencies]")
        {
            Some(index) => lines.insert(index + 1, line),
            None => {
                lines.push(String::new());
                lines.push("[dependencies]".into());
                lines.push(line);
            }
        }
        fs::write(parent_toml, lines.join("\n") + "\n")?;
        Ok(())
    }
}

fn table_keys(doc: &DocumentMut, table: &str) -> Vec<String> {
    let Some(table) = doc.get(table).and_then(|item| item.as_table_like()) else {
        return Vec::new();
    };
    table.iter().map(|(key, _)| key.to_string()).collect()
}

fn value_text(doc: &DocumentMut, table: &str, key: &str) -> Option<String> {
    doc.get(table)?
        .as_table_like()?
        .get(key)
        .map(|item| item.to_string())
}

fn set_value(doc: &mut DocumentMut, table: &str, key: &str, value: Option<&Item>) {
    match value {
        Some(item) => {
            let target = doc.entry(table).or_insert(toml_edit::table());
            if let Some(target) = target.as_table_like_mut() {
                target.insert(key, item.clone());
            }
        }
        None => {
            if let Some(target) = doc.get_mut(table).and_then(|item| item.as_table_like_mut()) {
                target.remove(key);
            }
        }
    }
    let emptied = doc
        .get(table)
        .and_then(|item| item.as_table_like())
        .is_some_and(|target| target.is_empty());
    if emptied {
        doc.remove(table);
    }
}

/// Applies only the keys whose rendering changed since `baseline`, so everything else in `existing` survives as written.
fn merge_manifest(existing: &str, baseline: &str, fresh: &str) -> Result<String> {
    let mut doc: DocumentMut = existing.parse()?;
    let before: DocumentMut = baseline.parse()?;
    let after: DocumentMut = fresh.parse()?;
    for (table, managed) in MANAGED {
        let mut keys: Vec<String> = managed.iter().map(|key| key.to_string()).collect();
        if keys.is_empty() {
            keys = table_keys(&before, table);
            keys.extend(table_keys(&after, table));
            keys.sort();
            keys.dedup();
        }
        for key in keys {
            if value_text(&before, table, &key) == value_text(&after, table, &key) {
                continue;
            }
            let value = after
                .get(table)
                .and_then(|item| item.as_table_like())
                .and_then(|target| target.get(&key));
            set_value(&mut doc, table, &key, value);
        }
    }
    Ok(doc.to_string())
}

fn report(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

pub fn init(yes: bool, kind_flag: Option<Kind>) -> Result<()> {
    let root = absolute(Path::new("."));
    let manifest_path = root.join("nomake.toml");
    if manifest_path.exists() {
        bail!(
            "{} already exists (use `nomake configure` to edit it)",
            manifest_path.display()
        );
    }
    let mut model = Model::scan(&root, kind_flag);
    if yes {
        println!("--- nomake.toml ---\n{}-----------------", model.manifest());
    } else if !tui::run(&mut model)? {
        println!("aborted");
        return Ok(());
    }
    report(model.write()?);
    Ok(())
}

pub fn configure() -> Result<()> {
    let mut model = Model::open(&absolute(Path::new(".")), None)?;
    if !tui::run(&mut model)? {
        println!("aborted");
        return Ok(());
    }
    report(model.write()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "# my project
[package]
name = \"demo\"
version = \"1.2.3\"
type = \"executable\"
cxx_standard = 17

[sources]
dirs = [\"src\"]

[build]
flags = [\"-Wall\"]   # keep
defines = [\"A\"]

[toolchain]
cxx = \"clang++\"

[target.windows]
sources.files = [\"win.cpp\"]
";

    fn project(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nomake_init_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for file in ["src/a.cpp", "src/b.cpp", "src/sub/c.cpp", "include/x.h"] {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        fs::write(root.join("nomake.toml"), MANIFEST).unwrap();
        root
    }

    #[test]
    fn untouched_configure_keeps_the_file_byte_for_byte() {
        let root = project("same");
        let model = Model::open(&root, None).unwrap();
        model.write().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("nomake.toml")).unwrap(),
            MANIFEST
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configure_edits_only_what_changed() {
        let root = project("edit");
        let mut model = Model::open(&root, None).unwrap();
        model.defines = "A, B".into();
        let index = model
            .nodes
            .iter()
            .position(|node| node.rel == "src/b.cpp")
            .unwrap();
        model.toggle_source(index);
        model.write().unwrap();
        let text = fs::read_to_string(root.join("nomake.toml")).unwrap();
        for expected in [
            "# my project",
            "version = \"1.2.3\"",
            "flags = [\"-Wall\"]   # keep",
            "[toolchain]",
            "[target.windows]\nsources.files = [\"win.cpp\"]",
            "defines = [\"A\", \"B\"]",
            "exclude = [\"src/b.cpp\"]",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependencies_are_listed_and_registered() {
        let root = project("deps");
        fs::create_dir_all(root.join("deps/lib")).unwrap();
        fs::write(
            root.join("deps/lib/nomake.toml"),
            "[package]\nname = \"lib\"\ntype = \"static\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("deps/raw")).unwrap();
        let mut model = Model::open(&root, None).unwrap();
        assert_eq!(model.deps.len(), 2);
        assert!(model.deps[0].configured && !model.deps[1].configured);
        model.deps[0].on = true;
        model.write().unwrap();
        let text = fs::read_to_string(root.join("nomake.toml")).unwrap();
        assert!(
            text.contains("[dependencies]\nlib = { path = \"deps/lib\" }"),
            "{text}"
        );
        assert!(text.contains("# my project"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_dependencies_survive_configure() {
        let root = project("registry");
        fs::write(
            root.join("nomake.toml"),
            format!("{MANIFEST}\n[dependencies]\nlua = {{ version = \"5.4\", public = true, features = [\"jit\"], default-features = false }}\n"),
        )
        .unwrap();
        let model = Model::open(&root, None).unwrap();
        assert_eq!(model.deps[0].version.as_deref(), Some("5.4"));
        model.write().unwrap();
        let text = fs::read_to_string(root.join("nomake.toml")).unwrap();
        assert!(text.contains("lua = { version = \"5.4\", public = true, features = [\"jit\"], default-features = false }"), "{text}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn added_folder_outside_deps_is_kept_relative() {
        let root = project("add");
        let mut model = Model::open(&root, None).unwrap();
        let index = model.add_dep("src/sub").unwrap();
        assert_eq!(model.deps[index].path, "src/sub");
        assert!(model.add_dep("missing").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
