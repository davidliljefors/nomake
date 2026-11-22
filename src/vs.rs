//! `nomake vs`: NMake-style Visual Studio solution in build/vs/. The IDE only browses and edits; builds call back into nomake.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::commands::out_dir;
use crate::config::{Kind, Os};
use crate::dag::{compile_scope, write_test_support};
use crate::scanner::{self, is_header, Pkg, Workspace};
use crate::toolchain::{Arch, Toolchain};
use crate::util::{hash, hash_seeded};
use crate::workspace::Project;
use crate::Result;

const CPP_PROJECT_TYPE: &str = "{8BC9CEB8-8B4A-11D0-9A44-00A0C91F9C40}";
const SOLUTION_FOLDER_TYPE: &str = "{2150E333-8FDC-42A3-9474-1A3956D46DE8}";
const MSBUILD_NAMESPACE: &str = "http://schemas.microsoft.com/developer/msbuild/2003";
const PLATFORMS: [(&str, Arch); 3] = [
    ("x64", Arch::X64),
    ("Win32", Arch::X86),
    ("ARM64", Arch::Arm64),
];
const PROFILES: [(&str, bool); 2] = [("Debug", false), ("Release", true)];

struct Config {
    profile: &'static str,
    platform: &'static str,
    arch: Arch,
    release: bool,
}

impl Config {
    fn key(&self) -> String {
        format!("{}|{}", self.profile, self.platform)
    }
}

fn configs(archs: &[Arch]) -> Vec<Config> {
    let mut configs = Vec::new();
    for (platform, arch) in PLATFORMS
        .into_iter()
        .filter(|(_, arch)| archs.contains(arch))
    {
        for (profile, release) in PROFILES {
            configs.push(Config {
                profile,
                platform,
                arch,
                release,
            });
        }
    }
    configs
}

struct Context<'a> {
    ws: &'a Workspace,
    tc: &'a Toolchain,
    archs: &'a [Arch],
    root_dir: PathBuf,
    nomake: PathBuf,
}

struct Item {
    tag: &'static str,
    path: PathBuf,
    filter: Option<String>,
}

struct VsProject {
    name: String,
    is_member: bool,
    is_primary: bool,
    is_test: bool,
    is_executable: bool,
    /// Part of `Build Solution` (`.Build.0`): non-test workspace members only, matching `nomake build`.
    builds: bool,
    order: usize,
    deps: Vec<String>,
}

