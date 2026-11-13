//! End-to-end tests: the real `nomake` binary against small projects. The wasm ones use a fake em++/emar
//! (a `.bat` on Windows, a script elsewhere) in a folder with a space in its name, so they need no emsdk.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("nomake e2e {name} {}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Sandbox { root }
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Pretends `file` was last edited `seconds` ago, so that an edit made now is newer than any build.
    fn age(&self, file: &str, seconds: u64) {
        let path = self.root.join(file);
        let when = SystemTime::now() - Duration::from_secs(seconds);
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    /// Runs nomake in `dir` (relative to the sandbox) with the fake emsdk first on the PATH.
    fn nomake(&self, dir: &str, args: &[&str], cxx: Option<&str>) -> Output {
        let mut path = vec![self.root.join("fake tools")];
        path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_nomake"));
        command
            .args(args)
            .current_dir(self.root.join(dir))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("NOMAKE_HOME", self.root.join("home"))
            .env_remove("CXX");
        if let Some(cxx) = cxx {
            command.env("CXX", cxx);
        }
        command.output().unwrap()
    }

    fn fake_emsdk(&self) {
        let (ext, em, emar) = if cfg!(windows) {
            (".bat", FAKE_EM_BAT, FAKE_EMAR_BAT)
        } else {
            ("", FAKE_EM_SH, FAKE_EMAR_SH)
        };
        for (name, text) in [("em++", em), ("emar", emar)] {
            let file = format!("{name}{ext}");
            self.write(&format!("fake tools/{file}"), text);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let path = self.root.join("fake tools").join(&file);
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    /// Commands the fake tools were asked to run.
    fn log(&self) -> String {
        fs::read_to_string(self.root.join("fake tools/log.txt")).unwrap_or_default()
    }

    fn links(&self, output: &str) -> usize {
        self.log()
            .lines()
            .filter(|line| line.contains(output))
            .count()
    }

    fn exists(&self, file: &str) -> bool {
        self.root.join(file).exists()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const FAKE_EM_BAT: &str = "@echo off\r\n\
echo %*>>\"%~dp0log.txt\"\r\n\
if \"%~1\"==\"--version\" goto version\r\n\
set \"out=\"\r\n\
:loop\r\n\
if \"%~1\"==\"\" goto done\r\n\
if \"%~1\"==\"-o\" set \"out=%~2\"\r\n\
shift\r\n\
goto loop\r\n\
:done\r\n\
if \"%out%\"==\"\" exit /b 0\r\n\
echo fake>\"%out%\"\r\n\
if /i \"%out:~-3%\"==\".js\" echo fake>\"%out:~0,-3%.wasm\"\r\n\
if /i \"%out:~-5%\"==\".html\" echo fake>\"%out:~0,-5%.js\"\r\n\
if /i \"%out:~-5%\"==\".html\" echo fake>\"%out:~0,-5%.wasm\"\r\n\
exit /b 0\r\n\
:version\r\n\
echo emcc (Emscripten gcc/clang-like replacement) 3.1.0\r\n\
exit /b 0\r\n";

const FAKE_EMAR_BAT: &str = "@echo off\r\n\
echo emar %*>>\"%~dp0log.txt\"\r\n\
echo fake>\"%~2\"\r\n";

const FAKE_EM_SH: &str = "#!/bin/sh\n\
echo \"$@\" >> \"$(dirname \"$0\")/log.txt\"\n\
if [ \"$1\" = --version ]; then echo 'emcc (Emscripten) 3.1.0'; exit 0; fi\n\
out=\n\
while [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\n\
[ -n \"$out\" ] || exit 0\n\
echo fake > \"$out\"\n\
case \"$out\" in *.js) echo fake > \"${out%.js}.wasm\";; *.html) echo fake > \"${out%.html}.js\"; echo fake > \"${out%.html}.wasm\";; esac\n";

const FAKE_EMAR_SH: &str = "#!/bin/sh\n\
echo emar \"$@\" >> \"$(dirname \"$0\")/log.txt\"\n\
echo fake > \"$2\"\n";

fn wasm_project(sandbox: &Sandbox, dir: &str, link_flags: &str) {
    sandbox.fake_emsdk();
    sandbox.write(
        &format!("{dir}/nomake.toml"),
        &format!(
            "[package]\nname = \"app\"\ntype = \"executable\"\n\
             [target.emscripten.build]\nlink_flags = {link_flags}\n"
        ),
    );
    sandbox.write(&format!("{dir}/src/main.cpp"), "int main() {}\n");
    sandbox.write(&format!("{dir}/assets/a.txt"), "a");
    sandbox.write(&format!("{dir}/shell.html"), "<html></html>");
    sandbox.age(&format!("{dir}/assets/a.txt"), 100);
    sandbox.age(&format!("{dir}/shell.html"), 100);
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn native_example_builds_and_runs() {
    let sandbox = Sandbox::new("native");
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello");
    sandbox.write(
        "hello/nomake.toml",
        &fs::read_to_string(example.join("nomake.toml")).unwrap(),
    );
    sandbox.write(
        "hello/src/main.cpp",
        &fs::read_to_string(example.join("src/main.cpp")).unwrap(),
    );
    let build = sandbox.nomake("hello", &["build"], None);
    if text(&build).contains("no C++ compiler found") {
        return;
    }
    assert!(build.status.success(), "{}", text(&build));
    let run = sandbox.nomake("hello", &["run"], None);
    assert!(run.status.success(), "{}", text(&run));
}

#[test]
fn wasm_builds_through_a_shim_in_a_path_with_spaces_and_ignores_a_native_cxx() {
    let sandbox = Sandbox::new("shim");
    wasm_project(&sandbox, "app", "[]");
    // A native CXX must not hijack -a wasm32; the shim identifies itself as Emscripten.
    let build = sandbox.nomake("app", &["build", "-a", "wasm32"], Some("clang++"));
    assert!(build.status.success(), "{}", text(&build));
    assert!(sandbox.exists("app/build/wasm32-debug/app.html"));
    assert!(sandbox.exists("app/build/wasm32-debug/app.js"));
    assert!(sandbox.exists("app/build/wasm32-debug/app.wasm"));
    assert!(sandbox.log().contains("-c"), "the shim compiled");

    let clean = sandbox.nomake("app", &["clean", "-a", "wasm32"], None);
    assert!(clean.status.success(), "{}", text(&clean));
    assert!(!sandbox.exists("app/build/wasm32-debug"));
}

#[test]
fn wasm_relinks_when_a_file_named_by_a_link_flag_changes() {
    let sandbox = Sandbox::new("relink");
    wasm_project(
        &sandbox,
        "app",
        r#"["--embed-file", "assets", "--shell-file", "shell.html"]"#,
    );
    let build = |sandbox: &Sandbox| {
        let output = sandbox.nomake("app", &["build", "-a", "wasm32"], None);
        assert!(output.status.success(), "{}", text(&output));
    };
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 1);
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 1, "nothing changed");

    sandbox.write("app/assets/a.txt", "changed");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 2, "an embedded file changed");

    sandbox.write("app/shell.html", "<html>changed</html>");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 3, "the shell changed");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 3);
}

