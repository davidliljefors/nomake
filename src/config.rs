use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::Result;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Executable,
    Static,
    Shared,
    /// An executable that `nomake test` runs; the build treats it as `Executable`.
    Test,
}

/// `cxx_standard = "20"` and `cxx_standard = 20` are both accepted.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Std {
    Str(String),
    Int(i64),
}

impl Std {
    pub fn get(&self) -> String {
        match self {
            Std::Str(s) => s.clone(),
            Std::Int(i) => i.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: Package,
    /// Makes this folder the root of a workspace; read by `workspace::find`, which is lenient about the rest.
    #[allow(dead_code)]
    pub workspace: Option<WorkspaceDef>,
    #[serde(default)]
    pub sources: Sources,
    pub include: Option<Include>,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub toolchain: Toolchain,
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    pub generate: Vec<Generate>,
    /// Per-OS additions, applied by `resolve`; keyed by `windows`, `macos`, `linux` or `unix`.
    #[serde(default)]
    pub target: BTreeMap<String, Target>,
    /// How `nomake test` runs a package of type `test`.
    #[serde(default)]
    pub test: TestConfig,
    /// Which features are on unless a dependent opts out.
    #[serde(default)]
    pub features: Features,
    /// Optional parts of the package, switched on by `enable`.
    #[serde(default)]
    pub feature: BTreeMap<String, Feature>,
    /// Present in registry recipes: where to fetch the package's sources.
    pub source: Option<Source>,
    /// Dependency keys of the root project named by enabled features; filled in by `enable`.
    #[serde(skip)]
    pub peers: BTreeSet<String>,
}

/// `[workspace]`: package folders built together. An entry ending in `/*` takes every subfolder with a nomake.toml.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDef {
    #[serde(default)]
    pub members: Vec<String>,
}

/// `[test]`: how a package of type `test` is run. It passes when it exits with 0.
#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct TestConfig {
    /// Seconds before the run is killed and counted as failed (default 60).
    pub timeout: Option<u64>,
    #[serde(default)]
    pub args: Vec<String>,
}

pub const DEFAULT_TEST_TIMEOUT: u64 = 60;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Features {
    #[serde(default)]
    pub default: Vec<String>,
}

/// What a `[feature.<name>]` table adds when the feature is on. Features cannot add dependencies, but
/// they can name peers: packages the root project depends on whose headers this package compiles against.
#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    #[serde(default)]
    pub peer: Vec<String>,
    #[serde(default)]
    pub sources: Sources,
    pub include: Option<Include>,
    #[serde(default)]
    pub build: Build,
}

/// What a `[target.<os>]` table may add on top of the base manifest.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Target {
    #[serde(default)]
    pub sources: Sources,
    pub include: Option<Include>,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub dependencies: BTreeMap<String, TargetDependency>,
    /// Features that only exist on this OS; they add to a base feature of the same name.
    #[serde(default)]
    pub feature: BTreeMap<String, Feature>,
}

/// A dependency entry inside a target table; it merges into the base entry of the same name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetDependency {
    pub path: Option<String>,
    pub version: Option<String>,
    pub public: Option<bool>,
    #[serde(default)]
    pub features: Vec<String>,
}

const TARGET_NAMES: [&str; 4] = ["windows", "macos", "linux", "unix"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_types", rename = "type")]
    pub types: Types,
    #[serde(default = "default_std")]
    pub cxx_standard: Std,
    #[serde(skip)]
    chosen: Option<Kind>,
}

/// `type = "static"`, or for a library that can be built either way `type = ["shared", "static"]`;
/// the first entry is what dependents get unless they ask for another with `link`.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Types {
    One(Kind),
    Many(Vec<Kind>),
}

impl Types {
    pub fn list(&self) -> Vec<Kind> {
        match self {
            Types::One(kind) => vec![*kind],
            Types::Many(kinds) => kinds.clone(),
        }
    }
}

impl Package {
    /// What this package is built as: the dependents' choice, else the first listed type. A test is an executable.
    pub fn kind(&self) -> Kind {
        let kind = self.chosen.unwrap_or_else(|| {
            self.types
                .list()
                .first()
                .copied()
                .unwrap_or(Kind::Executable)
        });
        if kind == Kind::Test {
            Kind::Executable
        } else {
            kind
        }
    }

    pub fn is_test(&self) -> bool {
        self.types.list() == [Kind::Test]
    }