/// Generates `<root_dir>/build/vs/<name>.sln` and per-package `.vcxproj` files for `project`.
pub fn generate(project: &Project, archs: &[Arch]) -> Result<()> {
    if archs.contains(&Arch::Wasm32) {
        bail!("`nomake vs` cannot target wasm32 (use `nomake compdb -a wasm32`)");
    }
    let root_dir = &project.root_dir;
    let vs_dir = root_dir.join("build").join("vs");
    fs::create_dir_all(&vs_dir)?;
    let member_dirs = project.member_dirs();
    let ws = scanner::load_project(
        root_dir,
        &member_dirs,
        &member_dirs,
        project.has_root_package,
        None,
        Os::host(),
    )?;
    let tc_cfg = if project.is_workspace {
        &project.toolchain
    } else {
        &ws.root().toolchain
    };
    let tc = Toolchain::detect(tc_cfg, Arch::host(), &root_dir.join("build"))?;
    for pkg in ws.pkgs.iter().filter(|pkg| pkg.kind == Kind::Test) {
        for config in configs(archs) {
            let out = out_dir(root_dir, config.arch, config.release);
            write_test_support(&out.join("gen").join(&pkg.name))?;
        }
    }
    let ctx = Context {
        ws: &ws,
        tc: &tc,
        archs,
        root_dir: root_dir.clone(),
        nomake: std::env::current_exe()?,
    };

    let mut projects = Vec::new();
    for (index, pkg) in ws.pkgs.iter().enumerate() {
        let items = collect_items(pkg);
        fs::write(
            vs_dir.join(format!("{}.vcxproj", pkg.name)),
            project_xml(&ctx, index, &items),
        )?;
        fs::write(
            vs_dir.join(format!("{}.vcxproj.filters", pkg.name)),
            filters_xml(pkg, &items),
        )?;
        let member_pos = project.members.iter().position(|m| m.dir == pkg.root);
        let is_member = ws.roots.contains(&index);
        let is_primary = is_member && project.primary.is_some_and(|p| Some(p) == member_pos);
        let is_test = pkg.kind == Kind::Test;
        let mut deps = Vec::new();
        for &dep in pkg.deps.iter().chain(&pkg.peers) {
            let dep_name = &ws.pkgs[dep].name;
            if dep_name != &pkg.name && !deps.contains(dep_name) {
                deps.push(dep_name.clone());
            }
        }
        projects.push(VsProject {
            name: pkg.name.clone(),
            is_member,
            is_primary,
            is_test,
            is_executable: pkg.kind == Kind::Executable,
            builds: is_member && !is_test,
            order: member_pos.unwrap_or(project.members.len() + index),
            deps,
        });
    }
    // Visual Studio starts the first project of the solution: prefer `[workspace] primary`, then non-test executables.
    projects.sort_by_key(|p| {
        (
            !p.is_member,
            !p.is_primary,
            p.is_test,
            !p.is_executable,
            p.order,
        )
    });
    let solution_name = if project.is_workspace {
        root_dir.file_name().map_or_else(
            || "nomake".to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    } else {
        ws.root().name.clone()
    };
    let solution = vs_dir.join(format!("{solution_name}.sln"));
    fs::write(&solution, solution_text(&projects, archs))?;
    println!(
        "nomake: wrote {} ({} projects)",
        solution.display(),
        projects.len()
    );
    Ok(())
}

fn guid(seed: &str) -> String {
    let high = hash(seed.as_bytes());
    let low = hash_seeded(seed.as_bytes(), 1);
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        high >> 32,
        (high >> 16) & 0xFFFF,
        high & 0xFFFF,
        low >> 48,
        low & 0xFFFF_FFFF_FFFF
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn property(name: &str, value: &str) -> String {
    format!("    <{name}>{}</{name}>\n", xml(value))
}

fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn condition(config: &Config) -> String {
    format!("'$(Configuration)|$(Platform)'=='{}'", config.key())
}

fn collect_items(pkg: &Pkg) -> Vec<Item> {
    let mut items = Vec::new();
    for source in &pkg.sources {
        items.push(make_item("ClCompile", pkg, source));
    }
    for header in collect_headers(pkg) {
        items.push(make_item("ClInclude", pkg, &header));
    }
    items.push(make_item("None", pkg, &pkg.root.join("nomake.toml")));
    items
}

fn make_item(tag: &'static str, pkg: &Pkg, path: &Path) -> Item {
    let relative = path.strip_prefix(&pkg.root).unwrap_or(Path::new(""));
    let folder = relative
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .filter(|dir| !dir.is_empty());
    Item {
        tag,
        path: path.to_path_buf(),
        filter: folder,
    }
}

/// Headers next to the sources plus everything under the include dirs, so excluded folders stay out of the tree.
fn collect_headers(pkg: &Pkg) -> BTreeSet<PathBuf> {
    let mut headers = BTreeSet::new();
    let source_dirs: BTreeSet<&Path> = pkg
        .sources
        .iter()
        .filter_map(|source| source.parent())
        .collect();
    for dir in source_dirs {
        add_headers(dir, false, &mut headers);
    }
    for dir in pkg.include_public.iter().chain(&pkg.include_private) {
        add_headers(dir, *dir != pkg.root, &mut headers);
    }
    headers
}

fn add_headers(dir: &Path, recursive: bool, headers: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if recursive && !name.starts_with('.') && name != "build" {
                add_headers(&path, true, headers);
            }
        } else if is_header(&path) {
            headers.insert(path);
        }
    }
}

