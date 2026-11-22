//! Emscripten (`wasm32`) specifics: wrapper detection, artifact/side-output rules, package-relative
//! file flag rebasing, link input tracking, default runtime settings, and `node` launching.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{which, Launch};
use crate::config::Kind;
use crate::util::{absolute, rel_str};
use crate::Result;

const FILE_OPTIONS: [&str; 6] = [
    "--shell-file",
    "--pre-js",
    "--post-js",
    "--js-library",
    "--embed-file",
    "--preload-file",
];

/// Whether `name` is a compiler wrapper around Emscripten, judged by what `--version` prints.
pub fn is_wrapper(name: &str, path_var: &OsStr) -> bool {
    which(name, path_var)
        .and_then(|path| Command::new(path).arg("--version").output().ok())
        .is_some_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains("emscripten")
        })
}

const EMSDK_REPO: &str = "https://github.com/emscripten-core/emsdk";

/// Environment of an activated emsdk, installing it into the nomake cache first when `em++` is not
/// already reachable. `EMSDK` points at an existing install to use instead.
pub fn provision() -> Result<Vec<(String, String)>> {
    let script = if cfg!(windows) { "emsdk.bat" } else { "emsdk" };
    let dir = match std::env::var_os("EMSDK").map(PathBuf::from) {
        Some(dir) if dir.join(script).is_file() => dir,
        _ => crate::registry::cache_dir()?.join("emsdk"),
    };
    let marker = dir.join(".nomake_env");
    if let Ok(text) = fs::read_to_string(&marker) {
        let env: Vec<(String, String)> = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        if !env.is_empty() {
            return Ok(env);
        }
    }

    if !dir.join(script).is_file() {
        eprintln!(
            "nomake: em++ not found, installing emsdk into {}",
            dir.display()
        );
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)?;
        }
        run_visible(
            Command::new("git")
                .args(["clone", "--depth", "1", EMSDK_REPO])
                .arg(&dir),
        )?;
    }
    for step in ["install", "activate"] {
        eprintln!("nomake: emsdk {step} latest (one-time download, a few hundred MB)");
        run_visible(
            Command::new(dir.join(script))
                .args([step, "latest"])
                .current_dir(&dir),
        )?;
    }

    let env = activated_env(&dir)?;
    fs::write(
        &marker,
        env.iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect::<String>(),
    )?;
    Ok(env)
}

fn run_visible(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .map_err(|e| format!("cannot run {:?}: {e}", command.get_program()))?;
    if !status.success() {
        bail!("{:?} failed ({status})", command.get_program());
    }
    Ok(())
}

/// Variables that `emsdk_env` changes, so builds can use the SDK without a configured shell.
fn activated_env(dir: &Path) -> Result<Vec<(String, String)>> {
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        Command::new("cmd")
            .raw_arg(format!(
                "/d /s /c \"\"{}\" >nul 2>&1 && set\"",
                dir.join("emsdk_env.bat").display()
            ))
            .current_dir(dir)
            .output()?
    };
    #[cfg(not(windows))]
    let output = Command::new("sh")
        .arg("-c")
        .arg(". ./emsdk_env.sh >/dev/null 2>&1; env")
        .current_dir(dir)
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let env: Vec<(String, String)> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| !key.is_empty() && !key.starts_with('='))
        .filter(|(key, value)| std::env::var(key).ok().as_deref() != Some(*value))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    if env.is_empty() {
        bail!(
            "emsdk_env produced no environment (delete {} and retry)",
            dir.display()
        );
    }
    Ok(env)
}

pub fn artifact(kind: Kind, name: &str) -> Result<(String, String)> {
    Ok(match kind {
        Kind::Executable => {
            let file = format!("{name}.html");
            (file.clone(), file)
        }
        Kind::Test => {
            let file = format!("{name}.js");
            (file.clone(), file)
        }
        Kind::Static => {
            let file = format!("lib{name}.a");
            (file.clone(), file)
        }
        Kind::Shared => bail!(
            "Emscripten cannot build shared libraries (use type = \"static\" or link = \"static\")"
        ),
    })
}

fn setting_at(flags: &[String], index: usize) -> Option<&str> {
    match flags[index].as_str() {
        "-s" => flags.get(index + 1).map(String::as_str),
        flag => flag.strip_prefix("-s"),
    }
}

pub fn single_file(link_flags: &[String]) -> bool {
    (0..link_flags.len()).any(|i| {
        matches!(
            setting_at(link_flags, i),
            Some("SINGLE_FILE" | "SINGLE_FILE=1")
        )
    })
}

