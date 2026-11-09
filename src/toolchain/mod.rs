use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Crt, Kind};
use crate::Result;

mod flags;
pub mod vs;

use flags::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Gcc,
    Clang,
    Msvc,
    ClangCl,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    X64,
    X86,
    Arm64,
}

impl Arch {
    pub fn parse(text: &str) -> Option<Arch> {
        match text.to_lowercase().as_str() {
            "x64" | "x86_64" | "amd64" => Some(Arch::X64),
            "x86" | "i686" | "i386" | "win32" => Some(Arch::X86),
            "arm64" | "aarch64" => Some(Arch::Arm64),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::X86 => "x86",
            Arch::Arm64 => "arm64",
        }
    }

    pub fn host() -> Arch {
        let reported = if cfg!(windows) {
            std::env::var("PROCESSOR_ARCHITEW6432")
                .or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE"))
                .unwrap_or_default()
        } else {
            std::env::consts::ARCH.to_string()
        };
        Arch::parse(&reported).unwrap_or(Arch::X64)
    }

    fn triple(self) -> String {
        let cpu = match self {
            Arch::X64 => "x86_64",
            Arch::X86 => "i686",
            Arch::Arm64 if cfg!(target_os = "macos") => "arm64",
            Arch::Arm64 => "aarch64",
        };
        if cfg!(windows) {
            format!("{cpu}-pc-windows-msvc")
        } else if cfg!(target_os = "macos") {
            format!("{cpu}-apple-darwin")
        } else {
            format!("{cpu}-unknown-linux-gnu")
        }
    }

    fn vcvars_arg(self, host: Arch) -> String {
        if self == host {
            self.name().to_string()
        } else {
            format!("{}_{}", host.name(), self.name())
        }
    }

    fn vs_component(self) -> &'static str {
        match self {
            Arch::Arm64 => "Microsoft.VisualStudio.Component.VC.Tools.ARM64",
            Arch::X64 | Arch::X86 => "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        }
    }
}

/// The real linker, run without the compiler driver; the driver only adds start-up cost and paths we can capture once.
#[derive(Clone)]
pub struct Linker {
    pub program: PathBuf,
    prefix: Vec<String>,
}

#[derive(Clone)]
pub struct Toolchain {
    pub family: Family,
    /// Compiler and linker flags that select the target; empty when the tool's default already matches.
    target_args: Vec<String>,
    pub cxx: PathBuf,
    pub ar: PathBuf,
    /// Set on Windows, where links go straight to link.exe / lld-link.
    pub linker: Option<Linker>,
    /// Extra environment for every spawned tool (the vcvars environment when not in a developer prompt).
    pub env: Vec<(String, String)>,
}

pub struct CompileSpec<'a> {
    pub src: &'a Path,
    pub obj: &'a Path,
    pub depfile: &'a Path,
    pub is_c: bool,
    pub std: &'a str,
    pub includes: &'a [PathBuf],
    pub defines: &'a [String],
    pub flags: &'a [String],
    pub release: bool,
    pub crt: Crt,
    pub pch: Pch<'a>,
}

#[derive(Clone, Copy)]
pub enum Pch<'a> {
    None,
    /// Precompile the header `name` into `file`.
    Build {
        name: &'a str,
        file: &'a Path,
    },
    Use {
        name: &'a str,
        file: &'a Path,
    },
}

pub struct LinkSpec<'a> {
    pub kind: Kind,
    pub out: &'a Path,
    pub objs: &'a [PathBuf],
    pub libs: &'a [PathBuf],
    pub sys_libs: &'a [String],
    pub flags: &'a [String],
    pub release: bool,
    pub crt: Crt,
    pub subsystem: Option<&'a str>,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn family_of(cxx: &str) -> Family {
    let stem = Path::new(cxx)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    if stem.contains("clang-cl") {
        Family::ClangCl
    } else if stem == "cl" {
        Family::Msvc
    } else if stem.contains("clang") {
        Family::Clang
    } else {
        Family::Gcc
    }
}

