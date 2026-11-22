use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::compdb;
use crate::config::Kind;
use crate::dag::{self, Graph, Target};
use crate::executor;
use crate::scanner;
use crate::state::State;
use crate::testrun::{self, Test};
use crate::toolchain::{Arch, Toolchain};
use crate::workspace::{self, Member, Project};
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
    pub targets: Vec<Target>,
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

/// Serializes concurrent builds/cleans sharing `<root_dir>/build` so `.nomake_state` and shared libs never race.
fn lock_build(root_dir: &Path) -> Result<File> {
    let build_dir = root_dir.join("build");
    fs::create_dir_all(&build_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(build_dir.join(".lock"))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            eprintln!("nomake: waiting for build lock...");
            file.lock()?;
        }
        Err(TryLockError::Error(err)) => return Err(err.into()),
    }
    Ok(file)
}

fn plan_ws(
    project: &Project,
    ws: &scanner::Workspace,
    arch: Arch,
    release: bool,
) -> Result<(Toolchain, Graph)> {
    let build_root = project.root_dir.join("build");
    let tc_cfg = project.effective_toolchain(&ws.root().toolchain);
    let tc = Toolchain::detect(&tc_cfg, arch, &build_root)?;
    let host = if arch != Arch::host() {
        let tools = dag::tool_roots(ws);
        if tools.is_empty() {
            None
        } else {
            let host_ws = scanner::load_host_tools(&project.root_dir, &tools)?;
            let host_tc = Toolchain::detect_host(&project.toolchain, &build_root)?;
            Some((host_ws, host_tc))
        }
    } else {
        None
    };
    let graph = dag::build_with_host(
        ws,
        &tc,
        host.as_ref().map(|(ws, tc)| (ws, tc)),
        release,
        &project.root_dir,
        &out_dir(&project.root_dir, arch, release),
    )?;
    Ok((tc, graph))
}

fn refresh_compdb_if_present(
    project: &Project,
    all_roots: &[PathBuf],
    full_graph: Option<&Graph>,
    arch: Arch,
    release: bool,
) -> Result<()> {
    if !project.root_dir.join("compile_commands.json").is_file() {
        return Ok(());
    }
    let stamp = compdb_stamp(&project.root_dir);
    if let Some(graph) = full_graph {
        return compdb::refresh(graph, &stamp);
    }
    if all_roots.is_empty() {
        return Ok(());
    }
    let full_ws = scanner::load_project(
        &project.root_dir,
        all_roots,
        all_roots,
        project.has_root_package,
        None,
        arch.os(),
    )?;
    let (_, graph) = plan_ws(project, &full_ws, arch, release)?;
    compdb::refresh(&graph, &stamp)
}

