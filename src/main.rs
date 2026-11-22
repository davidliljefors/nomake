macro_rules! bail {
    ($($t:tt)*) => { return Err(format!($($t)*).into()) };
}

mod commands;
mod compdb;
mod config;
mod dag;
mod executor;
mod init;
mod registry;
mod scanner;
mod state;
mod testrun;
mod toolchain;
mod util;
mod vs;
mod workspace;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

use commands::BuildOpts;
use config::{Kind, Os};
use toolchain::Arch;

const USAGE: &str = "\
nomake - C/C++ build tool

USAGE:
    nomake <command> [options]

COMMANDS:
    new <name> [--lib|--shared]  Create a new project
    init [-y] [--lib|--shared]   Create nomake.toml for current directory
    configure [pkg]                       Interactive project and dependency configuration
    add <name>[@version] [-p PKG]         Add a registry dependency
    update [name...]                      Update locked dependencies
    list | search | info                  Inspect registry dependencies
    cache clean                           Remove cached dependency sources
    build [pkg...] [-r] [-a ARCH] [-j N]  Build packages
    run [pkg] [-r] [-a ARCH] [-- args]    Build and run executable
    test [pkg...] [-r] [-a ARCH] [-- args] Build and run tests
    compdb [-r] [-a ARCH]                 Generate compile_commands.json
    vs [-a ARCH,...]                      Generate Visual Studio solution in build/vs/
    clean [pkg...] [-r] [-a ARCH]         Remove build artifacts

OPTIONS:
    -r, --release       Release build
    -a, --arch ARCH     Target architecture: x64, x86, arm64, wasm32
    -p, --package PKG   Target package (repeatable)
    -w, --workspace     Operate on the whole workspace
    -j, --jobs N        Parallel jobs (default: all cores)
    -v, --verbose       Print executed commands
    -d explain          Explain why targets rebuild
    -d stats            Print phase timing
    -y, --yes           Accept defaults in init
    -V, --version       Print version
";

#[derive(Default)]
struct Args {
    release: bool,
    archs: Vec<Arch>,
    packages: Vec<String>,
    workspace: bool,
    jobs: Option<usize>,
    verbose: bool,
    debug: executor::Debug,
    yes: bool,
    kind: Option<Kind>,
    positional: Vec<String>,
    rest: Vec<String>,
}

fn parse_archs(text: &str) -> Result<Vec<Arch>> {
    let mut archs = Vec::new();
    for name in text.split(',') {
        match Arch::parse(name) {
            Some(arch) => archs.push(arch),
            None => bail!("unknown architecture '{name}' (expected x64, x86, arm64 or wasm32)"),
        }
    }
    Ok(archs)
}

fn single_arch(args: &Args) -> Result<Arch> {
    match args.archs.as_slice() {
        [] => Ok(Arch::host()),
        [arch] => Ok(*arch),
        _ => bail!("only `nomake vs` accepts several architectures"),
    }
}

fn parse(argv: Vec<String>) -> Result<Args> {
    let mut args = Args::default();
    let mut remaining = argv.into_iter();
    while let Some(arg) = remaining.next() {
        match arg.as_str() {
            "--" => {
                args.rest = remaining.collect();
                break;
            }
            "-r" | "--release" => args.release = true,
            "-v" | "--verbose" => args.verbose = true,
            "-y" | "--yes" => args.yes = true,
            "-d" => match remaining.next().as_deref() {
                Some("explain") => args.debug.explain = true,
                Some("stats") => args.debug.stats = true,
                other => bail!("unknown debug mode {other:?} (expected `explain` or `stats`)"),
            },
            "-j" | "--jobs" => {
                args.jobs = Some(remaining.next().ok_or("-j needs a value")?.parse()?)
            }
            "-a" | "--arch" => args.archs.extend(parse_archs(
                &remaining.next().ok_or("--arch needs a value")?,
            )?),
            "-p" | "--package" => args
                .packages
                .push(remaining.next().ok_or("--package needs a value")?),
            "-w" | "--workspace" => args.workspace = true,
            "--bin" => args.kind = Some(Kind::Executable),
            "--lib" | "--static" => args.kind = Some(Kind::Static),
            "--shared" => args.kind = Some(Kind::Shared),
            text if text.starts_with("--arch=") => args.archs.extend(parse_archs(&text[7..])?),
            text if text.starts_with("--package=") => args.packages.push(text[10..].to_string()),
            text if text.starts_with("-p") && text.len() > 2 => {
                args.packages.push(text[2..].to_string())
            }
            text if text.starts_with("-j") => args.jobs = Some(text[2..].parse()?),
            text if text.starts_with('-') => bail!("unknown option {text}"),
            other => args.positional.push(other.to_string()),
        }
    }
    Ok(args)
}