fn project_xml(ctx: &Context, index: usize, items: &[Item]) -> String {
    let pkg = &ctx.ws.pkgs[index];
    let mut text = String::new();
    text.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    text.push_str(&format!(
        "<Project DefaultTargets=\"Build\" xmlns=\"{MSBUILD_NAMESPACE}\">\n"
    ));
    text.push_str("  <ItemGroup Label=\"ProjectConfigurations\">\n");
    for config in configs(ctx.archs) {
        text.push_str(&format!(
            "    <ProjectConfiguration Include=\"{}\">\n",
            config.key()
        ));
        text.push_str(&format!(
            "      <Configuration>{}</Configuration>\n",
            config.profile
        ));
        text.push_str(&format!("      <Platform>{}</Platform>\n", config.platform));
        text.push_str("    </ProjectConfiguration>\n");
    }
    text.push_str("  </ItemGroup>\n");
    text.push_str("  <PropertyGroup Label=\"Globals\">\n");
    text.push_str(&property("ProjectGuid", &guid(&pkg.name)));
    text.push_str(&property("Keyword", "MakeFileProj"));
    text.push_str(&property("ProjectName", &pkg.name));
    text.push_str("  </PropertyGroup>\n");
    text.push_str("  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.Default.props\" />\n");
    for config in configs(ctx.archs) {
        text.push_str(&format!(
            "  <PropertyGroup Condition=\"{}\" Label=\"Configuration\">\n",
            xml(&condition(&config))
        ));
        text.push_str(&property("ConfigurationType", "Makefile"));
        text.push_str(&property(
            "UseDebugLibraries",
            if config.release { "false" } else { "true" },
        ));
        text.push_str(&property("PlatformToolset", "$(DefaultPlatformToolset)"));
        text.push_str("  </PropertyGroup>\n");
    }
    text.push_str("  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.props\" />\n");
    for config in configs(ctx.archs) {
        text.push_str(&format!(
            "  <PropertyGroup Condition=\"{}\">\n",
            xml(&condition(&config))
        ));
        text.push_str(&config_properties(ctx, index, &config));
        text.push_str("  </PropertyGroup>\n");
    }
    for tag in ["ClCompile", "ClInclude", "None"] {
        text.push_str("  <ItemGroup>\n");
        for item in items.iter().filter(|item| item.tag == tag) {
            text.push_str(&format!(
                "    <{tag} Include=\"{}\" />\n",
                xml(&item.path.to_string_lossy())
            ));
        }
        text.push_str("  </ItemGroup>\n");
    }
    text.push_str("  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.targets\" />\n");
    text.push_str("</Project>\n");
    text
}

fn config_properties(ctx: &Context, index: usize, config: &Config) -> String {
    let pkg = &ctx.ws.pkgs[index];
    let release = config.release;
    let out = out_dir(&ctx.root_dir, config.arch, release);
    let (includes, mut defines) = compile_scope(ctx.ws, index, &out.join("gen"));
    defines.push(if release { "NDEBUG" } else { "_DEBUG" }.to_string());
    defines.push("$(NMakePreprocessorDefinitions)".to_string());
    let mut include_list: Vec<String> = includes
        .iter()
        .map(|dir| dir.display().to_string())
        .collect();
    include_list.push("$(NMakeIncludeSearchPath)".to_string());
    let mut options = vec![format!("/std:c++{}", pkg.std)];
    if ctx.tc.msvc_style() {
        options.extend(pkg.flags.iter().cloned());
    }

    let root = ctx.root_dir.display();
    let nomake = ctx.nomake.display();
    let flag = if release { " -r" } else { "" };
    let arch = config.arch.name();
    let name = &pkg.name;
    let build = format!("cd /d \"{root}\" && \"{nomake}\" build {name} --arch {arch}{flag}");
    let clean = format!("cd /d \"{root}\" && \"{nomake}\" clean {name} --arch {arch}{flag}");
    let rebuild = format!("{clean} && \"{nomake}\" build {name} --arch {arch}{flag}");
    let output = out.join(
        ctx.tc
            .artifact(pkg.kind, &pkg.name)
            .expect("artifact name")
            .0,
    );

    let mut text = String::new();
    text.push_str(&property("NMakeBuildCommandLine", &build));
    text.push_str(&property("NMakeReBuildCommandLine", &rebuild));
    text.push_str(&property("NMakeCleanCommandLine", &clean));
    text.push_str(&property("NMakeOutput", &output.to_string_lossy()));
    text.push_str(&property(
        "NMakePreprocessorDefinitions",
        &defines.join(";"),
    ));
    text.push_str(&property("NMakeIncludeSearchPath", &include_list.join(";")));
    text.push_str(&property("AdditionalOptions", &options.join(" ")));
    text.push_str(&property(
        "LocalDebuggerWorkingDirectory",
        &pkg.root.to_string_lossy(),
    ));
    if pkg.kind == Kind::Test && !pkg.test_args.is_empty() {
        let joined: Vec<String> = pkg.test_args.iter().map(|a| quote_windows_arg(a)).collect();
        text.push_str(&property(
            "LocalDebuggerCommandArguments",
            &joined.join(" "),
        ));
    }
    text
}