fn execute_ws(
    project: &Project,
    all_roots: &[PathBuf],
    ws: scanner::Workspace,
    active_is_full: bool,
    opts: &BuildOpts,
    start: Instant,
    _lock: File,
) -> Result<Built> {
    let dir = out_dir(&project.root_dir, opts.arch, opts.release);
    let (planned, state) = rayon::join(
        || plan_ws(project, &ws, opts.arch, opts.release),
        || -> Result<State> {
            fs::create_dir_all(&dir)?;
            State::open(&dir.join(".nomake_state"))
        },
    );
    let (tc, graph) = planned?;
    let state = state?;
    refresh_compdb_if_present(
        project,
        all_roots,
        active_is_full.then_some(&graph),
        opts.arch,
        opts.release,
    )?;

    let jobs = opts
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |count| count.get()));
    let root = ws.root();
    let title = if ws.roots.len() > 1 {
        format!(
            "workspace {} ({} packages)",
            project
                .root_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
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
    let targets = graph.targets.clone();
    // Flush and close `.nomake_state` before `_lock` drops at return.
    drop(state);
    std::thread::spawn(move || drop((graph, ws)));
    Ok(Built {
        ok: summary.ok,
        targets,
    })
}

pub fn build_roots(project: &Project, active_roots: &[PathBuf], opts: &BuildOpts) -> Result<Built> {
    let start = Instant::now();
    let lock = lock_build(&project.root_dir)?;
    let all_roots = project.member_dirs();
    let active_is_full = active_roots == all_roots.as_slice();
    let ws = scanner::load_project(
        &project.root_dir,
        &all_roots,
        active_roots,
        project.has_root_package,
        None,
        opts.arch.os(),
    )?;
    execute_ws(project, &all_roots, ws, active_is_full, opts, start, lock)
}

pub fn build(
    project: &Project,
    cwd: &Path,
    selectors: &[String],
    workspace_flag: bool,
    opts: &BuildOpts,
) -> Result<Built> {
    if !selectors.is_empty() {
        match project.select(cwd, selectors) {
            Ok(members) => {
                let roots: Vec<PathBuf> = members.into_iter().map(|m| m.dir.clone()).collect();
                return build_roots(project, &roots, opts);
            }
            Err(err) if err.to_string().starts_with("ambiguous package selector") => {
                return Err(err);
            }
            Err(member_err) => {
                // Allow targeting a non-member dependency (e.g. VS right-click -> Build on a `deps/` project).
                let start = Instant::now();
                let lock = lock_build(&project.root_dir)?;
                let all_roots = project.member_dirs();
                let mut ws = scanner::load_project(
                    &project.root_dir,
                    &all_roots,
                    &all_roots,
                    project.has_root_package,
                    None,
                    opts.arch.os(),
                )?;
                let items: Vec<(&str, &Path)> = ws
                    .pkgs
                    .iter()
                    .map(|pkg| (pkg.name.as_str(), pkg.root.as_path()))
                    .collect();
                let indices = match workspace::select_items(
                    &items,
                    &project.root_dir,
                    project.is_workspace,
                    cwd,
                    selectors,
                ) {
                    Ok(indices) => indices,
                    Err(dep_err)
                        if dep_err
                            .to_string()
                            .starts_with("ambiguous package selector") =>
                    {
                        return Err(dep_err);
                    }
                    Err(_) => return Err(member_err),
                };
                let chosen: Vec<PathBuf> = indices
                    .into_iter()
                    .map(|idx| ws.pkgs[idx].root.clone())
                    .collect();
                ws.retain_roots(&chosen);
                return execute_ws(project, &all_roots, ws, false, opts, start, lock);
            }
        }
    }

    let active_roots: Vec<PathBuf> = if !project.is_workspace {
        vec![project.members[0].dir.clone()]
    } else if !workspace_flag {
        if let Some(idx) = project.default_member {
            vec![project.members[idx].dir.clone()]
        } else {
            project
                .members
                .iter()
                .filter(|m| m.kind != Kind::Test)
                .map(|m| m.dir.clone())
                .collect()
        }
    } else {
        project
            .members
            .iter()
            .filter(|m| m.kind != Kind::Test)
            .map(|m| m.dir.clone())
            .collect()
    };

    if active_roots.is_empty() {
        let _lock = lock_build(&project.root_dir)?;
        let all_roots = project.member_dirs();
        scanner::load_project(
            &project.root_dir,
            &all_roots,
            &[],
            project.has_root_package,
            None,
            opts.arch.os(),
        )?;
        refresh_compdb_if_present(project, &all_roots, None, opts.arch, opts.release)?;
        println!(
            "nomake: nothing to build (all workspace members are test packages; run 'nomake test')"
        );
        return Ok(Built {
            ok: true,
            targets: Vec::new(),
        });
    }
    build_roots(project, &active_roots, opts)
}

pub fn test(
    project: &Project,
    cwd: &Path,
    selectors: &[String],
    workspace_flag: bool,
    opts: &BuildOpts,
    extra: &[String],
) -> Result<i32> {
    let mut tests = Vec::new();
    if !selectors.is_empty() {
        for member in project.select(cwd, selectors)? {
            let Some(config) = &member.test else {
                bail!(
                    "package '{}' is not a test (set type = \"test\")",
                    member.name
                );
            };
            tests.push(Test {
                name: member.name.clone(),
                dir: member.dir.clone(),
                config: config.clone(),
            });
        }
        return testrun::run(project, opts, &tests, extra);
    }

    if !workspace_flag {
        if let Some(idx) = project.default_member {
            let member = &project.members[idx];
            if let Some(config) = &member.test {
                tests.push(Test {
                    name: member.name.clone(),
                    dir: member.dir.clone(),
                    config: config.clone(),
                });
                return testrun::run(project, opts, &tests, extra);
            }
            if !project.is_workspace {
                bail!(
                    "package '{}' is not a test (set type = \"test\")",
                    member.name
                );
            }
            // Inside a non-test workspace member without selectors: run all workspace tests.
        }
    }
    for member in &project.members {
        if let Some(config) = &member.test {
            tests.push(Test {
                name: member.name.clone(),
                dir: member.dir.clone(),
                config: config.clone(),
            });
        }
    }
    testrun::run(project, opts, &tests, extra)
}

pub fn run(
    project: &Project,
    cwd: &Path,
    selectors: &[String],
    workspace_flag: bool,
    opts: &BuildOpts,
    args: &[String],
) -> Result<i32> {
    if selectors.len() > 1 {
        bail!("'nomake run' takes at most one package name (pass program arguments after '--')");
    }
    let member: &Member = if !selectors.is_empty() {
        let chosen = project.select(cwd, selectors)?;
        let member = chosen[0];
        if !member.kind.is_program() {
            bail!(
                "package '{}' is a {} library, not an executable",
                member.name,
                member.kind.name()
            );
        }
        member
    } else if !workspace_flag && project.default_member.is_some() {
        let member = &project.members[project.default_member.unwrap()];
        if !member.kind.is_program() {
            if project.is_workspace {
                bail!(
                    "package '{}' is a {} library, not an executable (pass '-p <name>' or '-w' to run a workspace executable)",
                    member.name,
                    member.kind.name()
                );
            }
            bail!(
                "package '{}' is a {} library, not an executable",
                member.name,
                member.kind.name()
            );
        }
        member
    } else if let Some(idx) = project.primary {
        &project.members[idx]
    } else {
        let exes: Vec<&Member> = project
            .members
            .iter()
            .filter(|m| m.kind == Kind::Executable)
            .collect();
        match exes.as_slice() {
            [only] => only,
            [] => {
                if !project.is_workspace {
                    let only = &project.members[0];
                    bail!(
                        "package '{}' is a {} library, not an executable",
                        only.name,
                        only.kind.name()
                    );
                }
                bail!("no executable package in workspace");
            }
            [first, ..] => {
                let names: Vec<&str> = exes.iter().map(|m| m.name.as_str()).collect();
                bail!(
                    "multiple executables in workspace ({}); pass a package name (`nomake run <name>`) or set `primary = \"{}\"` under [workspace]",
                    names.join(", "),
                    first.name
                );
            }
        }
    };

    let built = build_roots(project, std::slice::from_ref(&member.dir), opts)?;
    if !built.ok {
        return Ok(1);
    }
    let Some(launch) = &built.targets[0].launch else {
        bail!("package '{}' is not an executable", member.name);
    };
    let mut command = launch.command()?;
    command.args(args).current_dir(&member.dir);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn clean_pkg_in_dir(cfg_dir: &Path, name: &str) -> Result<()> {
    for sub in ["obj", "gen"] {
        let dir = cfg_dir.join(sub).join(name);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
        }
    }
    let candidates = [
        name.to_string(),
        format!("{name}.exe"),
        format!("{name}.pdb"),
        format!("{name}.ilk"),
        format!("{name}.exp"),
        format!("{name}.lib"),
        format!("{name}.dll"),
        format!("lib{name}.a"),
        format!("lib{name}.so"),
        format!("lib{name}.dylib"),
        format!("{name}.html"),
        format!("{name}.js"),
        format!("{name}.wasm"),
        format!("{name}.data"),
        format!("{name}.wasm.map"),
    ];
    for file in candidates {
        let path = cfg_dir.join(file);
        if path.is_file() {
            fs::remove_file(&path)?;
        }
    }
    let host_dir = cfg_dir.join("host");
    if host_dir.is_dir() {
        clean_pkg_in_dir(&host_dir, name)?;
    }
    Ok(())
}

/// Removes one configuration's build directory, specific packages' outputs, or everything under build/ except `vs`.
pub fn clean(
    project: &Project,
    cwd: &Path,
    selectors: &[String],
    config: Option<(Arch, bool)>,
) -> Result<()> {
    let build_dir = project.root_dir.join("build");
    if !selectors.is_empty() {
        let target_os = config.map_or(crate::config::Os::host(), |(arch, _)| arch.os());
        let names: Vec<String> = match project.select(cwd, selectors) {
            Ok(members) => members.into_iter().map(|m| m.name.clone()).collect(),
            Err(err) if err.to_string().starts_with("ambiguous package selector") => {
                return Err(err);
            }
            Err(member_err) => {
                let all_roots = project.member_dirs();
                let ws = scanner::load_project(
                    &project.root_dir,
                    &all_roots,
                    &all_roots,
                    project.has_root_package,
                    None,
                    target_os,
                )?;
                let items: Vec<(&str, &Path)> = ws
                    .pkgs
                    .iter()
                    .map(|pkg| (pkg.name.as_str(), pkg.root.as_path()))
                    .collect();
                match workspace::select_items(
                    &items,
                    &project.root_dir,
                    project.is_workspace,
                    cwd,
                    selectors,
                ) {
                    Ok(indices) => indices
                        .into_iter()
                        .map(|idx| ws.pkgs[idx].name.clone())
                        .collect(),
                    Err(dep_err)
                        if dep_err
                            .to_string()
                            .starts_with("ambiguous package selector") =>
                    {
                        return Err(dep_err);
                    }
                    Err(_) => return Err(member_err),
                }
            }
        };
        if build_dir.is_dir() {
            let _lock = lock_build(&project.root_dir)?;
            let cfg_dirs: Vec<PathBuf> = match config {
                Some((arch, release)) => vec![out_dir(&project.root_dir, arch, release)],
                None => fs::read_dir(&build_dir)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir() && p.file_name().is_none_or(|n| n != "vs"))
                    .collect(),
            };
            for cfg_dir in cfg_dirs {
                if cfg_dir.is_dir() {
                    for name in &names {
                        clean_pkg_in_dir(&cfg_dir, name)?;
                    }
                }
            }
        }
        println!("nomake: cleaned {}", names.join(", "));
        return Ok(());
    }

    if !build_dir.is_dir() {
        let target = match config {
            Some((arch, release)) => out_dir(&project.root_dir, arch, release),
            None => build_dir,
        };
        println!("nomake: removed {}", target.display());
        return Ok(());
    }
    let _lock = lock_build(&project.root_dir)?;
    if let Some((arch, release)) = config {
        let dir = out_dir(&project.root_dir, arch, release);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
        }
        println!("nomake: removed {}", dir.display());
        return Ok(());
    }
    for entry in fs::read_dir(&build_dir)? {
        let path = entry?.path();
        // Keep the open VS solution and the active `.lock` file handle.
        if path
            .file_name()
            .is_some_and(|name| name == "vs" || name == ".lock")
        {
            continue;
        }
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    println!("nomake: removed {}", build_dir.display());
    Ok(())
}

