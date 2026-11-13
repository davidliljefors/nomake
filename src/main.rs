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
    configure                    Interactive project and dependency configuration
    add <name>[@version]         Add a registry dependency
    update [name...]             Update locked dependencies
    list | search | info         Inspect registry dependencies
    cache clean                  Remove cached dependency sources
    build [-r] [-a ARCH] [-j N]  Build project
    run [-r] [-a ARCH] [-- args] Build and run executable
    test [-r] [-a ARCH] [-- args] Build and run tests
    compdb [-r] [-a ARCH]        Generate compile_commands.json
    vs [-a ARCH,...]             Generate Visual Studio solution in build/vs/
    clean [-r] [-a ARCH]         Remove build artifacts

OPTIONS:
    -r, --release    Release build
    -a, --arch ARCH  Target architecture: x64, x86, arm64, wasm32
    -j, --jobs N     Parallel jobs (default: all cores)
    -v, --verbose    Print executed commands
    -d explain       Explain why targets rebuild
    -d stats         Print phase timing
    -y, --yes        Accept defaults in init
    -V, --version    Print version
";

#[derive(Default)]
struct Args {
    release: bool,
    archs: Vec<Arch>,
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
            "--bin" => args.kind = Some(Kind::Executable),
            "--lib" | "--static" => args.kind = Some(Kind::Static),
            "--shared" => args.kind = Some(Kind::Shared),
            text if text.starts_with("--arch=") => args.archs.extend(parse_archs(&text[7..])?),
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
    let here = util::absolute(std::path::Path::new("."));
    let workspace = match command.as_str() {
        "build" | "b" | "run" | "r" | "test" | "t" | "clean" | "compdb" | "vs" => workspace::find(
            &here,
            args.archs.first().map_or(Os::host(), |arch| arch.os()),
        )?,
        _ => None,
    };
    if command == "vs" {
        let archs = if args.archs.is_empty() {
            vec![Arch::host()]
        } else {
            args.archs
        };
        if let Some(root) = &workspace {
            return workspace::solution(root, &archs).map(|_| 0);
        }
        return vs::generate(&archs).map(|_| 0);
    }
    let arch = single_arch(&args)?;
    let opts = BuildOpts {
        release: args.release,
        arch,
        jobs: args.jobs,
        verbose: args.verbose,
        debug: args.debug,
        roots: Vec::new(),
        refresh_compdb: true,
    };
    if let Some(root) = &workspace {
        let config = (!args.archs.is_empty() || args.release).then_some((arch, args.release));
        return match command.as_str() {
            "build" | "b" => workspace::build(root, &args.positional, opts),
            "run" | "r" => workspace::run(root, &args.positional, opts, &args.rest),
            "test" | "t" => workspace::test(root, &args.positional, opts, &args.rest),
            "clean" => commands::clean(config).map(|_| 0),
            _ => workspace::compdb(root, arch, args.release).map(|_| 0),
        };
    }
    match command.as_str() {
        "build" | "b" => Ok(if commands::build(&opts)?.ok { 0 } else { 1 }),
        "run" | "r" => commands::run(&opts, &args.rest, None),
        "test" | "t" => commands::test(opts, &args.rest),
        "clean" => {
            let config = (!args.archs.is_empty() || args.release).then_some((arch, args.release));
            commands::clean(config).map(|_| 0)
        }
        "compdb" => commands::compdb(arch, args.release, &[]).map(|_| 0),
        "init" => init::init(args.yes, args.kind).map(|_| 0),
        "configure" => init::configure().map(|_| 0),
        "add" | "update" | "list" | "search" | "info" | "cache" => {
            let mut words = vec![command.clone()];
            words.extend(args.positional.iter().cloned());
            registry::command(&words, &std::env::current_dir()?).map(|_| 0)
        }
        "new" => {
            let Some(name) = args.positional.first() else {
                bail!("usage: nomake new <name>")
            };
            commands::new(name, args.kind.unwrap_or(Kind::Executable)).map(|_| 0)
        }
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "-V" | "--version" => {
            println!("nomake {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        other => bail!("unknown command '{other}' (try `nomake help`)"),
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
