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
set \"single=\"\r\n\
:loop\r\n\
if \"%~1\"==\"\" goto done\r\n\
if \"%~1\"==\"-o\" set \"out=%~2\"\r\n\
if /i \"%~1\"==\"-sSINGLE_FILE=1\" set \"single=1\"\r\n\
shift\r\n\
goto loop\r\n\
:done\r\n\
if \"%out%\"==\"\" exit /b 0\r\n\
echo fake>\"%out%\"\r\n\
if /i \"%out:~-3%\"==\".js\" echo fake>\"%out:~0,-3%.wasm\"\r\n\
if /i \"%out:~-3%\"==\".js\" echo process.exit(0^);>\"%out%\"\r\n\
if /i \"%out:~-5%\"==\".html\" if not defined single echo process.exit(0^);>\"%out:~0,-5%.js\"\r\n\
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
single=0\n\
while [ $# -gt 0 ]; do\n\
  [ \"$1\" = -o ] && out=$2\n\
  [ \"$1\" = -sSINGLE_FILE=1 ] && single=1\n\
  shift\n\
done\n\
[ -n \"$out\" ] || exit 0\n\
echo fake > \"$out\"\n\
case \"$out\" in *.js) echo fake > \"${out%.js}.wasm\"; echo 'process.exit(0);' > \"$out\";; *.html) [ \"$single\" = 1 ] || echo 'process.exit(0);' > \"${out%.html}.js\"; echo fake > \"${out%.html}.wasm\";; esac\n";

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
    sandbox.write(&format!("{dir}/assets/nested/a.txt"), "a");
    sandbox.write(&format!("{dir}/shell.html"), "<html></html>");
    sandbox.age(&format!("{dir}/assets/nested/a.txt"), 100);
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
fn test_packages_get_a_runner_with_filtering_and_crash_isolation() {
    let sandbox = Sandbox::new("runner");
    sandbox.write(
        "suite/nomake.toml",
        "[package]\nname = \"suite\"\ntype = \"test\"\n",
    );
    sandbox.write(
        "suite/src/a.c",
        "#include <nomake/tests.h>\n\
         TEST(passes) { TEST_CHECK(1 + 1 == 2); }\n\
         TEST(skips) { TEST_SKIP(\"later\"); }\n",
    );
    sandbox.write(
        "suite/src/b.cpp",
        "#include <nomake/tests.h>\nTEST(cpp_passes) { TEST_CHECK(true); }\n",
    );
    let pass = sandbox.nomake("suite", &["test"], None);
    if text(&pass).contains("no C++ compiler found") {
        return;
    }
    assert!(pass.status.success(), "{}", text(&pass));
    assert!(text(&pass).contains("2 passed, 0 failed, 1 skipped"));

    sandbox.write(
        "suite/src/c.c",
        "#include <nomake/tests.h>\n\
         TEST(soft) { TEST_CHECK_MSG(1 == 2, \"got %d\", 7); }\n\
         TEST(crash) { volatile int *p = 0; *p = 1; }\n",
    );
    let fail = sandbox.nomake("suite", &["test"], None);
    assert!(!fail.status.success());
    let out = text(&fail);
    assert!(out.contains("FAIL soft: 1 == 2 (got 7)"), "{out}");
    assert!(out.contains("CRASH crash"), "{out}");
    assert!(out.contains("2 passed, 2 failed, 1 skipped"), "{out}");

    let filtered = sandbox.nomake("suite", &["test", "--", "--filter", "*passes"], None);
    assert!(filtered.status.success(), "{}", text(&filtered));
    assert!(text(&filtered).contains("2 passed, 0 failed, 0 skipped"));
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
    assert!(
        sandbox.log().contains("--shell-file")
            && sandbox.exists("app/build/wasm32-debug/shell.html"),
        "executables get nomake's full-page shell by default"
    );
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
    assert!(
        !sandbox.exists("app/build/wasm32-debug/shell.html"),
        "a user --shell-file replaces the default"
    );
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 1, "nothing changed");

    sandbox.write("app/assets/nested/a.txt", "changed");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 2, "an embedded file changed");

    sandbox.write("app/shell.html", "<html>changed</html>");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 3, "the shell changed");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 3);

    fs::remove_file(sandbox.root.join("app/assets/nested/a.txt")).unwrap();
    build(&sandbox);
    assert_eq!(
        sandbox.links("app.html"),
        4,
        "a nested embedded file was removed"
    );
    sandbox.write("app/assets/nested/new.txt", "new");
    build(&sandbox);
    assert_eq!(
        sandbox.links("app.html"),
        5,
        "a nested embedded file was added"
    );
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
fn switching_to_single_file_does_not_run_a_stale_js_companion() {
    let sandbox = Sandbox::new("single stale js");
    wasm_project(&sandbox, "app", "[]");

    let regular = sandbox.nomake("app", &["run", "-a", "wasm32"], None);
    assert!(regular.status.success(), "{}", text(&regular));
    assert!(sandbox.exists("app/build/wasm32-debug/app.js"));
    assert_eq!(
        fs::read_to_string(sandbox.root.join("app/build/wasm32-debug/app.js")).unwrap(),
        "process.exit(0);\r\n"
    );

    sandbox.write(
        "app/nomake.toml",
        "[package]\nname = \"app\"\ntype = \"executable\"\n\
         [target.emscripten.build]\nlink_flags = [\"-sSINGLE_FILE=1\"]\n",
    );
    let single = sandbox.nomake("app", &["run", "-a", "wasm32"], None);
    assert!(!single.status.success());
    assert!(
        text(&single).contains("single-file HTML"),
        "{}",
        text(&single)
    );
    assert!(sandbox.exists("app/build/wasm32-debug/app.js"));
}