    fn check(&self) -> Result<()> {
        let kinds = self.types.list();
        let libraries = kinds
            .iter()
            .all(|kind| !matches!(kind, Kind::Executable | Kind::Test));
        let unique = kinds
            .iter()
            .enumerate()
            .all(|(at, kind)| !kinds[..at].contains(kind));
        if kinds.is_empty() || (kinds.len() > 1 && !libraries) || !unique {
            bail!("type must be one kind, or a list of different libraries such as [\"shared\", \"static\"]");
        }
        Ok(())
    }
}

fn default_version() -> String {
    "0.1.0".into()
}
fn default_types() -> Types {
    Types::One(Kind::Executable)
}
fn default_std() -> Std {
    Std::Int(20)
}

/// Package-relative source selection. `dirs` are scanned recursively; `files` are globs.
#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    pub dirs: Option<Vec<String>>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Include {
    /// Exported to every package that depends on this one.
    #[serde(default)]
    pub public: Vec<String>,
    #[serde(default)]
    pub private: Vec<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Crt {
    #[default]
    Dynamic,
    Static,
}

impl Crt {
    pub fn name(self) -> &'static str {
        match self {
            Crt::Dynamic => "dynamic",
            Crt::Static => "static",
        }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Build {
    #[serde(default)]
    pub defines: Vec<String>,
    #[serde(default)]
    pub public_defines: Vec<String>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub link_flags: Vec<String>,
    #[serde(default)]
    pub libs: Vec<String>,
    /// How the C runtime is linked: `dynamic` (default) or `static`. One process needs one runtime, so every
    /// package in a build has to agree.
    pub crt: Option<Crt>,
    /// `console` or `windows`; COFF linkers only.
    pub subsystem: Option<String>,
    /// Applied only when the package is built as a shared library, e.g. a `DLL_EXPORT` switch.
    #[serde(default)]
    pub shared_defines: Vec<String>,
    /// Header precompiled once and used by every C++ source in the package (clang only).
    pub pch: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    pub cxx: Option<String>,
    pub ar: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    /// A folder, relative to this package. Exactly one of `path` and `version` is set.
    pub path: Option<String>,
    /// A version (or version prefix) of a package from the registry.
    pub version: Option<String>,
    /// Re-exports this dependency's headers and defines to whoever depends on this package.
    #[serde(default)]
    pub public: bool,
    /// Features of the dependency to switch on, besides its defaults.
    #[serde(default)]
    pub features: Vec<String>,
    /// `false` leaves out the dependency's default features.
    #[serde(default = "yes", rename = "default-features")]
    pub default_features: bool,
    /// Which of the library's types to build, when it offers several; executables take no `link`.
    pub link: Option<Kind>,
}

fn yes() -> bool {
    true
}

/// Where a recipe's sources come from: a git repository and the full commit hash to use.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub git: String,
    pub rev: String,
}

impl Source {
    fn check(&self) -> Result<()> {
        if self.rev.len() != 40 || !self.rev.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "[source] rev must be a full 40-character commit hash, not '{}'",
                self.rev
            );
        }
        Ok(())
    }
}

/// One invocation per matched input, of either `command` (placeholders: {input} {output} {out_dir} {stem}
/// {depfile}) or `tool`, a dependency that is built first and called as
/// `tool --out-dir <dir> --depfile <file> -I<include dir of each dependency>... <input>`.
/// A tool writes a Makefile-style depfile naming every other file it read.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    /// Names the step in build output; a tool step defaults to the tool's name.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Run once for all matched inputs instead of once per input.
    #[serde(default)]
    pub batch: bool,
}

impl Generate {
    pub fn label(&self) -> &str {
        match (&self.tool, self.name.is_empty()) {
            (Some(tool), true) => tool,
            _ => &self.name,
        }
    }

    fn check(&self) -> Result<()> {
        match (&self.tool, self.command.is_empty()) {
            (Some(_), false) => bail!("[[generate]] takes either `tool` or `command`, not both"),
            (None, true) => bail!("[[generate]] needs `tool` or `command`"),
            _ => {}
        }
        if self.label().is_empty() {
            bail!("[[generate]] with a command needs a `name`");
        }
        if self.outputs.is_empty() {
            bail!("[[generate]] '{}' needs `outputs`", self.label());
        }
        if self.tool.is_some() && self.batch {
            bail!(
                "[[generate]] '{}': a tool step runs once per input",
                self.label()
            );
        }
        Ok(())
    }
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest> {
        let text = fs::read_to_string(path)?;
        let manifest: Manifest =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        manifest
            .check()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(manifest)
    }

    /// Loads the manifest as it applies to the host OS, with its target tables merged in.
    pub fn load_resolved(path: &Path) -> Result<Manifest> {
        let mut manifest = Manifest::load(path)?;
        manifest
            .resolve(std::env::consts::OS)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(manifest)
    }