fn filters_xml(pkg: &Pkg, items: &[Item]) -> String {
    let mut folders = BTreeSet::new();
    for filter in items.iter().filter_map(|item| item.filter.as_deref()) {
        let mut folder = String::new();
        for part in filter.split('\\') {
            if !folder.is_empty() {
                folder.push('\\');
            }
            folder.push_str(part);
            folders.insert(folder.clone());
        }
    }

    let mut text = String::new();
    text.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    text.push_str(&format!(
        "<Project ToolsVersion=\"4.0\" xmlns=\"{MSBUILD_NAMESPACE}\">\n"
    ));
    text.push_str("  <ItemGroup>\n");
    for folder in &folders {
        text.push_str(&format!("    <Filter Include=\"{}\">\n", xml(folder)));
        text.push_str(&format!(
            "      <UniqueIdentifier>{}</UniqueIdentifier>\n",
            guid(&format!("{}\\{folder}", pkg.name))
        ));
        text.push_str("    </Filter>\n");
    }
    text.push_str("  </ItemGroup>\n");
    for tag in ["ClCompile", "ClInclude", "None"] {
        text.push_str("  <ItemGroup>\n");
        for item in items.iter().filter(|item| item.tag == tag) {
            let path = xml(&item.path.to_string_lossy());
            match &item.filter {
                Some(filter) => {
                    text.push_str(&format!("    <{tag} Include=\"{path}\">\n"));
                    text.push_str(&format!("      <Filter>{}</Filter>\n", xml(filter)));
                    text.push_str(&format!("    </{tag}>\n"));
                }
                None => text.push_str(&format!("    <{tag} Include=\"{path}\" />\n")),
            }
        }
        text.push_str("  </ItemGroup>\n");
    }
    text.push_str("</Project>\n");
    text
}