#[test]
fn single_file_builds_do_not_relink_for_missing_side_files() {
    let sandbox = Sandbox::new("single");
    wasm_project(&sandbox, "app", r#"["-sSINGLE_FILE=1"]"#);
    for _ in 0..2 {
        let output = sandbox.nomake("app", &["build", "-a", "wasm32"], None);
        assert!(output.status.success(), "{}", text(&output));
    }
    assert_eq!(sandbox.links("app.html"), 1);
}

#[test]
fn workspace_link_flag_paths_are_relative_to_where_nomake_runs() {
    let sandbox = Sandbox::new("workspace");
    wasm_project(&sandbox, "ws/app", r#"["--embed-file", "assets"]"#);
    sandbox.write("ws/nomake.toml", "[workspace]\nmembers = [\"app\"]\n");
    sandbox.write("ws/assets/a.txt", "a");
    sandbox.age("ws/assets/a.txt", 100);
    let build = |sandbox: &Sandbox| {
        let output = sandbox.nomake("ws", &["build", "-a", "wasm32"], None);
        assert!(output.status.success(), "{}", text(&output));
    };
    build(&sandbox);
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 1);
    sandbox.write("ws/assets/a.txt", "changed");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 2);
}

#[test]
fn wasm_shared_libraries_are_a_clear_error() {
    let sandbox = Sandbox::new("shared");
    sandbox.fake_emsdk();
    sandbox.write(
        "lib/nomake.toml",
        "[package]\nname = \"lib\"\ntype = \"shared\"\n",
    );
    sandbox.write("lib/src/lib.cpp", "int f() { return 1; }\n");
    let build = sandbox.nomake("lib", &["build", "-a", "wasm32"], None);
    assert!(!build.status.success());
    assert!(text(&build).contains("cannot build shared libraries"));
}

#[test]
fn single_file_html_cannot_run_under_node() {
    let sandbox = Sandbox::new("html");
    wasm_project(&sandbox, "app", r#"["-sSINGLE_FILE=1"]"#);
    let _ = sandbox.nomake("app", &["build", "-a", "wasm32"], None);
    let _ = fs::remove_file(sandbox.root.join("app/build/wasm32-debug/app.js"));
    let run = sandbox.nomake("app", &["run", "-a", "wasm32"], None);
    assert!(!run.status.success());
    assert!(
        text(&run).contains("single-file HTML builds must be served"),
        "{}",
        text(&run)
    );
}