    /// Merges the `unix` table (on anything but Windows) and then the table for `os`; lists add up.
    pub fn resolve(&mut self, os: &str) -> Result<()> {
        if let Some(name) = self
            .target
            .keys()
            .find(|name| !TARGET_NAMES.contains(&name.as_str()))
        {
            return Err(format!(
                "unknown target '{name}' (expected windows, macos, linux or unix)"
            )
            .into());
        }
        let targets = std::mem::take(&mut self.target);

        // The defaults only apply when nothing, on any platform or in any feature, says otherwise, so they
        // don't differ per OS or per feature set.
        let names_sources = |s: &Sources| s.dirs.is_some() || !s.files.is_empty();
        let features = || {
            self.feature
                .values()
                .chain(targets.values().flat_map(|t| t.feature.values()))
        };
        let (in_overlays, include_in_overlays) = (
            targets.values().any(|t| names_sources(&t.sources))
                || features().any(|f| names_sources(&f.sources)),
            targets.values().any(|t| t.include.is_some())
                || features().any(|f| f.include.is_some()),
        );
        if !names_sources(&self.sources) {
            self.sources.dirs = Some(if in_overlays {
                Vec::new()
            } else {
                vec!["src".into()]
            });
        }
        if self.include.is_none() && include_in_overlays {
            self.include = Some(Include::default());
        }

        let mut order = Vec::new();
        if os != "windows" {
            order.push("unix");
        }
        order.push(os);
        for name in order {
            if let Some(target) = targets.get(name) {
                self.merge(name, target)?;
            }
        }
        Ok(())
    }

    /// Switches on `requested` features, plus the default ones when `defaults` is set. Needs `resolve` first.
    pub fn enable(&mut self, requested: &BTreeSet<String>, defaults: bool) -> Result<()> {
        let available = std::mem::take(&mut self.feature);
        let default_names = if defaults {
            self.features.default.clone()
        } else {
            Vec::new()
        };
        let mut done = BTreeSet::new();
        for name in default_names.iter().chain(requested) {
            if !done.insert(name) {
                continue;
            }
            let Some(feature) = available.get(name) else {
                bail!("package '{}' has no feature '{name}'", self.package.name);
            };
            self.peers.extend(feature.peer.iter().cloned());
            add_sources(&mut self.sources, &feature.sources);
            add_include(&mut self.include, &feature.include);
            add_build(&mut self.build, &feature.build);
        }
        Ok(())
    }

    fn merge(&mut self, name: &str, target: &Target) -> Result<()> {
        add_sources(&mut self.sources, &target.sources);
        add_include(&mut self.include, &target.include);
        add_build(&mut self.build, &target.build);
        for (feature_name, extra) in &target.feature {
            let feature = self.feature.entry(feature_name.clone()).or_default();
            add_sources(&mut feature.sources, &extra.sources);
            add_include(&mut feature.include, &extra.include);
            add_build(&mut feature.build, &extra.build);
            feature.peer.extend(extra.peer.iter().cloned());
        }
        for (key, extra) in &target.dependencies {
            match self.dependencies.get_mut(key) {
                Some(dep) => {
                    let differs = |mine: &Option<String>, theirs: &Option<String>| {
                        theirs.is_some() && mine != theirs
                    };
                    if differs(&dep.path, &extra.path) || differs(&dep.version, &extra.version) {
                        bail!("[target.{name}.dependencies] {key}: conflicts with the entry in [dependencies]");
                    }
                    dep.public = extra.public.unwrap_or(dep.public);
                    dep.features.extend(extra.features.iter().cloned());
                }
                None => {
                    let dep = Dependency {
                        path: extra.path.clone(),
                        version: extra.version.clone(),
                        public: extra.public.unwrap_or(false),
                        features: extra.features.clone(),
                        default_features: true,
                        link: None,
                    };
                    if dep.path.is_some() == dep.version.is_some() {
                        bail!(
                            "[target.{name}.dependencies] {key} needs exactly one of path and version"
                        );
                    }
                    self.dependencies.insert(key.clone(), dep);
                }
            }
        }
        Ok(())
    }

    fn check(&self) -> Result<()> {
        self.package.check()?;
        if !self.package.is_test() && (self.test.timeout.is_some() || !self.test.args.is_empty()) {
            bail!("[test] needs type = \"test\"");
        }
        if let Some(source) = &self.source {
            source.check()?;
        }
        for step in &self.generate {
            step.check()?;
        }
        for (key, dep) in &self.dependencies {
            if dep.path.is_some() == dep.version.is_some() {
                bail!("dependency '{key}' needs exactly one of path and version");
            }
            if matches!(dep.link, Some(Kind::Executable | Kind::Test)) {
                bail!("dependency '{key}': link must be \"static\" or \"shared\"");
            }
        }
        Ok(())
    }

