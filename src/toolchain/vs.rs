//! `nomake vs`: NMake-style Visual Studio solution in build/vs/. The IDE only browses and edits; builds call back into nomake.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::{Arch, Toolchain};
use crate::commands::out_dir;
use crate::config::Kind;
use crate::dag::compile_scope;
use crate::scanner::{self, is_header, Pkg, Workspace};
use crate::util::{absolute, hash, hash_seeded};
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
    /// Member projects name themselves on the command line so a build covers just them.
    workspace: bool,
}

struct Item {
    tag: &'static str,
    path: PathBuf,
    filter: Option<String>,
}

struct Project {
    name: String,
    /// Part of the solution build; packages that are only dependencies are browse-only.
    builds: bool,
    executable: bool,
}

pub fn generate(archs: &[Arch]) -> Result<()> {
    let root_dir = absolute(Path::new("."));
    generate_for(&root_dir, std::slice::from_ref(&root_dir), archs)
}

/// One solution in `root_dir` holding the packages in `member_dirs`, all building into its build/.
pub fn generate_for(root_dir: &Path, member_dirs: &[PathBuf], archs: &[Arch]) -> Result<()> {
    let vs_dir = root_dir.join("build").join("vs");
    fs::create_dir_all(&vs_dir)?;
    let ws = scanner::load_roots(root_dir, member_dirs)?;
    let tc = Toolchain::detect(&ws.root().toolchain, Arch::host(), &root_dir.join("build"))?;
    let workspace = !(member_dirs.len() == 1 && member_dirs[0] == root_dir);
    let ctx = Context {
        ws: &ws,
        tc: &tc,
        archs,
        root_dir: root_dir.to_path_buf(),
        nomake: std::env::current_exe()?,
        workspace,
    };

    let mut projects: Vec<Project> = Vec::new();
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
        projects.push(Project {
            name: pkg.name.clone(),
            builds: ws.roots.contains(&index),
            executable: pkg.kind == Kind::Executable,
        });
    }
    // Visual Studio starts the first project of the solution, so programs go first.
    projects.sort_by_key(|p| (!p.builds, !p.executable));
    let solution_name = if workspace {
        root_dir
            .file_name()
            .map_or_else(|| "nomake".to_string(), |n| n.to_string_lossy().into_owned())
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
    let member = if ctx.workspace && ctx.ws.roots.contains(&index) {
        format!(" {}", pkg.name)
    } else {
        String::new()
    };
    let build = format!("cd /d \"{root}\" && \"{nomake}\" build{member} --arch {arch}{flag}");
    let clean = format!("cd /d \"{root}\" && \"{nomake}\" clean --arch {arch}{flag}");
    let rebuild = format!("{clean} && \"{nomake}\" build{member} --arch {arch}{flag}");
    let output = out.join(ctx.tc.artifact(pkg.kind, &pkg.name).0);

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
        &root.to_string(),
    ));
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

/// Member projects build with the solution; each member's own `nomake build` already covers its dependencies.
fn solution_text(projects: &[Project], archs: &[Arch]) -> String {
    let has_deps = projects.iter().any(|p| !p.builds);
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
        for project in projects.iter().filter(|p| !p.builds) {
            text.push_str(&format!("\t\t{} = {deps_folder}\n", guid(&project.name)));
        }
        text.push_str("\tEndGlobalSection\n");
    }
    text.push_str("EndGlobal\n");
    text.replace('\n', "\r\n")
}