fn mentions_setting(link_flags: &[String], key: &str) -> bool {
    (0..link_flags.len()).any(|i| {
        setting_at(link_flags, i).is_some_and(|setting| {
            setting == key
                || setting
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    })
}

const DEFAULT_SHELL: &str = include_str!("emscripten_shell.html");

pub fn push_default_shell(
    kind: Kind,
    flags: &mut Vec<String>,
    out_dir: &Path,
    root_dir: &Path,
) -> Result<()> {
    let has_shell = flags
        .iter()
        .any(|flag| flag == "--shell-file" || flag.starts_with("--shell-file="));
    if kind != Kind::Executable || has_shell {
        return Ok(());
    }
    let path = out_dir.join("shell.html");
    if fs::read_to_string(&path).ok().as_deref() != Some(DEFAULT_SHELL) {
        fs::create_dir_all(out_dir)?;
        fs::write(&path, DEFAULT_SHELL)?;
    }
    flags.push("--shell-file".into());
    flags.push(rebase_path(&path.to_string_lossy(), root_dir, root_dir));
    Ok(())
}

/// Default flags placed before user `link_flags` so explicit user settings still win.
pub fn push_link_defaults(args: &mut Vec<String>, user_flags: &[String], release: bool) {
    if !mentions_setting(user_flags, "ALLOW_MEMORY_GROWTH") {
        args.push("-sALLOW_MEMORY_GROWTH=1".into());
    }
    if release {
        args.push("-O2".into());
    }
}

/// Files the link writes besides the primary artifact (`<name>.html` or `<name>.js`).
pub fn side_outputs(kind: Kind, name: &str, link_flags: &[String]) -> Vec<String> {
    if single_file(link_flags) {
        return Vec::new();
    }
    let mut files = Vec::new();
    if kind != Kind::Test {
        files.push(format!("{name}.js"));
    }
    files.push(format!("{name}.wasm"));
    if link_flags
        .iter()
        .any(|flag| flag.starts_with("--preload-file"))
    {
        files.push(format!("{name}.data"));
    }
    files
}

pub fn launch(kind: Kind, out: &Path, link_flags: &[String]) -> Option<Launch> {
    match kind {
        Kind::Test => Some(Launch::Node(out.to_path_buf())),
        Kind::Executable if single_file(link_flags) => Some(Launch::Unsupported {
            artifact: out.to_path_buf(),
            reason: "single-file HTML builds must be served over HTTP",
        }),
        Kind::Executable => Some(Launch::Node(out.with_extension("js"))),
        Kind::Static | Kind::Shared => None,
    }
}

fn rebase_path(path: &str, pkg_dir: &Path, root_dir: &Path) -> String {
    let full = absolute(&pkg_dir.join(path));
    if full.starts_with(root_dir) {
        rel_str(root_dir, &full)
    } else {
        full.to_string_lossy().replace('\\', "/")
    }
}

fn rebase_value(option: &str, value: &str, pkg_dir: &Path, root_dir: &Path) -> String {
    if matches!(option, "--embed-file" | "--preload-file") {
        let (src, dst) = match value.split_once('@') {
            Some((src, dst)) => (src, dst),
            None => {
                let dst = value
                    .trim_start_matches("./")
                    .trim_start_matches(".\\")
                    .trim_end_matches(['/', '\\']);
                (value, dst)
            }
        };
        if Path::new(src).is_absolute() {
            return value.to_string();
        }
        format!("{}@{dst}", rebase_path(src, pkg_dir, root_dir))
    } else if Path::new(value).is_relative() {
        rebase_path(value, pkg_dir, root_dir)
    } else {
        value.to_string()
    }
}

/// Rewrites file-bearing flags declared in `pkg_dir` so they resolve from `root_dir` while keeping virtual mount paths.
pub fn rebase_link_flags(flags: &[String], pkg_dir: &Path, root_dir: &Path) -> Vec<String> {
    if pkg_dir == root_dir {
        return flags.to_vec();
    }
    let mut out = Vec::with_capacity(flags.len());
    let mut index = 0;
    while index < flags.len() {
        let word = &flags[index];
        let matched = FILE_OPTIONS.iter().copied().find(|option| {
            word.strip_prefix(*option)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
        });
        match matched {
            Some(option) => match word[option.len()..].strip_prefix('=') {
                Some(value) => {
                    out.push(format!(
                        "{option}={}",
                        rebase_value(option, value, pkg_dir, root_dir)
                    ));
                    index += 1;
                }
                None => {
                    out.push(word.clone());
                    if let Some(value) = flags.get(index + 1) {
                        out.push(rebase_value(option, value, pkg_dir, root_dir));
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            },
            None => {
                out.push(word.clone());
                index += 1;
            }
        }
    }
    out
}

/// `path` itself, or every file and directory under it when it is a directory.
fn collect_files(path: &Path, into: &mut Vec<PathBuf>) {
    if path.is_dir() {
        into.push(path.to_path_buf());
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
    } else {
        into.push(path.to_path_buf());
    }
}

/// Host files and directories read by file-bearing Emscripten link flags.
pub fn link_inputs(flags: &[String], root_dir: &Path) -> Vec<PathBuf> {
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
        collect_files(&absolute(&root_dir.join(source)), &mut inputs);
    }
    inputs
}