/// Non-test member projects build with the solution; test suites and dependency projects still build individually on demand.
fn solution_text(projects: &[VsProject], archs: &[Arch]) -> String {
    let has_deps = projects.iter().any(|p| !p.is_member);
    let deps_folder = guid("nomake:deps");
    let mut text = String::new();
    text.push_str("Microsoft Visual Studio Solution File, Format Version 12.00\n");
    text.push_str("# Visual Studio Version 17\n");
    text.push_str("MinimumVisualStudioVersion = 10.0.40219.1\n");
    for project in projects {
        let name = &project.name;
        text.push_str(&format!(
            "Project(\"{CPP_PROJECT_TYPE}\") = \"{name}\", \"{name}.vcxproj\", \"{}\"\n",
            guid(name)
        ));
        if !project.deps.is_empty() {
            text.push_str("\tProjectSection(ProjectDependencies) = postProject\n");
            for dep in &project.deps {
                let dep_guid = guid(dep);
                text.push_str(&format!("\t\t{dep_guid} = {dep_guid}\n"));
            }
            text.push_str("\tEndProjectSection\n");
        }
        text.push_str("EndProject\n");
    }
    if has_deps {
        text.push_str(&format!(
            "Project(\"{SOLUTION_FOLDER_TYPE}\") = \"deps\", \"deps\", \"{deps_folder}\"\n"
        ));
        text.push_str("EndProject\n");
    }
    text.push_str("Global\n");
    text.push_str("\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\n");
    for config in configs(archs) {
        text.push_str(&format!("\t\t{} = {}\n", config.key(), config.key()));
    }
    text.push_str("\tEndGlobalSection\n");
    text.push_str("\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\n");
    for project in projects {
        let guid = guid(&project.name);
        for config in configs(archs) {
            text.push_str(&format!(
                "\t\t{guid}.{}.ActiveCfg = {}\n",
                config.key(),
                config.key()
            ));
            if project.builds {
                text.push_str(&format!(
                    "\t\t{guid}.{}.Build.0 = {}\n",
                    config.key(),
                    config.key()
                ));
            }
        }
    }
    text.push_str("\tEndGlobalSection\n");
    text.push_str("\tGlobalSection(SolutionProperties) = preSolution\n");
    text.push_str("\t\tHideSolutionNode = FALSE\n");
    text.push_str("\tEndGlobalSection\n");
    if has_deps {
        text.push_str("\tGlobalSection(NestedProjects) = preSolution\n");
        for project in projects.iter().filter(|p| !p.is_member) {
            text.push_str(&format!("\t\t{} = {deps_folder}\n", guid(&project.name)));
        }
        text.push_str("\tEndGlobalSection\n");
    }
    text.push_str("EndGlobal\n");
    text.replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::absolute;

    #[test]
    fn windows_arg_quoting_handles_spaces_quotes_and_trailing_slashes() {
        assert_eq!(quote_windows_arg("--verbose"), "--verbose");
        assert_eq!(quote_windows_arg(""), "\"\"");
        assert_eq!(quote_windows_arg("math suite"), "\"math suite\"");
        assert_eq!(quote_windows_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_windows_arg("C:\\dir a\\"), "\"C:\\dir a\\\\\"");
    }

    #[test]
    fn vs_solution_orders_startup_project_wires_deps_pregenerates_tests_h_and_per_pkg_cmds() {
        let root = std::env::temp_dir().join(format!("nomake_vs_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let ws_dir = absolute(&root.join("ws"));
        fs::create_dir_all(&ws_dir).unwrap();

        let write = |rel: &str, body: &str| {
            let path = ws_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        write(
            "nomake.toml",
            "[workspace]\nmembers = [\"suite\", \"mathlib\", \"app\", \"tool\"]\n",
        );
        write(
            "deps/lz4/nomake.toml",
            "[package]\nname = \"lz4\"\ntype = \"static\"\n",
        );
        write("deps/lz4/src/lz4.c", "int lz4(void) { return 4; }\n");
        write(
            "mathlib/nomake.toml",
            "[package]\nname = \"mathlib\"\ntype = \"static\"\n",
        );
        write("mathlib/src/m.cpp", "int m() { return 1; }\n");
        write(
            "app/nomake.toml",
            "[package]\nname = \"app\"\n[dependencies]\nmathlib = { path = \"../mathlib\" }\nlz4 = { path = \"../deps/lz4\" }\n",
        );
        write("app/src/main.cpp", "int main() { return 0; }\n");
        write("tool/nomake.toml", "[package]\nname = \"tool\"\n");
        write("tool/src/main.cpp", "int main() { return 0; }\n");
        write(
            "suite/nomake.toml",
            "[package]\nname = \"suite\"\ntype = \"test\"\n[test]\nargs = [\"--verbose\", \"--filter\", \"math suite\"]\n",
        );
        write(
            "suite/src/t.c",
            "#include <nomake/tests.h>\nTEST(ok) { TEST_CHECK(1); }\n",
        );

        let project = Project::discover(&ws_dir, Os::host()).unwrap();
        if let Err(err) = generate(&project, &[Arch::X64]) {
            if err.to_string().contains("no C++ compiler found") {
                let _ = fs::remove_dir_all(&root);
                return;
            }
            panic!("{err}");
        }

        // 1. `<nomake/tests.h>` is pre-generated for both Debug and Release on a clean tree.
        assert!(ws_dir
            .join("build/x64-debug/gen/suite/nomake/tests.h")
            .exists());
        assert!(ws_dir
            .join("build/x64-release/gen/suite/nomake/tests.h")
            .exists());

        // 2. Without `primary`, `app` (first non-test executable) precedes `tool`, `mathlib`, `suite`, and `lz4`.
        let sln = fs::read_to_string(ws_dir.join("build/vs/ws.sln")).unwrap();
        let pos = |name: &str| {
            sln.find(&format!("= \"{name}\", \"{name}.vcxproj\""))
                .unwrap()
        };
        assert!(pos("app") < pos("tool"), "{sln}");
        assert!(pos("tool") < pos("mathlib"), "{sln}");
        assert!(pos("mathlib") < pos("suite"), "{sln}");
        assert!(pos("suite") < pos("lz4"), "{sln}");

        // 3. `ProjectDependencies` on `app` lists both `mathlib` and `lz4`.
        let app_guid = guid("app");
        let math_guid = guid("mathlib");
        let lz4_guid = guid("lz4");
        let suite_guid = guid("suite");
        assert!(
            sln.contains(&format!("{math_guid} = {math_guid}"))
                && sln.contains(&format!("{lz4_guid} = {lz4_guid}")),
            "{sln}"
        );

        // 4. `.Build.0` is set ONLY on non-test workspace members (`app`, `tool`, `mathlib`), NOT `suite` or `lz4`.
        assert!(sln.contains(&format!("{app_guid}.Debug|x64.Build.0 = Debug|x64")));
        assert!(sln.contains(&format!("{math_guid}.Debug|x64.Build.0 = Debug|x64")));
        assert!(!sln.contains(&format!("{suite_guid}.Debug|x64.Build.0")));
        assert!(!sln.contains(&format!("{lz4_guid}.Debug|x64.Build.0")));

        // 5. `NestedProjects` puts `lz4` in `deps`, NOT `suite`.
        let deps_guid = guid("nomake:deps");
        assert!(sln.contains(&format!("{lz4_guid} = {deps_guid}")));
        assert!(!sln.contains(&format!("{suite_guid} = {deps_guid}")));

        // 6. `.vcxproj` files have package-dir debugger CWD, quoted test args, and per-package build/clean commands (including `deps`).
        let app_xml = fs::read_to_string(ws_dir.join("build/vs/app.vcxproj")).unwrap();
        assert!(app_xml.contains(&format!(
            "<LocalDebuggerWorkingDirectory>{}</LocalDebuggerWorkingDirectory>",
            xml(&ws_dir.join("app").to_string_lossy())
        )));
        assert!(!app_xml.contains("LocalDebuggerCommandArguments"));
        assert!(app_xml.contains("build app --arch x64</NMakeBuildCommandLine>"));
        assert!(app_xml.contains("clean app --arch x64</NMakeCleanCommandLine>"));

        let suite_xml = fs::read_to_string(ws_dir.join("build/vs/suite.vcxproj")).unwrap();
        assert!(suite_xml.contains(&format!(
            "<LocalDebuggerWorkingDirectory>{}</LocalDebuggerWorkingDirectory>",
            xml(&ws_dir.join("suite").to_string_lossy())
        )));
        assert!(suite_xml.contains(
            "<LocalDebuggerCommandArguments>--verbose --filter &quot;math suite&quot;</LocalDebuggerCommandArguments>"
        ));

        let lz4_xml = fs::read_to_string(ws_dir.join("build/vs/lz4.vcxproj")).unwrap();
        assert!(lz4_xml.contains("build lz4 --arch x64</NMakeBuildCommandLine>"));
        assert!(lz4_xml.contains("clean lz4 --arch x64</NMakeCleanCommandLine>"));

        // 7. Setting `primary = "tool"` promotes `tool` to the first project (VS Startup Project).
        write(
            "nomake.toml",
            "[workspace]\nmembers = [\"suite\", \"mathlib\", \"app\", \"tool\"]\nprimary = \"tool\"\n",
        );
        let project_primary = Project::discover(&ws_dir, Os::host()).unwrap();
        generate(&project_primary, &[Arch::X64]).unwrap();
        let sln_primary = fs::read_to_string(ws_dir.join("build/vs/ws.sln")).unwrap();
        let pos_p = |name: &str| {
            sln_primary
                .find(&format!("= \"{name}\", \"{name}.vcxproj\""))
                .unwrap()
        };
        assert!(pos_p("tool") < pos_p("app"), "{sln_primary}");

        fs::remove_dir_all(root).unwrap();
    }
}