    /// Fixes what the package is built as: the `link` its dependents ask for, else its first type.
    pub fn link(&mut self, wanted: Option<Kind>) -> Result<()> {
        if let Some(kind) = wanted {
            if !self.package.types.list().contains(&kind) {
                bail!(
                    "package '{}' cannot be built as {kind:?}; it offers {:?}",
                    self.package.name,
                    self.package.types.list()
                );
            }
            self.package.chosen = Some(kind);
        }
        if self.package.kind() == Kind::Shared {
            let defines = self.build.shared_defines.clone();
            self.build.defines.extend(defines);
        }
        Ok(())
    }
}

fn add_sources(base: &mut Sources, extra: &Sources) {
    if let Some(dirs) = &extra.dirs {
        base.dirs
            .get_or_insert_with(Vec::new)
            .extend(dirs.iter().cloned());
    }
    base.files.extend(extra.files.iter().cloned());
    base.exclude.extend(extra.exclude.iter().cloned());
}

fn add_include(base: &mut Option<Include>, extra: &Option<Include>) {
    match (base.as_mut(), extra) {
        (Some(base), Some(extra)) => {
            base.public.extend(extra.public.iter().cloned());
            base.private.extend(extra.private.iter().cloned());
        }
        (None, Some(extra)) => *base = Some(extra.clone()),
        _ => {}
    }
}