#[test]
fn package_relative_link_flags_work_across_workspaces_and_dependencies() {
    let sandbox = Sandbox::new("workspace");
    wasm_project(&sandbox, "ws/app", r#"["--embed-file", "assets"]"#);
    sandbox.write(
        "ws/app/nomake.toml",
        "[package]\nname = \"app\"\ntype = \"executable\"\n\
         [dependencies]\njsdep = { path = \"../jsdep\" }\n\
         [target.emscripten.build]\nlink_flags = [\"--embed-file\", \"assets\"]\n",
    );
    sandbox.write(
        "ws/jsdep/nomake.toml",
        "[package]\nname = \"jsdep\"\ntype = \"static\"\n\
         [target.emscripten.build]\nlink_flags = [\"--js-library\", \"js/bridge.js\"]\n",
    );
    sandbox.write("ws/jsdep/src/lib.cpp", "int bridge() { return 1; }\n");
    sandbox.write(
        "ws/jsdep/js/bridge.js",
        "mergeInto(LibraryManager.library, {});",
    );
    sandbox.age("ws/jsdep/js/bridge.js", 100);
    sandbox.write("ws/nomake.toml", "[workspace]\nmembers = [\"app\"]\n");
    let build = |sandbox: &Sandbox| {
        let output = sandbox.nomake("ws", &["build", "-a", "wasm32"], None);
        assert!(output.status.success(), "{}", text(&output));
    };
    build(&sandbox);
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 1);
    assert!(sandbox.log().contains("app/assets@assets"));
    assert!(sandbox.log().contains("jsdep/js/bridge.js"));

    sandbox.write("ws/app/assets/nested/a.txt", "changed");
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 2);

    sandbox.write(
        "ws/jsdep/js/bridge.js",
        "mergeInto(LibraryManager.library, { x: 1 });",
    );
    build(&sandbox);
    assert_eq!(sandbox.links("app.html"), 3);
}