pub fn compdb(project: &Project, arch: Arch, release: bool) -> Result<()> {
    let all_roots = project.member_dirs();
    if all_roots.is_empty() {
        bail!("no packages in workspace");
    }
    let ws = scanner::load_project(
        &project.root_dir,
        &all_roots,
        &all_roots,
        project.has_root_package,
        None,
        arch.os(),
    )?;
    let (_, graph) = plan_ws(project, &ws, arch, release)?;
    let count = compdb::generate(&graph, &compdb_stamp(&project.root_dir))?;
    println!("nomake: wrote compile_commands.json ({count} entries)");
    Ok(())
}

pub fn new(name: &str, kind: Kind) -> Result<()> {
    let dir = Path::new(name);
    if dir.exists() {
        bail!("{name} already exists");
    }
    fs::create_dir_all(dir.join("src"))?;
    let kind_str = kind.name();
    let public_inc = if kind.is_program() {
        "[]"
    } else {
        "[\"include\"]"
    };
    fs::write(
        dir.join("nomake.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"{kind_str}\"\ncxx_standard = \"20\"\n\n\
             [sources]\ndirs = [\"src\"]\n\n\
             [include]\npublic = {public_inc}\nprivate = [\"src\"]\n"
        ),
    )?;
    if kind.is_program() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolchain::{Family, Launch};

    #[test]
    fn launch_spec_runs_native_directly_and_wasm_companion_js_under_node() {
        let native_tc = Toolchain::for_tests(Family::Clang);
        let native = native_tc
            .launch(Kind::Executable, Path::new("app.exe"), &[])
            .unwrap()
            .command()
            .unwrap();
        assert_eq!(native.get_program(), "app.exe");

        let wasm_tc = Toolchain::for_tests(Family::Emscripten);
        let html = Path::new("build/app.html");
        let regular = wasm_tc
            .launch(Kind::Executable, html, &[])
            .unwrap()
            .command()
            .unwrap();
        assert_eq!(regular.get_program(), "node");
        assert_eq!(
            regular.get_args().collect::<Vec<_>>(),
            [Path::new("build/app.js").as_os_str()]
        );

        let single = wasm_tc
            .launch(Kind::Executable, html, &["-sSINGLE_FILE=1".into()])
            .unwrap();
        assert!(single
            .command()
            .err()
            .unwrap()
            .to_string()
            .contains("single-file HTML"));

        let test_js = wasm_tc
            .launch(
                Kind::Test,
                Path::new("build/suite.js"),
                &["-sSINGLE_FILE=1".into()],
            )
            .unwrap();
        assert_eq!(test_js, Launch::Node(PathBuf::from("build/suite.js")));
    }

    #[test]
    fn build_lock_serializes_contending_callers() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let root = std::env::temp_dir().join(format!("nomake_flock_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let events = Arc::new(Mutex::new(Vec::new()));

        let first = lock_build(&root).unwrap();
        events.lock().unwrap().push("first_acquired");

        let root2 = root.clone();
        let events2 = events.clone();
        let waiter = std::thread::spawn(move || {
            let _second = lock_build(&root2).unwrap();
            events2.lock().unwrap().push("second_acquired");
        });

        std::thread::sleep(Duration::from_millis(60));
        events.lock().unwrap().push("first_releasing");
        drop(first);
        waiter.join().unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["first_acquired", "first_releasing", "second_acquired"]
        );
        let _ = fs::remove_dir_all(&root);
    }
}
