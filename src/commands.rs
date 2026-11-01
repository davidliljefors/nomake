use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::compdb;
use crate::config::{Kind, Manifest};
use crate::dag::{self, Graph};
use crate::executor;
use crate::scanner;
use crate::state::State;
use crate::testrun::{self, Test};
use crate::toolchain::{Arch, Toolchain};
use crate::util::absolute;
use crate::Result;

pub struct BuildOpts {
    pub release: bool,
    pub arch: Arch,
    pub jobs: Option<usize>,
    pub verbose: bool,
    pub debug: executor::Debug,
    /// Package folders to build together; empty means the package in the current directory.
    pub roots: Vec<PathBuf>,
    /// Keep an existing compile_commands.json current; off when only some of a workspace is built.
    pub refresh_compdb: bool,
}

pub struct Built {
    pub ok: bool,
    pub target: PathBuf,
    pub targets: Vec<(PathBuf, PathBuf)>,
    pub kind: Kind,
}

fn profile(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

pub fn out_dir(root_dir: &Path, arch: Arch, release: bool) -> PathBuf {
    root_dir
        .join("build")
        .join(format!("{}-{}", arch.name(), profile(release)))
}

fn compdb_stamp(root_dir: &Path) -> PathBuf {
    root_dir.join("build").join(".nomake_compdb")
}

/// Plans the build of `roots` (the current directory's package when empty) in the current directory's build/.
fn plan(
    arch: Arch,
    release: bool,
    roots: &[PathBuf],
) -> Result<(scanner::Workspace, Toolchain, Graph)> {
    let root_dir = absolute(Path::new("."));
    let ws = if roots.is_empty() {
        scanner::load(&root_dir)?
    } else {
        scanner::load_roots(&root_dir, roots)?
    };
    let tc = Toolchain::detect(&ws.root().toolchain, arch, &root_dir.join("build"))?;
    let graph = dag::build(
        &ws,
        &tc,
        release,
        &root_dir,
        &out_dir(&root_dir, arch, release),
    )?;
    Ok((ws, tc, graph))
}

pub fn build(opts: &BuildOpts) -> Result<Built> {
    let start = Instant::now();
    let root_dir = absolute(Path::new("."));
    let manifest = root_dir.join("nomake.toml");
    if !manifest.is_file() {
        bail!(
            "{} not found (run `nomake init` in {})",
            manifest.display(),
            root_dir.display()
        );
    }
    let dir = out_dir(&root_dir, opts.arch, opts.release);
    // The state file is independent of the plan, so the two load side by side.
    let (planned, state) = rayon::join(
        || plan(opts.arch, opts.release, &opts.roots),
        || -> Result<State> {
            fs::create_dir_all(&dir)?;
            State::open(&dir.join(".nomake_state"))
        },
    );
    let (ws, tc, graph) = planned?;
    let state = state?;
    if opts.refresh_compdb {
        compdb::refresh(&graph, &compdb_stamp(&root_dir))?;
    }
    let jobs = opts
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |count| count.get()));

    let root = ws.root();
    let title = if ws.roots.len() > 1 {
        format!(
            "workspace {} ({} packages)",
            root_dir.file_name().unwrap_or_default().to_string_lossy(),
            ws.roots.len()
        )
    } else {
        format!("{} v{}", root.name, root.version)
    };
    println!(
        "nomake: {title} [{} {}] with {}",
        opts.arch.name(),
        profile(opts.release),
        tc.cxx.file_name().unwrap().to_string_lossy()
    );
    let summary = executor::run(&graph, &state, jobs, opts.verbose, opts.debug)?;
    if !summary.ok {
        eprintln!("nomake: build failed");
    } else if summary.ran == 0 {
        println!("nomake: no work to do");
    } else {
        println!(
            "nomake: finished {} steps in {:.2?}",
            summary.ran,
            start.elapsed()
        );
    }
    let kind = root.kind;
    let target = graph.target.clone();
    let targets = graph.targets.clone();
    // Freeing 10k nodes is pure latency; let it overlap the exit or the program `run` starts.
    std::thread::spawn(move || drop((graph, ws)));
    Ok(Built {
        ok: summary.ok,
        target,
        targets,
        kind,
    })
}

/// Builds and runs the test package in the current directory.
pub fn test(opts: BuildOpts, extra: &[String]) -> Result<i32> {
    let dir = absolute(Path::new("."));
    let manifest = Manifest::load_resolved(&dir.join("nomake.toml"))?;
    if !manifest.package.is_test() {
        bail!(
            "package '{}' is not a test (set type = \"test\")",
            manifest.package.name
        );
    }
    let test = Test {
        name: manifest.package.name.clone(),
        dir,
        config: manifest.test.clone(),
    };
    testrun::run(opts, &[test], extra)
}

/// Builds and runs the executable, from `cwd` when given (the package's folder, in a workspace).
pub fn run(opts: &BuildOpts, args: &[String], cwd: Option<&Path>) -> Result<i32> {
    let built = build(opts)?;
    if !built.ok {
        return Ok(1);
    }
    if built.kind != Kind::Executable {
        bail!("package is not an executable");
    }
    let mut command = Command::new(&built.target);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

/// Removes one configuration's build directory, or everything under build/ except the VS solution.
pub fn clean(config: Option<(Arch, bool)>) -> Result<()> {
    let root = absolute(Path::new("."));
    if !root.join("nomake.toml").is_file() {
        bail!("no nomake.toml in {}", root.display());
    }
    let build = root.join("build");
    if let Some((arch, release)) = config {
        let dir = out_dir(&root, arch, release);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
        }
        println!("nomake: removed {}", dir.display());
        return Ok(());
    }
    if build.is_dir() {
        for entry in fs::read_dir(&build)? {
            let path = entry?.path();
            // The Visual Studio solution lives here and may be open while VS runs the clean.
            if path.file_name().is_some_and(|name| name == "vs") {
                continue;
            }
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    println!("nomake: removed {}", build.display());
    Ok(())
}

pub fn compdb(arch: Arch, release: bool, roots: &[PathBuf]) -> Result<()> {
    let (_, _, graph) = plan(arch, release, roots)?;
    let count = compdb::generate(&graph, &compdb_stamp(&graph.root_dir))?;
    println!("nomake: wrote compile_commands.json ({count} entries)");
    Ok(())
}

pub fn new(name: &str, kind: Kind) -> Result<()> {
    let dir = Path::new(name);
    if dir.exists() {
        bail!("{name} already exists");
    }
    fs::create_dir_all(dir.join("src"))?;
    let kind_str = match kind {
        Kind::Executable => "executable",
        Kind::Test => "test",
        Kind::Static => "static",
        Kind::Shared => "shared",
    };
    fs::write(
        dir.join("nomake.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"{kind_str}\"\ncxx_standard = \"20\"\n"),
    )?;
    if kind == Kind::Executable {
        fs::write(
            dir.join("src/main.cpp"),
            "#include <cstdio>\n\nint main() {\n    std::puts(\"hello from nomake\");\n}\n",
        )?;
    } else {
        fs::create_dir_all(dir.join("include"))?;
        fs::write(
            dir.join("include").join(format!("{name}.h")),
            "#pragma once\n\nint answer();\n",
        )?;
        fs::write(
            dir.join("src").join(format!("{name}.cpp")),
            format!("#include <{name}.h>\n\nint answer() {{ return 42; }}\n"),
        )?;
    }
    fs::write(dir.join(".gitignore"), "/build\n/compile_commands.json\n")?;
    println!("nomake: created {name}");
    Ok(())
}