fn add_build(base: &mut Build, extra: &Build) {
    base.defines.extend(extra.defines.iter().cloned());
    base.public_defines
        .extend(extra.public_defines.iter().cloned());
    base.flags.extend(extra.flags.iter().cloned());
    base.link_flags.extend(extra.link_flags.iter().cloned());
    base.libs.extend(extra.libs.iter().cloned());
    base.shared_defines
        .extend(extra.shared_defines.iter().cloned());
    if extra.pch.is_some() {
        base.pch = extra.pch.clone();
    }
    if extra.crt.is_some() {
        base.crt = extra.crt;
    }
    if extra.subsystem.is_some() {
        base.subsystem = extra.subsystem.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packages_build_as_executables() {
        let text =
            "[package]\nname = \"t\"\ntype = \"test\"\n\n[test]\ntimeout = 5\nargs = [\"-q\"]\n";
        let manifest: Manifest = toml::from_str(text).unwrap();
        manifest.check().unwrap();
        assert!(manifest.package.is_test());
        assert_eq!(manifest.package.kind(), Kind::Executable);
        assert_eq!(manifest.test.timeout, Some(5));

        let plain: Manifest =
            toml::from_str("[package]\nname = \"a\"\n\n[test]\ntimeout = 5\n").unwrap();
        assert!(plain.check().is_err());
        let mixed: Manifest =
            toml::from_str("[package]\nname = \"a\"\ntype = [\"static\", \"test\"]\n").unwrap();
        assert!(mixed.check().is_err());
    }

    const FEATURES: &str = r#"
[package]
name = "imgui"

[sources]
files = ["imgui.cpp"]

[features]
default = ["dx11"]

[feature.dx11]
sources.files = ["dx11.cpp"]
build.libs = ["d3d11"]

[feature.opengl]
sources.files = ["gl.cpp"]

[target.windows.feature.dx11]
build.defines = ["DX11_WIN"]
"#;

    fn with_features(requested: &[&str], defaults: bool) -> Result<Manifest> {
        let mut manifest: Manifest = toml::from_str(FEATURES).unwrap();
        manifest.resolve("windows")?;
        manifest.enable(
            &requested.iter().map(|name| name.to_string()).collect(),
            defaults,
        )?;
        Ok(manifest)
    }

    #[test]
    fn features_add_to_the_package_when_enabled() {
        let default = with_features(&[], true).unwrap();
        assert_eq!(default.sources.files, ["imgui.cpp", "dx11.cpp"]);
        assert_eq!(
            (default.build.libs, default.build.defines),
            (vec!["d3d11".to_string()], vec!["DX11_WIN".to_string()])
        );
        assert_eq!(
            with_features(&["opengl"], false).unwrap().sources.files,
            ["imgui.cpp", "gl.cpp"]
        );
        assert_eq!(
            with_features(&["opengl", "dx11"], true)
                .unwrap()
                .sources
                .files,
            ["imgui.cpp", "dx11.cpp", "gl.cpp"]
        );
        assert!(with_features(&["vulkan"], true).is_err());
        let mut linux: Manifest = toml::from_str(FEATURES).unwrap();
        linux.resolve("linux").unwrap();
        linux.enable(&BTreeSet::new(), true).unwrap();
        assert!(linux.build.defines.is_empty());
    }

    #[test]
    fn features_cannot_add_dependencies() {
        let text = "[package]\nname = \"x\"\n[feature.f.dependencies]\na = { path = \"a\" }";
        assert!(toml::from_str::<Manifest>(text).is_err());
    }

    const TEXT: &str = r#"
[package]
name = "lib"

[sources]
files = ["common.c"]

[dependencies]
a = { path = "deps/a" }

[target.windows]
sources.files = ["win.c"]
build.defines = ["WIN"]

[target.unix]
sources.files = ["unix.c"]

[target.macos]
sources.files = ["mac.c"]

[target.windows.dependencies]
a = { public = true }
w = { path = "deps/w" }
"#;

    fn resolved(os: &str) -> Manifest {
        let mut manifest: Manifest = toml::from_str(TEXT).unwrap();
        manifest.resolve(os).unwrap();
        manifest
    }

    #[test]
    fn target_tables_add_to_the_base_for_their_os() {
        assert_eq!(resolved("windows").sources.files, ["common.c", "win.c"]);
        assert_eq!(resolved("linux").sources.files, ["common.c", "unix.c"]);
        assert_eq!(
            resolved("macos").sources.files,
            ["common.c", "unix.c", "mac.c"]
        );
        assert_eq!(resolved("windows").build.defines, ["WIN"]);
        assert!(resolved("linux").build.defines.is_empty());
    }

    #[test]
    fn target_dependencies_merge_by_name() {
        let windows = resolved("windows");
        assert!(
            windows.dependencies["a"].public
                && windows.dependencies["a"].path.as_deref() == Some("deps/a")
        );
        assert_eq!(windows.dependencies["w"].path.as_deref(), Some("deps/w"));
        assert!(!resolved("linux").dependencies.contains_key("w"));
    }

    #[test]
    fn default_source_dir_ignores_which_os_adds_files() {
        let mut manifest: Manifest =
            toml::from_str("[package]\nname = \"x\"\n[target.windows]\nsources.files = [\"w.c\"]")
                .unwrap();
        manifest.resolve("linux").unwrap();
        assert_eq!(manifest.sources.dirs.as_deref(), Some(&[][..]));
        assert!(manifest.sources.files.is_empty());
        let mut manifest: Manifest = toml::from_str("[package]\nname = \"x\"").unwrap();
        manifest.resolve("linux").unwrap();
        assert_eq!(
            manifest.sources.dirs.as_deref(),
            Some(&["src".to_string()][..])
        );
    }

    #[test]
    fn libraries_offer_types_and_dependents_choose() {
        let text = "[package]\nname = \"sdl\"\ntype = [\"shared\", \"static\"]\n[build]\nshared_defines = [\"EXPORT\"]\n";
        let build = |link: Option<Kind>| {
            let mut manifest: Manifest = toml::from_str(text).unwrap();
            manifest.check().unwrap();
            manifest.link(link).map(|_| manifest)
        };
        let default = build(None).unwrap();
        assert_eq!(default.package.kind(), Kind::Shared);
        assert_eq!(default.build.defines, ["EXPORT"]);
        let fixed = build(Some(Kind::Static)).unwrap();
        assert_eq!(fixed.package.kind(), Kind::Static);
        assert!(fixed.build.defines.is_empty());
        let mut only: Manifest =
            toml::from_str("[package]\nname = \"lua\"\ntype = \"static\"").unwrap();
        assert!(only.link(Some(Kind::Shared)).is_err());
        let mixed: Manifest =
            toml::from_str("[package]\nname = \"x\"\ntype = [\"executable\", \"static\"]").unwrap();
        assert!(mixed.check().is_err());
    }

    #[test]
    fn bad_target_tables_are_rejected() {
        let mut manifest: Manifest =
            toml::from_str("[package]\nname = \"x\"\n[target.beos]\n").unwrap();
        assert!(manifest.resolve("linux").is_err());
        let mut manifest: Manifest =
            toml::from_str("[package]\nname = \"x\"\n[dependencies]\na = { path = \"a\" }\n[target.unix.dependencies]\na = { path = \"b\" }").unwrap();
        assert!(manifest.resolve("linux").is_err());
    }
}