fn real_main() -> Result<i32> {
    let mut argv = std::env::args().skip(1);
    let Some(command) = argv.next() else {
        print!("{USAGE}");
        return Ok(0);
    };
    let args = parse(argv.collect())?;
    match command.as_str() {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            return Ok(0);
        }
        "-V" | "--version" => {
            println!("nomake {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        "init" => return init::init(args.yes, args.kind).map(|_| 0),
        "new" => {
            let Some(name) = args.positional.first() else {
                bail!("usage: nomake new <name>")
            };
            if args.positional.len() > 1 {
                bail!("usage: nomake new <name>");
            }
            return commands::new(name, args.kind.unwrap_or(Kind::Executable)).map(|_| 0);
        }
        "configure" => {
            if args.workspace {
                bail!("cannot combine '--workspace' with 'nomake configure'");
            }
            let here = util::absolute(std::path::Path::new("."));
            let project = workspace::Project::discover(&here, Os::host())?;
            let mut selectors = args.packages.clone();
            selectors.extend(args.positional.iter().cloned());
            return init::configure(&project, &here, &selectors).map(|_| 0);
        }
        "add" | "update" | "list" | "search" | "info" | "cache" => {
            if args.workspace && command == "add" {
                bail!("cannot combine '--workspace' with 'nomake add'");
            }
            let mut words = vec![command.clone()];
            words.extend(args.positional.iter().cloned());
            let here = util::absolute(std::path::Path::new("."));
            return registry::command(&words, &args.packages, &here).map(|_| 0);
        }
        "build" | "b" | "run" | "r" | "test" | "t" | "clean" | "compdb" | "vs" => {}
        other => bail!("unknown command '{other}' (try `nomake help`)"),
    }

    let here = util::absolute(std::path::Path::new("."));
    let target_os = args.archs.first().map_or(Os::host(), |arch| arch.os());
    let project = workspace::Project::discover(&here, target_os)?;

    let mut selectors = args.packages.clone();
    selectors.extend(args.positional.iter().cloned());
    if args.workspace && !selectors.is_empty() {
        bail!("cannot combine '--workspace' with package selectors");
    }

    if command == "vs" {
        if !selectors.is_empty() {
            bail!("'nomake vs' covers the whole workspace and takes no package selectors");
        }
        let archs = if args.archs.is_empty() {
            vec![Arch::host()]
        } else {
            args.archs
        };
        return vs::generate_for(&project.root_dir, &project.member_dirs(), &archs).map(|_| 0);
    }

    let arch = single_arch(&args)?;
    let opts = BuildOpts {
        release: args.release,
        arch,
        jobs: args.jobs,
        verbose: args.verbose,
        debug: args.debug,
    };
    match command.as_str() {
        "build" | "b" => Ok(
            if commands::build(&project, &here, &selectors, args.workspace, &opts)?.ok {
                0
            } else {
                1
            },
        ),
        "run" | "r" => commands::run(
            &project,
            &here,
            &selectors,
            args.workspace,
            &opts,
            &args.rest,
        ),
        "test" | "t" => commands::test(
            &project,
            &here,
            &selectors,
            args.workspace,
            &opts,
            &args.rest,
        ),
        "clean" => {
            let config = (!args.archs.is_empty() || args.release).then_some((arch, args.release));
            commands::clean(&project, &here, &selectors, config).map(|_| 0)
        }
        "compdb" => {
            if !selectors.is_empty() {
                bail!("'nomake compdb' covers the whole workspace and takes no package selectors");
            }
            commands::compdb(&project, arch, args.release).map(|_| 0)
        }
        _ => unreachable!(),
    }
}

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("nomake: error: {e}");
            std::process::exit(1);
        }
    }
}
