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
        if self.root.join("registry").is_dir() {
            command.env("NOMAKE_REGISTRY", self.root.join("registry"));
        }
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

#[test]
fn workspace_walkup_compdb_refresh_primary_run_and_per_package_clean() {
    let sandbox = Sandbox::new("ws unified");
    sandbox.write(
        "ws/nomake.toml",
        "[workspace]\nmembers = [\"mathlib\", \"app\", \"tool\", \"suite\"]\n",
    );
    sandbox.write(
        "ws/deps/extlib/nomake.toml",
        "[package]\nname = \"extlib\"\ntype = \"static\"\n",
    );
    sandbox.write("ws/deps/extlib/src/ext.cpp", "int ext() { return 2; }\n");
    sandbox.write(
        "ws/mathlib/nomake.toml",
        "[package]\nname = \"mathlib\"\ntype = \"static\"\n[dependencies]\nextlib = { path = \"../deps/extlib\" }\n",
    );
    sandbox.write("ws/mathlib/include/mathlib.h", "#pragma once\nint val();\n");
    sandbox.write(
        "ws/mathlib/src/mathlib.cpp",
        "#include <mathlib.h>\nint ext();\nint val() { return 40 + ext(); }\n",
    );
    sandbox.write(
        "ws/app/nomake.toml",
        "[package]\nname = \"app\"\n[dependencies]\nmathlib = { path = \"../mathlib\" }\n",
    );
    sandbox.write("ws/app/data.txt", "from-app");
    sandbox.write(
        "ws/app/src/main.cpp",
        "#include <cstdio>\n#include <mathlib.h>\nint main() {\n\
         FILE* f = std::fopen(\"data.txt\", \"rb\");\n\
         if (!f) return 2;\nstd::fclose(f);\n\
         std::printf(\"app=%d\\n\", val());\nreturn 0;\n}\n",
    );
    sandbox.write("ws/tool/nomake.toml", "[package]\nname = \"tool\"\n");
    sandbox.write("ws/tool/tool.txt", "from-tool");
    sandbox.write(
        "ws/tool/src/main.cpp",
        "#include <cstdio>\nint main() {\n\
         FILE* f = std::fopen(\"tool.txt\", \"rb\");\n\
         if (!f) return 3;\nstd::fclose(f);\n\
         std::puts(\"ran-tool\");\nreturn 0;\n}\n",
    );
    sandbox.write(
        "ws/suite/nomake.toml",
        "[package]\nname = \"suite\"\ntype = \"test\"\n[dependencies]\nmathlib = { path = \"../mathlib\" }\n",
    );
    sandbox.write(
        "ws/suite/src/t.cpp",
        "#include <nomake/tests.h>\n#include <mathlib.h>\nTEST(v) { TEST_CHECK(val() == 42); }\n",
    );

    // Walk-up from `ws/app` builds only `app` (+ deps) into `<ws>/build`, never `ws/app/build`.
    let sub_build = sandbox.nomake("ws/app", &["build"], None);
    if text(&sub_build).contains("no C++ compiler found") {
        return;
    }
    assert!(sub_build.status.success(), "{}", text(&sub_build));
    assert!(!sandbox.exists("ws/app/build"));
    assert!(sandbox.exists("ws/build"));

    // `compdb` includes `suite`, and a subsequent `nomake build` after editing `mathlib` keeps `suite` in compile_commands.json!
    let cdb = sandbox.nomake("ws", &["compdb"], None);
    assert!(cdb.status.success(), "{}", text(&cdb));
    let before = fs::read_to_string(sandbox.root.join("ws/compile_commands.json")).unwrap();
    assert!(before.contains("t.cpp"), "{before}");
    sandbox.write(
        "ws/mathlib/nomake.toml",
        "[package]\nname = \"mathlib\"\ntype = \"static\"\n[build]\ndefines = [\"EXTRA=1\"]\n[dependencies]\nextlib = { path = \"../deps/extlib\" }\n",
    );
    let full_build = sandbox.nomake("ws", &["build"], None);
    assert!(full_build.status.success(), "{}", text(&full_build));
    let after = fs::read_to_string(sandbox.root.join("ws/compile_commands.json")).unwrap();
    assert!(
        after.contains("EXTRA=1") && after.contains("t.cpp"),
        "{after}"
    );

    // `nomake run` at workspace root with 2 executables and no `primary` errors naming both and suggesting `primary`.
    let ambig_run = sandbox.nomake("ws", &["run"], None);
    assert!(!ambig_run.status.success());
    let err = text(&ambig_run);
    assert!(
        err.contains("app, tool") && err.contains("primary ="),
        "{err}"
    );

    // `cd ws/app && nomake run` runs `app` with CWD = `ws/app`.
    let app_run = sandbox.nomake("ws/app", &["run"], None);
    assert!(app_run.status.success(), "{}", text(&app_run));
    assert!(text(&app_run).contains("app=42"), "{}", text(&app_run));

    // Setting `primary = "tool"` lets `nomake run` at root (and `nomake run -w` from `ws/app`) run `tool` in `ws/tool`.
    sandbox.write(
        "ws/nomake.toml",
        "[workspace]\nmembers = [\"mathlib\", \"app\", \"tool\", \"suite\"]\nprimary = \"tool\"\n",
    );
    let root_run = sandbox.nomake("ws", &["run"], None);
    assert!(root_run.status.success(), "{}", text(&root_run));
    assert!(text(&root_run).contains("ran-tool"), "{}", text(&root_run));
    let ws_flag_run = sandbox.nomake("ws/app", &["run", "-w"], None);
    assert!(ws_flag_run.status.success(), "{}", text(&ws_flag_run));
    assert!(text(&ws_flag_run).contains("ran-tool"));

    // `cd ws/app && nomake test` runs workspace tests, while explicit `nomake test app` still errors.
    let sub_test = sandbox.nomake("ws/app", &["test"], None);
    assert!(sub_test.status.success(), "{}", text(&sub_test));
    assert!(text(&sub_test).contains("1 test(s) passed"));
    let explicit_bad = sandbox.nomake("ws", &["test", "app"], None);
    assert!(!explicit_bad.status.success());
    assert!(text(&explicit_bad).contains("not a test"));

    // Non-member dep `nomake build extlib` and per-package `nomake clean mathlib` preserve `tool`'s objects.
    assert!(sandbox
        .nomake("ws", &["clean", "mathlib"], None)
        .status
        .success());
    let cfg_dir = fs::read_dir(sandbox.root.join("ws/build"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    assert!(!cfg_dir.join("obj/mathlib").exists());
    assert!(cfg_dir.join("obj/tool").exists());
    let dep_build = sandbox.nomake("ws", &["build", "extlib"], None);
    assert!(dep_build.status.success(), "{}", text(&dep_build));
}

#[test]
fn test_only_workspace_builds_cleanly_and_partial_builds_keep_lockfile() {
    let sandbox = Sandbox::new("test only lock");
    // Set up a tiny local git repo + registry recipe for `tiny` 1.0.0.
    let repo = sandbox.root.join("tiny_repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/tiny.c"), "int tiny(void) { return 7; }\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "core.autocrlf", "false"],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
        &["add", "."],
        &["commit", "-qm", "init"],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    }
    let rev = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let rev = rev.trim();
    let git_url = repo.display().to_string().replace('\\', "/");
    sandbox.write(
        "registry/tiny/1.0.0/nomake.toml",
        &format!(
            "[package]\nname = \"tiny\"\ntype = \"static\"\n[source]\ngit = \"{git_url}\"\nrev = \"{rev}\"\n"
        ),
    );

    // 1. Test-only workspace `nomake build` exits 0 cleanly without panicking.
    sandbox.write(
        "only_tests/nomake.toml",
        "[workspace]\nmembers = [\"suite\"]\n",
    );
    sandbox.write(
        "only_tests/suite/nomake.toml",
        "[package]\nname = \"suite\"\ntype = \"test\"\n",
    );
    sandbox.write(
        "only_tests/suite/src/t.c",
        "#include <nomake/tests.h>\nTEST(ok) { TEST_CHECK(1); }\n",
    );
    let empty_build = sandbox.nomake("only_tests", &["build"], None);
    assert!(empty_build.status.success(), "{}", text(&empty_build));
    assert!(text(&empty_build).contains("nothing to build"));

    // 2. Mixed workspace: `nomake test` and `nomake build mathlib` never drop `app`'s `tiny` lock entry!
    sandbox.write(
        "mixed/nomake.toml",
        "[workspace]\nmembers = [\"mathlib\", \"app\", \"suite\"]\n",
    );
    sandbox.write(
        "mixed/mathlib/nomake.toml",
        "[package]\nname = \"mathlib\"\ntype = \"static\"\n",
    );
    sandbox.write("mixed/mathlib/src/m.c", "int m(void) { return 1; }\n");
    sandbox.write(
        "mixed/app/nomake.toml",
        "[package]\nname = \"app\"\n[dependencies]\ntiny = { version = \"1\" }\n",
    );
    sandbox.write(
        "mixed/app/src/main.c",
        "int tiny(void);\nint main(void) { return tiny() - 7; }\n",
    );
    sandbox.write(
        "mixed/suite/nomake.toml",
        "[package]\nname = \"suite\"\ntype = \"test\"\n",
    );
    sandbox.write(
        "mixed/suite/src/t.c",
        "#include <nomake/tests.h>\nTEST(ok) { TEST_CHECK(1); }\n",
    );

    let full = sandbox.nomake("mixed", &["build"], None);
    if text(&full).contains("no C++ compiler found") {
        return;
    }
    assert!(full.status.success(), "{}", text(&full));
    let lock_before = fs::read_to_string(sandbox.root.join("mixed/nomake.lock")).unwrap();
    assert!(lock_before.contains("name = \"tiny\""), "{lock_before}");

    let sub = sandbox.nomake("mixed", &["build", "mathlib"], None);
    assert!(sub.status.success(), "{}", text(&sub));
    assert_eq!(
        fs::read_to_string(sandbox.root.join("mixed/nomake.lock")).unwrap(),
        lock_before
    );

    let tested = sandbox.nomake("mixed", &["test"], None);
    assert!(tested.status.success(), "{}", text(&tested));
    assert_eq!(
        fs::read_to_string(sandbox.root.join("mixed/nomake.lock")).unwrap(),
        lock_before
    );
}