fn which(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    let direct = Path::new(name);
    if direct.components().count() > 1 {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    for dir in std::env::split_paths(path_var) {
        for ext in ["", ".exe"] {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Environment of the MSVC developer prompt for `arch`, built by running vcvarsall.
#[cfg(windows)]
fn vcvars_env(arch: Arch) -> Result<Vec<(String, String)>> {
    use std::os::windows::process::CommandExt;

    let program_files = std::env::var("ProgramFiles(x86)")?;
    let vswhere = Path::new(&program_files).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    let output = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            arch.vs_component(),
            "-property",
            "installationPath",
        ])
        .output()?;
    let install = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if install.is_empty() {
        bail!(
            "Visual Studio with the C++ tools for {} was not found",
            arch.name()
        );
    }
    let vcvarsall = Path::new(&install).join(r"VC\Auxiliary\Build\vcvarsall.bat");
    let output = Command::new("cmd")
        .raw_arg(format!(
            "/d /s /c \"\"{}\" {} >nul && set\"",
            vcvarsall.display(),
            arch.vcvars_arg(Arch::host())
        ))
        .env_remove("VSCMD_VER")
        .env_remove("VSCMD_ARG_HOST_ARCH")
        .env_remove("VSCMD_ARG_TGT_ARCH")
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect())
}

#[cfg(not(windows))]
fn vcvars_env(_arch: Arch) -> Result<Vec<(String, String)>> {
    bail!("MSVC is only available on Windows")
}

/// vcvarsall takes seconds, so the variables it changes are cached until an include or lib directory they name disappears.
fn cached_vcvars(arch: Arch, cache: &Path) -> Result<Vec<(String, String)>> {
    let file = cache.join(format!(".nomake_vcvars_{}", arch.name()));
    if let Ok(text) = fs::read_to_string(&file) {
        let env: Vec<(String, String)> = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        let dirs_exist = env
            .iter()
            .filter(|(key, _)| {
                key.eq_ignore_ascii_case("include") || key.eq_ignore_ascii_case("lib")
            })
            .flat_map(|(_, value)| value.split(';'))
            .filter(|dir| !dir.is_empty())
            .all(|dir| Path::new(dir).is_dir());
        if dirs_exist {
            return Ok(env);
        }
    }
    let mut env = vcvars_env(arch)?;
    env.retain(|(key, value)| std::env::var(key).ok().as_deref() != Some(value.as_str()));
    fs::create_dir_all(cache)?;
    fs::write(
        &file,
        env.iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect::<String>(),
    )?;
    Ok(env)
}

/// Whether the current shell is already a developer prompt for `arch`.
fn ambient_env_matches(arch: Arch) -> bool {
    if std::env::var_os("INCLUDE").is_none() {
        return false;
    }
    std::env::var("VSCMD_ARG_TGT_ARCH")
        .map_or(true, |target| target.eq_ignore_ascii_case(arch.name()))
}

/// Quoted, backslash-escaped words as printed by `clang -###`.
fn quoted_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut word = String::new();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => word.push(chars.next().unwrap()),
                '"' => break,
                ch => word.push(ch),
            }
        }
        words.push(word);
    }
    words
}

/// The lld-link command line clang would run (default libs, library paths), minus inputs and output.
/// Probing costs a process, so the answer is cached in `cache` until the compiler or a library path changes.
fn driver_linker(
    family: Family,
    cxx: &Path,
    target_args: &[String],
    arch: Arch,
    cache: &Path,
) -> Result<Linker> {
    let key = format!("{}|{:?}", cxx.display(), fs::metadata(cxx)?.modified()?);
    let file = cache.join(format!(
        ".nomake_linker_{}_{}",
        cxx.file_stem().unwrap().to_string_lossy(),
        arch.name()
    ));
    if let Ok(text) = fs::read_to_string(&file) {
        let mut lines = text.lines();
        if lines.next() == Some(key.as_str()) {
            let words: Vec<String> = lines.map(String::from).collect();
            if words
                .iter()
                .filter_map(|word| word.strip_prefix("-libpath:"))
                .all(|dir| Path::new(dir).is_dir())
            {
                return Ok(Linker {
                    program: PathBuf::from(&words[0]),
                    prefix: words[1..].to_vec(),
                });
            }
        }
    }
    fs::create_dir_all(cache)?;
    fs::write(cache.join("probe.o"), "")?;
    let mut command = Command::new(cxx);
    command
        .current_dir(cache)
        .args(target_args)
        .args(["-fuse-ld=lld", "-###", "probe.o"]);
    command.arg(if family == Family::ClangCl {
        "/Feprobe.exe"
    } else {
        "-oprobe.exe"
    });
    let output = command.output()?;
    let text = String::from_utf8_lossy(&output.stderr);
    let Some(line) = text.lines().rev().find(|line| line.contains("lld-link")) else {
        bail!(
            "could not find the lld-link command in `{} -###`",
            cxx.display()
        );
    };
    let words: Vec<String> = quoted_words(line)
        .into_iter()
        .filter(|word| !word.starts_with("-out:") && word != "probe.o")
        .collect();
    fs::write(&file, format!("{key}\n{}\n", words.join("\n")))?;
    Ok(Linker {
        program: PathBuf::from(&words[0]),
        prefix: words[1..].to_vec(),
    })
}