#[test]
fn wasm_falls_back_to_static_for_dual_mode_libs_and_runs_host_generators_and_js_tests() {
    let sandbox = Sandbox::new("zero config wasm");
    sandbox.fake_emsdk();

    // Dual-mode library: shared-first for fast native iteration, automatic static on wasm32.
    sandbox.write(
        "dual/nomake.toml",
        "[package]\nname = \"dual\"\ntype = [\"shared\", \"static\"]\n[build]\nshared_defines = [\"DUAL_DLL\"]\n",
    );
    sandbox.write("dual/src/dual.cpp", "int dual() { return 7; }\n");

    // Host C++ generator tool: built with the host compiler and run to produce generated C++ for wasm.
    sandbox.write(
        "gen/nomake.toml",
        "[package]\nname = \"gen\"\ntype = \"executable\"\n",
    );
    sandbox.write(
        "gen/src/main.cpp",
        "#include <cstdio>\n#include <cstring>\n\
         int main(int argc, char** argv) {\n\
             const char* out_dir = nullptr;\n\
             const char* depfile = nullptr;\n\
             for (int i = 1; i + 1 < argc; ++i) {\n\
                 if (std::strcmp(argv[i], \"--out-dir\") == 0) out_dir = argv[++i];\n\
                 else if (std::strcmp(argv[i], \"--depfile\") == 0) depfile = argv[++i];\n\
             }\n\
             char hdr[512];\n\
             std::snprintf(hdr, sizeof hdr, \"%s/val.gen.hpp\", out_dir);\n\
             FILE* f = std::fopen(hdr, \"w\");\n\
             std::fputs(\"inline int gen_val() { return 42; }\\n\", f);\n\
             std::fclose(f);\n\
             FILE* d = std::fopen(depfile, \"w\");\n\
             std::fprintf(d, \"%s: %s\\n\", hdr, argv[argc - 1]);\n\
             std::fclose(d);\n\
             return 0;\n\
         }\n",
    );

    // App with an Emscripten overlay on top of default `src/`, dual-mode dep, and host generator.
    sandbox.write(
        "app/nomake.toml",
        "[package]\nname = \"app\"\ntype = \"executable\"\n\
         [dependencies]\ndual = { path = \"../dual\" }\ngen = { path = \"../gen\" }\n\
         [[generate]]\ntool = \"gen\"\ninputs = [\"schema/val.in\"]\noutputs = [\"{stem}.gen.hpp\"]\n\
         [target.emscripten]\nsources.files = [\"web/extra.cpp\"]\n",
    );
    sandbox.write("app/schema/val.in", "42\n");
    sandbox.write(
        "app/src/main.cpp",
        "#include \"val.gen.hpp\"\nint main() { return gen_val() == 42 ? 0 : 1; }\n",
    );
    sandbox.write("app/web/extra.cpp", "int web_only() { return 1; }\n");

    let native = sandbox.nomake("app", &["build"], None);
    if !text(&native).contains("no C++ compiler found") {
        assert!(native.status.success(), "{}", text(&native));
        let wasm = sandbox.nomake("app", &["build", "-a", "wasm32"], None);
        assert!(wasm.status.success(), "{}", text(&wasm));
        assert!(sandbox.exists("app/build/wasm32-debug/libdual.a"));
        assert!(sandbox.exists("app/build/wasm32-debug/gen/app/val.gen.hpp"));
        assert!(!sandbox.log().contains("-DDUAL_DLL"));
    }

    // `type = "test"` builds `<name>.js` directly without an HTML shell and runs under node.
    sandbox.write(
        "suite/nomake.toml",
        "[package]\nname = \"suite\"\ntype = \"test\"\n",
    );
    sandbox.write(
        "suite/src/test.cpp",
        "#include <nomake/tests.h>\nTEST(ok) { TEST_CHECK(true); }\n",
    );
    let test = sandbox.nomake("suite", &["test", "-a", "wasm32"], None);
    assert!(test.status.success(), "{}", text(&test));
    assert!(sandbox.exists("suite/build/wasm32-debug/suite.js"));
    assert!(!sandbox.exists("suite/build/wasm32-debug/suite.html"));
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
