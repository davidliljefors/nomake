use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::compdb;
use crate::config::Kind;
use crate::dag::{self, Graph};
use crate::executor;
use crate::scanner;
use crate::state::State;
use crate::toolchain::{Arch, Toolchain};
use crate::util::absolute;
use crate::Result;

pub struct BuildOpts {
    pub release: bool,
    pub arch: Arch,
    pub jobs: Option<usize>,
    pub verbose: bool,
    pub debug: executor::Debug,
}

pub struct Built {
    pub ok: bool,
    pub target: PathBuf,
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

fn plan(arch: Arch, release: bool) -> Result<(scanner::Workspace, Toolchain, Graph)> {
    let root_dir = absolute(Path::new("."));
    let ws = scanner::load(&root_dir)?;
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
        || plan(opts.arch, opts.release),
        || -> Result<State> {
            fs::create_dir_all(&dir)?;
            State::open(&dir.join(".nomake_state"))
        },
    );
    let (ws, tc, graph) = planned?;
    let state = state?;
    compdb::refresh(&graph, &compdb_stamp(&root_dir))?;
    let jobs = opts
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |count| count.get()));

    let root = ws.root();
    println!(
        "nomake: {} v{} [{} {}] with {}",
        root.name,
        root.version,
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
    // Freeing 10k nodes is pure latency; let it overlap the exit or the program `run` starts.
    std::thread::spawn(move || drop((graph, ws)));
    Ok(Built {
        ok: summary.ok,
        target,
        kind,
    })
}

pub fn run(opts: &BuildOpts, args: &[String]) -> Result<i32> {
    let built = build(opts)?;
    if !built.ok {
        return Ok(1);
    }
    if built.kind != Kind::Executable {
        bail!("package is not an executable");
    }
    let status = Command::new(&built.target).args(args).status()?;
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

pub fn compdb(arch: Arch, release: bool) -> Result<()> {
    let (_, _, graph) = plan(arch, release)?;
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
    fs::write(
        dir.join("nomake.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"{kind_str}\"\ncxx_standard = \"20\"\n{deps_hint}"),
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
