use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Crt, Kind, Os};
use crate::Result;

mod flags;

use flags::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Gcc,
    Clang,
    Msvc,
    ClangCl,
    Emscripten,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    X64,
    X86,
    Arm64,
    Wasm32,
}

impl Arch {
    pub fn parse(text: &str) -> Option<Arch> {
        match text.to_lowercase().as_str() {
            "x64" | "x86_64" | "amd64" => Some(Arch::X64),
            "x86" | "i686" | "i386" | "win32" => Some(Arch::X86),
            "arm64" | "aarch64" => Some(Arch::Arm64),
            "wasm32" | "wasm" => Some(Arch::Wasm32),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::X86 => "x86",
            Arch::Arm64 => "arm64",
            Arch::Wasm32 => "wasm32",
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

    /// The OS this architecture is built for: wasm32 is Emscripten, the rest run on the host.
    pub fn os(self) -> Os {
        match self {
            Arch::Wasm32 => Os::Emscripten,
            _ => Os::host(),
        }
    }

    fn triple(self, os: Os) -> String {
        let cpu = match self {
            Arch::X64 => "x86_64",
            Arch::X86 => "i686",
            Arch::Arm64 if os == Os::MacOs => "arm64",
            Arch::Arm64 => "aarch64",
            Arch::Wasm32 => "wasm32",
        };
        match os {
            Os::Windows => format!("{cpu}-pc-windows-msvc"),
            Os::MacOs => format!("{cpu}-apple-darwin"),
            Os::Linux | Os::Unix => format!("{cpu}-unknown-linux-gnu"),
            Os::Emscripten => "wasm32-unknown-emscripten".into(),
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
            Arch::X64 | Arch::X86 | Arch::Wasm32 => {
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
            }
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
    pub os: Os,
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
    if stem == "em++" || stem == "emcc" {
        Family::Emscripten
    } else if stem.contains("clang-cl") {
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
        for ext in ["", ".exe", ".bat"] {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `name` is a compiler wrapper around Emscripten, judged by what `--version` prints.
fn is_emscripten_wrapper(name: &str, path_var: &OsStr) -> bool {
    which(name, path_var)
        .and_then(|path| Command::new(path).arg("--version").output().ok())
        .is_some_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains("emscripten")
        })
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
fn target_args_for(family: Family, arch: Arch, os: Os) -> Result<Vec<String>> {
    let host = Arch::host();
    match family {
        Family::Msvc | Family::Emscripten => Ok(Vec::new()),
        Family::ClangCl if os == Os::Windows || arch != host => {
            Ok(vec![format!("--target={}", arch.triple(os))])
        }
        Family::Clang if arch != host => Ok(vec![format!("--target={}", arch.triple(os))]),
        Family::Clang | Family::ClangCl => Ok(Vec::new()),
        Family::Gcc if arch == host => Ok(Vec::new()),
        Family::Gcc if arch == Arch::X86 && host == Arch::X64 => Ok(vec!["-m32".into()]),
        Family::Gcc => bail!(
            "g++ cannot target {}; set CXX or [toolchain] cxx to a cross compiler",
            arch.name()
        ),
    }
}

/// `path` itself, or every file under it when it is a directory.
fn collect_files(path: &Path, into: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for entry in entries {
            collect_files(&entry, into);
        }
    } else if path.is_file() {
        into.push(path.to_path_buf());
    }
}

fn single_file(link_flags: &[String]) -> bool {
    link_flags.iter().enumerate().any(|(i, flag)| {
        let setting = match flag.as_str() {
            "-s" => link_flags.get(i + 1).map(String::as_str),
            _ => flag.strip_prefix("-s"),
        };
        matches!(setting, Some("SINGLE_FILE" | "SINGLE_FILE=1"))
    })
}

impl Toolchain {
    /// Order: `CXX` env var, `[toolchain] cxx`, then the target's default (clang++, cl, g++ / clang++, g++, c++ / em++).
    /// `cache` is a writable directory for probe results that outlive one build.
    pub fn detect(cfg: &config::Toolchain, arch: Arch, cache: &Path) -> Result<Toolchain> {
        let os = arch.os();
        let host_path = std::env::var_os("PATH").unwrap_or_default();
        // A native compiler in CXX / cxx must not break `-a wasm32`: ignore it unless it is Emscripten's.
        let requested = std::env::var("CXX")
            .ok()
            .or_else(|| cfg.cxx.clone())
            .filter(|name| {
                os != Os::Emscripten
                    || family_of(name) == Family::Emscripten
                    || is_emscripten_wrapper(name, &host_path)
            });
        let defaults: &[&str] = match os {
            Os::Emscripten => &["em++"],
            Os::Windows => &["clang++", "cl", "g++"],
            _ => &["clang++", "g++", "c++"],
        };
        let candidates: Vec<String> = match requested {
            Some(name) => vec![name],
            None => defaults.iter().map(|&name| name.to_string()).collect(),
        };
        for candidate in &candidates {
            let family = if os == Os::Emscripten {
                Family::Emscripten
            } else {
                family_of(candidate)
            };
            if (family == Family::Emscripten) != (os == Os::Emscripten) {
                bail!(
                    "{candidate} and -a {} do not go together: em++ only builds wasm32",
                    arch.name()
                );
            }
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
                (None, Family::Emscripten) => vec!["emar"],
            };
            let Some(ar) = ar_names.iter().find_map(|name| which(name, &path_var)) else {
                bail!("no archiver found (tried {})", ar_names.join(", "));
            };
            let target_args = target_args_for(family, arch, os)?;
            let linker = match family {
                Family::Msvc => Some(Linker {
                    program: cxx.with_file_name("link.exe"),
                    prefix: vec!["/nologo".into()],
                }),
                Family::Clang | Family::ClangCl if os == Os::Windows => {
                    Some(driver_linker(family, &cxx, &target_args, arch, cache)?)
                }
                _ => None,
            };
            return Ok(Toolchain {
                family,
                os,
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
        let os = if family == Family::Emscripten {
            Os::Emscripten
        } else {
            Os::host()
        };
        Toolchain::for_tests_on(family, os)
    }

    #[cfg(test)]
    pub fn for_tests_on(family: Family, os: Os) -> Toolchain {
        Toolchain {
            family,
            os,
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
    pub fn artifact(&self, kind: Kind, name: &str) -> Result<(String, String)> {
        let coff = self.os == Os::Windows && self.family != Family::Gcc;
        Ok(match kind {
            Kind::Executable | Kind::Test => {
                let file = match self.os {
                    Os::Windows => format!("{name}.exe"),
                    Os::Emscripten => format!("{name}.html"),
                    _ => name.to_string(),
                };
                (file.clone(), file)
            }
            Kind::Static => {
                let file = if coff {
                    format!("{name}.lib")
                } else {
                    format!("lib{name}.a")
                };
                (file.clone(), file)
            }
            Kind::Shared => match self.os {
                Os::Emscripten => bail!(
                    "Emscripten cannot build shared libraries; use type = \"static\" (or link = \"static\")"
                ),
                Os::Windows => {
                    let dll = format!("{name}.dll");
                    let import_lib = if coff {
                        format!("{name}.lib")
                    } else {
                        dll.clone()
                    };
                    (dll, import_lib)
                }
                Os::MacOs => {
                    let file = format!("lib{name}.dylib");
                    (file.clone(), file)
                }
                Os::Linux | Os::Unix => {
                    let file = format!("lib{name}.so");
                    (file.clone(), file)
                }
            },
        })
    }

    /// Files the link writes besides `<name>.html`: `.js`, `.wasm` and `.data` (unless `-sSINGLE_FILE` folds them in).
    pub fn side_outputs(&self, name: &str, link_flags: &[String]) -> Vec<String> {
        if self.os != Os::Emscripten || single_file(link_flags) {
            return Vec::new();
        }
        let mut files = vec![format!("{name}.js"), format!("{name}.wasm")];
        if link_flags
            .iter()
            .any(|flag| flag.starts_with("--preload-file"))
        {
            files.push(format!("{name}.data"));
        }
        files
    }

    /// Files named by Emscripten link flags (`--shell-file`, `--embed-file`, etc.) that the link reads.
    pub fn link_inputs(&self, flags: &[String], dir: &Path) -> Vec<PathBuf> {
        if self.family != Family::Emscripten {
            return Vec::new();
        }
        const FILE_OPTIONS: [&str; 6] = [
            "--shell-file",
            "--pre-js",
            "--post-js",
            "--js-library",
            "--embed-file",
            "--preload-file",
        ];
        let mut inputs = Vec::new();
        let mut words = flags.iter();
        while let Some(word) = words.next() {
            let Some(&option) = FILE_OPTIONS.iter().find(|option| {
                word.strip_prefix(**option)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
            }) else {
                continue;
            };
            let value = match word[option.len()..].strip_prefix('=') {
                Some(value) => value,
                None => match words.next() {
                    Some(value) => value,
                    None => break,
                },
            };
            let source = value.split('@').next().unwrap_or(value);
            collect_files(&dir.join(source), &mut inputs);
        }
        inputs
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
        let clang_windows = self.family == Family::Clang && self.os == Os::Windows;
        let table = if clang_windows {
            &CLANG_WINDOWS_CRT
        } else {
            &dialect.crt
        };
        flags::extend(&mut args, table[crt_index(spec.crt, spec.release)]);
        if (self.family == Family::ClangCl || clang_windows) && !spec.release {
            flags::extend(&mut args, CODEVIEW_GHASH);
        }
        if !msvc && !matches!(self.os, Os::Windows | Os::Emscripten) {
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
            args.push("rcs".into());
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
        if spec.crt == Crt::Static
            && spec.kind == Kind::Executable
            && self.linker.is_none()
            && !matches!(self.os, Os::MacOs | Os::Emscripten)
        {
            flags::extend(&mut args, STATIC_LINK);
        }
        if let (Some(opt), Some(subsystem)) = (dialect.subsystem, spec.subsystem) {
            opt.push(&mut args, subsystem);
        }
        if self.family == Family::Emscripten && spec.release {
            args.push("-O2".into());
        }
        args.extend(spec.flags.iter().cloned());
        if self.linker.is_none() && self.os == Os::Linux {
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
    fn emscripten_skips_native_only_flags_and_tracks_side_outputs() {
        let compile = compile(Family::Emscripten, Crt::Static);
        assert!(!compile.contains("-fPIC") && !compile.contains("-fms-runtime-lib"));
        assert!(!compile.contains("codeview"));

        let tc = Toolchain::for_tests(Family::Emscripten);
        assert_eq!(tc.artifact(Kind::Executable, "app").unwrap().0, "app.html");
        assert_eq!(tc.artifact(Kind::Static, "lib").unwrap().0, "liblib.a");
        assert!(tc.artifact(Kind::Shared, "x").is_err());
        assert_eq!(tc.side_outputs("x", &[]), ["x.js", "x.wasm"]);
        assert_eq!(
            tc.side_outputs("x", &["--preload-file".into(), "assets".into()]),
            ["x.js", "x.wasm", "x.data"]
        );
        assert!(tc.side_outputs("x", &["-sSINGLE_FILE=1".into()]).is_empty());

        let link = tc.link_args(&LinkSpec {
            kind: Kind::Executable,
            out: Path::new("app.html"),
            objs: &[PathBuf::from("a.o")],
            libs: &[],
            sys_libs: &[],
            flags: &["-sALLOW_MEMORY_GROWTH=1".to_string()],
            release: true,
            crt: Crt::Static,
            subsystem: None,
        });
        assert!(
            !link.contains(&"-static".to_string()) && !link.iter().any(|a| a.contains("rpath"))
        );
    }

    #[test]
    fn artifact_names_follow_the_target_not_the_host() {
        let name = |os, kind| {
            Toolchain::for_tests_on(Family::Clang, os)
                .artifact(kind, "x")
                .unwrap()
                .0
        };
        assert_eq!(name(Os::Windows, Kind::Executable), "x.exe");
        assert_eq!(name(Os::Linux, Kind::Executable), "x");
        assert_eq!(name(Os::Unix, Kind::Shared), "libx.so");
        assert_eq!(name(Os::MacOs, Kind::Shared), "libx.dylib");
        assert_eq!(name(Os::Windows, Kind::Shared), "x.dll");
        assert_eq!(name(Os::Windows, Kind::Static), "x.lib");
    }

    #[test]
    fn which_finds_older_emsdk_bat_shims() {
        let dir = std::env::temp_dir().join(format!("nomake_which_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("em++.bat"), "@echo off").unwrap();
        let path = std::env::join_paths([&dir]).unwrap();
        assert_eq!(which("em++", &path), Some(dir.join("em++.bat")));
        assert_eq!(which("emar", &path), None);
        std::fs::remove_dir_all(&dir).unwrap();
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