/// Flags that make the compiler target `arch`, or none when its default already does.
fn target_args_for(family: Family, arch: Arch) -> Result<Vec<String>> {
    let host = Arch::host();
    match family {
        Family::Msvc => Ok(Vec::new()),
        Family::ClangCl if cfg!(windows) => Ok(vec![format!("--target={}", arch.triple())]),
        Family::Clang | Family::ClangCl if arch != host => {
            Ok(vec![format!("--target={}", arch.triple())])
        }
        Family::Clang | Family::ClangCl => Ok(Vec::new()),
        Family::Gcc if arch == host => Ok(Vec::new()),
        Family::Gcc if arch == Arch::X86 && host == Arch::X64 => Ok(vec!["-m32".into()]),
        Family::Gcc => bail!(
            "g++ cannot target {}; set CXX or [toolchain] cxx to a cross compiler",
            arch.name()
        ),
    }
}

impl Toolchain {
    /// Order: `CXX` env var, `[toolchain] cxx`, then the platform default (clang++, cl, g++ / clang++, g++, c++).
    /// `cache` is a writable directory for probe results that outlive one build.
    pub fn detect(cfg: &config::Toolchain, arch: Arch, cache: &Path) -> Result<Toolchain> {
        let requested = std::env::var("CXX").ok().or_else(|| cfg.cxx.clone());
        let candidates: Vec<String> = match requested {
            Some(name) => vec![name],
            None if cfg!(windows) => ["clang++", "cl", "g++"].map(String::from).to_vec(),
            None => ["clang++", "g++", "c++"].map(String::from).to_vec(),
        };
        for candidate in &candidates {
            let family = family_of(candidate);
            let mut env = Vec::new();
            let mut path_var = std::env::var_os("PATH").unwrap_or_default();
            if matches!(family, Family::Msvc | Family::ClangCl) && !ambient_env_matches(arch) {
                let Ok(vcvars) = cached_vcvars(arch, cache) else {
                    continue;
                };
                path_var = vcvars
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("path"))
                    .map(|(_, value)| value.into())
                    .unwrap_or_default();
                env = vcvars;
            }
            let Some(cxx) = which(candidate, &path_var) else {
                continue;
            };
            let ar_names: Vec<&str> = match (&cfg.ar, family) {
                (Some(name), _) => vec![name.as_str()],
                (None, Family::Gcc) => vec!["ar"],
                (None, Family::Clang) => vec!["llvm-ar", "ar"],
                (None, Family::Msvc) => vec!["lib"],
                (None, Family::ClangCl) => vec!["llvm-lib", "lib"],
            };
            let Some(ar) = ar_names.iter().find_map(|name| which(name, &path_var)) else {
                bail!("no archiver found (tried {})", ar_names.join(", "));
            };
            let target_args = target_args_for(family, arch)?;
            let linker = match family {
                Family::Msvc => Some(Linker {
                    program: cxx.with_file_name("link.exe"),
                    prefix: vec!["/nologo".into()],
                }),
                Family::Clang | Family::ClangCl if cfg!(windows) => {
                    Some(driver_linker(family, &cxx, &target_args, arch, cache)?)
                }
                _ => None,
            };
            return Ok(Toolchain {
                family,
                target_args,
                cxx,
                ar,
                linker,
                env,
            });
        }
        bail!("no C++ compiler found (tried {})", candidates.join(", "))
    }

    #[cfg(test)]
    pub fn for_tests(family: Family) -> Toolchain {
        Toolchain {
            family,
            target_args: Vec::new(),
            cxx: "cxx".into(),
            ar: "ar".into(),
            linker: None,
            env: Vec::new(),
        }
    }

    pub fn msvc_style(&self) -> bool {
        matches!(self.family, Family::Msvc | Family::ClangCl)
    }

    /// COFF-style naming (`.lib`) applies to MSVC-flavoured tools and to clang targeting Windows.
    fn win_style(&self) -> bool {
        cfg!(windows) && self.family != Family::Gcc
    }

    /// Lowercased INCLUDE directories; headers under them are not tracked as dependencies.
    pub fn system_includes(&self) -> Vec<String> {
        let include = self
            .env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("include"))
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var("INCLUDE").ok())
            .unwrap_or_default();
        include
            .split(';')
            .filter(|dir| !dir.is_empty())
            .map(|dir| dir.trim_end_matches(['\\', '/']).to_lowercase())
            .collect()
    }

    pub fn link_program(&self) -> &Path {
        self.linker
            .as_ref()
            .map_or(&self.cxx, |linker| &linker.program)
    }

    /// (file to produce, file other packages link against)
    pub fn artifact(&self, kind: Kind, name: &str) -> (String, String) {
        match kind {
            Kind::Executable | Kind::Test => {
                let file = if cfg!(windows) {
                    format!("{name}.exe")
                } else {
                    name.to_string()
                };
                (file.clone(), file)
            }
            Kind::Static => {
                let file = if self.win_style() {
                    format!("{name}.lib")
                } else {
                    format!("lib{name}.a")
                };
                (file.clone(), file)
            }
            Kind::Shared if cfg!(windows) => {
                let dll = format!("{name}.dll");
                let import_lib = if self.win_style() {
                    format!("{name}.lib")
                } else {
                    dll.clone()
                };
                (dll, import_lib)
            }
            Kind::Shared => {
                let ext = if cfg!(target_os = "macos") {
                    "dylib"
                } else {
                    "so"
                };
                let file = format!("lib{name}.{ext}");
                (file.clone(), file)
            }
        }
    }

    pub fn compile_args(&self, spec: &CompileSpec) -> Vec<String> {
        let msvc = self.msvc_style();
        let dialect = if msvc { &MSVC_COMPILE } else { &GNU_COMPILE };
        let mut args: Vec<String> = self.target_args.clone();
        flags::extend(&mut args, dialect.always);
        if spec.is_c {
            flags::extend(&mut args, dialect.c_source);
        } else {
            if !msvc && matches!(spec.pch, Pch::Build { .. }) {
                flags::extend(&mut args, GNU_PCH_HEADER_LANG);
            }
            dialect.std.push(&mut args, spec.std);
        }
        match (spec.pch, msvc) {
            (Pch::None, _) | (Pch::Build { .. }, false) => {}
            (Pch::Build { name, file }, true) => {
                MSVC_PCH_CREATE.push(&mut args, name);
                MSVC_PCH_FILE.push(&mut args, path_string(file));
            }
            (Pch::Use { name, file }, true) => {
                MSVC_PCH_USE.push(&mut args, name);
                MSVC_PCH_FORCE_INCLUDE.push(&mut args, name);
                MSVC_PCH_FILE.push(&mut args, path_string(file));
            }
            (Pch::Use { file, .. }, false) => GNU_PCH_USE.push(&mut args, path_string(file)),
        }
        args.push(dialect.compile_only.into());
        args.push(path_string(spec.src));
        dialect.object.push(&mut args, path_string(spec.obj));
        if !msvc {
            flags::extend(&mut args, GNU_DEPFILE_FLAGS);
            GNU_DEPFILE.push(&mut args, path_string(spec.depfile));
        }
        flags::extend(
            &mut args,
            if spec.release {
                dialect.release
            } else {
                dialect.debug
            },
        );
        let table = if self.family == Family::Clang && cfg!(windows) {
            &CLANG_WINDOWS_CRT
        } else {
            &dialect.crt
        };
        flags::extend(&mut args, table[crt_index(spec.crt, spec.release)]);
        let lld_codeview = match self.family {
            Family::ClangCl => true,
            Family::Clang => cfg!(windows),
            Family::Msvc | Family::Gcc => false,
        };
        if lld_codeview && !spec.release {
            flags::extend(&mut args, CODEVIEW_GHASH);
        }
        if !msvc && !spec.release && cfg!(target_os = "linux") {
            flags::extend(&mut args, SPLIT_DWARF);
        }
        if !msvc && !cfg!(windows) {
            flags::extend(&mut args, PIC);
        }
        for dir in spec.includes {
            dialect.include.push(&mut args, path_string(dir));
        }
        for define in spec.defines {
            dialect.define.push(&mut args, define.as_str());
        }
        args.extend(spec.flags.iter().cloned());
        args
    }

    pub fn archive_args(&self, out: &Path, objs: &[PathBuf]) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        if self.msvc_style() {
            flags::extend(&mut args, MSVC_ARCHIVE);
            MSVC_ARCHIVE_OUT.push(&mut args, path_string(out));
        } else {
            args.push(
                if cfg!(target_os = "macos") {
                    APPLE_ARCHIVE
                } else {
                    GNU_ARCHIVE
                }
                .into(),
            );
            args.push(path_string(out));
        }
        args.extend(objs.iter().map(|obj| path_string(obj)));
        args
    }

    pub fn link_args(&self, spec: &LinkSpec) -> Vec<String> {
        let dialect = if self.linker.is_some() {
            &COFF_LINK
        } else {
            &DRIVER_LINK
        };
        let mut args: Vec<String> = Vec::new();
        match &self.linker {
            // The objects carry the runtime they were compiled for.
            Some(linker) => args.extend(
                linker
                    .prefix
                    .iter()
                    .filter(|word| !flags::is_runtime_lib(word))
                    .cloned(),
            ),
            None => args.extend(self.target_args.iter().cloned()),
        }
        if spec.kind == Kind::Shared {
            flags::extend(&mut args, dialect.shared);
            if let Some(implib) = dialect.implib {
                implib.push(&mut args, path_string(&spec.out.with_extension("lib")));
            }
        }
        dialect.out.push(&mut args, path_string(spec.out));
        if !spec.release {
            args.push(
                match (&self.linker, self.family) {
                    (None, _) => DRIVER_DEBUG_LINK,
                    (Some(_), Family::Msvc) => MSVC_DEBUG_LINK,
                    (Some(_), _) => LLD_DEBUG_LINK,
                }
                .into(),
            );
        }
        args.extend(spec.objs.iter().map(|obj| path_string(obj)));
        args.extend(spec.libs.iter().map(|lib| path_string(lib)));
        args.extend(spec.sys_libs.iter().map(|name| (dialect.sys_lib)(name)));
        if spec.crt == Crt::Static && spec.kind == Kind::Executable && !cfg!(target_os = "macos") {
            flags::extend(&mut args, dialect.static_crt);
        }
        if let (Some(opt), Some(subsystem)) = (dialect.subsystem, spec.subsystem) {
            opt.push(&mut args, subsystem);
        }
        args.extend(spec.flags.iter().cloned());
        if self.linker.is_none() && cfg!(target_os = "linux") {
            args.push(LINUX_RPATH_ORIGIN.into());
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(family: Family, crt: Crt) -> String {
        let spec = CompileSpec {
            src: Path::new("a.c"),
            obj: Path::new("a.o"),
            depfile: Path::new("a.d"),
            is_c: true,
            std: "20",
            includes: &[PathBuf::from("inc")],
            defines: &["X=1".to_string()],
            flags: &[],
            release: false,
            crt,
            pch: Pch::None,
        };
        Toolchain::for_tests(family).compile_args(&spec).join(" ")
    }

    fn link(linker: Option<Vec<&str>>, crt: Crt) -> String {
        let mut tc = Toolchain::for_tests(Family::Clang);
        tc.linker = linker.map(|prefix| Linker {
            program: "lld-link".into(),
            prefix: prefix.into_iter().map(String::from).collect(),
        });
        tc.link_args(&LinkSpec {
            kind: Kind::Executable,
            out: Path::new("a.exe"),
            objs: &[PathBuf::from("a.o")],
            libs: &[],
            sys_libs: &["kernel32".to_string()],
            flags: &["/raw".to_string()],
            release: true,
            crt,
            subsystem: Some("console"),
        })
        .join(" ")
    }

    #[test]
    fn crt_mode_picks_the_runtime_flag() {
        let msvc = compile(Family::Msvc, Crt::Dynamic);
        assert!(msvc.contains("/MDd") && !msvc.contains("/MTd"));
        let msvc = compile(Family::Msvc, Crt::Static);
        assert!(msvc.contains("/MTd") && !msvc.contains("/MDd"));

        let gnu = compile(Family::Gcc, Crt::Dynamic);
        assert!(!gnu.contains("-ffreestanding") && gnu.contains("-MMD -MP -MF a.d"));
        assert!(gnu.contains("-Iinc") && gnu.contains("-DX=1") && gnu.contains("-o a.o"));
    }

    #[test]
    fn link_leaves_the_runtime_to_the_objects() {
        let driver_prefix = || {
            Some(vec![
                "-defaultlib:libcmt",
                "-libpath:x",
                "-defaultlib:oldnames",
            ])
        };
        let coff = link(driver_prefix(), Crt::Dynamic);
        assert!(!coff.contains("libcmt") && coff.contains("-defaultlib:oldnames"));
        assert!(coff.contains("-libpath:x") && coff.contains("/SUBSYSTEM:console"));
        assert!(coff.contains("kernel32.lib") && !coff.contains("NODEFAULTLIB"));
        assert!(
            coff.ends_with("/raw"),
            "user flags come last so they can override"
        );

        let gnu = link(None, Crt::Dynamic);
        assert!(!gnu.contains("-static") && !gnu.contains("-nostdlib"));
        if !cfg!(target_os = "macos") {
            assert!(link(None, Crt::Static).contains("-static"));
        }
    }
}
