use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use crate::config::{Crt, Generate, Kind};
use crate::scanner::{Pkg, Workspace};
use crate::toolchain::{CompileSpec, Family, LinkSpec, Pch, Toolchain};
use crate::util::{self, rel_str};
use crate::Result;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Generate,
    Compile,
    Archive,
    Link,
}

pub enum DepStyle {
    None,
    /// Makefile-style depfile written by `-MMD -MF`.
    Gcc(PathBuf),
    /// `/showIncludes` on stdout.
    Msvc,
}

pub struct Edge {
    pub node: usize,
    /// Must finish first, but does not by itself make this node dirty.
    pub order_only: bool,
}

/// What every compile in a package shares; the per-file command line is only assembled when the compile runs.
pub struct CompileCfg {
    tc: Arc<Toolchain>,
    std: String,
    includes: Vec<PathBuf>,
    defines: Vec<String>,
    flags: Vec<String>,
    release: bool,
    /// How the C runtime is linked.
    crt: Crt,
    /// (header name as included, precompiled output) that C++ sources use.
    pch: Option<(String, PathBuf)>,
    /// Hash of the command line with the per-file parts blanked out.
    hash: u64,
}

impl CompileCfg {
    fn spec<'a>(
        &'a self,
        src: &'a Path,
        obj: &'a Path,
        depfile: &'a Path,
        is_c: bool,
    ) -> CompileSpec<'a> {
        CompileSpec {
            src,
            obj,
            depfile,
            is_c,
            std: &self.std,
            includes: &self.includes,
            defines: &self.defines,
            flags: &self.flags,
            release: self.release,
            crt: self.crt,
            pch: match &self.pch {
                Some((name, file)) if !is_c => Pch::Use { name, file },
                _ => Pch::None,
            },
        }
    }
}

pub enum Args {
    Fixed(Vec<String>),
    Compile { cfg: Arc<CompileCfg>, is_c: bool },
}

pub struct Node {
    pub kind: NodeKind,
    pub desc: String,
    pub program: PathBuf,
    pub args: Args,
    pub cwd: PathBuf,
    pub inputs: Vec<PathBuf>,
    /// The first output is the key in the state file.
    pub outputs: Vec<PathBuf>,
    pub deps: Vec<Edge>,
    pub dep_style: DepStyle,
    pub rm_outputs: bool,
    /// Dependents compare the first output by content: rewriting identical bytes does not dirty them.
    pub cutoff: bool,
    pub hash: u64,
}

impl Node {
    pub fn args(&self) -> Cow<'_, [String]> {
        match &self.args {
            Args::Fixed(args) => Cow::Borrowed(args),
            Args::Compile { cfg, is_c } => {
                let depfile = match &self.dep_style {
                    DepStyle::Gcc(path) => path.as_path(),
                    _ => Path::new(""),
                };
                Cow::Owned(cfg.tc.compile_args(&cfg.spec(
                    &self.inputs[0],
                    &self.outputs[0],
                    depfile,
                    *is_c,
                )))
            }
        }
    }

    pub fn command_line(&self) -> String {
        let quote = |arg: &str| {
            if arg.contains(' ') {
                format!("\"{arg}\"")
            } else {
                arg.to_string()
            }
        };
        let mut parts = vec![quote(&self.program.to_string_lossy())];
        parts.extend(self.args().iter().map(|arg| quote(arg)));
        parts.join(" ")
    }
}

/// A header with the flags of the package that owns it; only the compile database uses it.
pub struct Header {
    pub path: PathBuf,
    cfg: Arc<CompileCfg>,
    obj: PathBuf,
}

impl Header {
    pub fn program(&self) -> &Path {
        &self.cfg.tc.cxx
    }

    pub fn hash(&self) -> u64 {
        self.cfg.hash
    }

    pub fn args(&self) -> Vec<String> {
        let depfile = self.obj.with_extension("d");
        let mut args = self
            .cfg
            .tc
            .compile_args(&self.cfg.spec(&self.path, &self.obj, &depfile, false));
        // The extension alone would make `.h` a C header.
        let lang: &[&str] = if self.cfg.tc.msvc_style() {
            &["/TP"]
        } else {
            &["-x", "c++-header"]
        };
        args.splice(0..0, lang.iter().map(|arg| arg.to_string()));
        args
    }
}

/// Nodes are ordered so every edge points to a lower index.
pub struct Graph {
    pub nodes: Vec<Node>,
    pub headers: Vec<Header>,
    pub dependents: Vec<Vec<usize>>,
    pub env: Vec<(String, String)>,
    pub sys_includes: Vec<String>,
    pub root_dir: PathBuf,
    pub target: PathBuf,
    /// Each root package's folder and the file it builds, in the order the roots were given.
    pub targets: Vec<(PathBuf, PathBuf)>,
}

struct Built {
    gens: Vec<usize>,
    /// (producing node, path to hand to the linker)
    artifact: Option<(usize, PathBuf)>,
}

/// Packages linked into `pkg_idx`. A shared library carries its own dependencies, so the search stops there;
/// an executable dependency (a tool) is never linked.
fn closure(ws: &Workspace, pkg_idx: usize) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![pkg_idx];
    while let Some(current) = stack.pop() {
        for &dep in &ws.pkgs[current].deps {
            if ws.pkgs[dep].kind == Kind::Executable {
                continue;
            }
            if seen.insert(dep) && ws.pkgs[dep].kind != Kind::Shared {
                stack.push(dep);
            }
        }
    }
    by_name(ws, seen)
}

/// Packages whose headers and defines `pkg_idx` sees: its direct dependencies and peers plus whatever those re-export.
fn visible(ws: &Workspace, pkg_idx: usize) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<usize> = ws.pkgs[pkg_idx].deps.clone();
    stack.extend(&ws.pkgs[pkg_idx].peers);
    while let Some(current) = stack.pop() {
        if seen.insert(current) {
            stack.extend(&ws.pkgs[current].reexports);
        }
    }
    by_name(ws, seen)
}

fn by_name(ws: &Workspace, set: BTreeSet<usize>) -> Vec<usize> {
    let mut list: Vec<usize> = set.into_iter().collect();
    list.sort_by(|&a, &b| ws.pkgs[a].name.cmp(&ws.pkgs[b].name));
    list
}

fn hashed(mut node: Node) -> Node {
    node.hash = match &node.args {
        Args::Fixed(args) => {
            let mut cmd = node.program.to_string_lossy().into_owned();
            for arg in args {
                cmd.push('\0');
                cmd.push_str(arg);
            }
            util::hash(cmd.as_bytes())
        }
        Args::Compile { cfg, is_c } => util::hash_seeded(
            node.inputs[0].as_os_str().as_encoded_bytes(),
            cfg.hash + *is_c as u64,
        ),
    };
    node
}

fn push(nodes: &mut Vec<Node>, node: Node) -> usize {
    nodes.push(hashed(node));
    nodes.len() - 1
}

fn edges(
    real: impl IntoIterator<Item = usize>,
    order_only: impl IntoIterator<Item = usize>,
) -> Vec<Edge> {
    let mut list: Vec<Edge> = real
        .into_iter()
        .map(|node| Edge {
            node,
            order_only: false,
        })
        .chain(order_only.into_iter().map(|node| Edge {
            node,
            order_only: true,
        }))
        .collect();
    list.sort_by_key(|edge| (edge.node, edge.order_only));
    list.dedup_by_key(|edge| edge.node);
    list
}

struct GenOutput {
    path: PathBuf,
    /// Relative to the package's gen dir; this is what later steps' input globs match.
    rel: String,
    node: usize,
}

/// Expands each `[[generate]]` step into one node per matched input. Steps may consume outputs of earlier steps.
fn generate_nodes(
    ws: &Workspace,
    pkg_idx: usize,
    built: &[Built],
    includes: &[PathBuf],
    out_dir: &Path,
    nodes: &mut Vec<Node>,
) -> Result<Vec<GenOutput>> {
    let pkg = &ws.pkgs[pkg_idx];
    let gen_dir = out_dir.join("gen").join(&pkg.name);
    let mut produced: Vec<GenOutput> = Vec::new();
    for step in &pkg.generate {
        let tool = match &step.tool {
            Some(name) => {
                let found = pkg
                    .deps
                    .iter()
                    .find(|&&dep| {
                        ws.pkgs[dep].name == *name && ws.pkgs[dep].kind == Kind::Executable
                    })
                    .and_then(|&dep| built[dep].artifact.clone());
                match found {
                    Some(artifact) => Some(artifact),
                    None => bail!(
                        "[[generate]] in '{}': tool '{name}' must be an executable in [dependencies]",
                        pkg.name
                    ),
                }
            }
            None => None,
        };
        let mut inputs: Vec<(Option<PathBuf>, Option<usize>)> = Vec::new();
        if step.inputs.is_empty() {
            inputs.push((None, None));
        }
        for pattern in &step.inputs {
            inputs.extend(
                util::glob(&pkg.root, pattern)
                    .into_iter()
                    .map(|path| (Some(path), None)),
            );
            inputs.extend(
                produced
                    .iter()
                    .filter(|output| util::matches(pattern, &output.rel))
                    .map(|output| (Some(output.path.clone()), Some(output.node))),
            );
        }
        if inputs.is_empty() {
            bail!(
                "[[generate]] '{}' in '{}' matched no inputs",
                step.label(),
                pkg.name
            );
        }
        inputs.sort();
        inputs.dedup();

        // A batch step runs once for all matched inputs; {input} is the first.
        let runs: Vec<Vec<(Option<PathBuf>, Option<usize>)>> = if step.batch {
            vec![inputs]
        } else {
            inputs.into_iter().map(|input| vec![input]).collect()
        };

        let mut step_outputs = Vec::new();
        for run in runs {
            let node = step_node(pkg, step, &gen_dir, run, tool.as_ref(), includes);
            for out in &node.outputs {
                step_outputs.push((out.clone(), rel_str(&gen_dir, out)));
            }
            let idx = push(nodes, node);
            for (path, rel) in step_outputs.drain(..) {
                produced.push(GenOutput {
                    path,
                    rel,
                    node: idx,
                });
            }
        }
    }
    Ok(produced)
}

fn step_node(
    pkg: &Pkg,
    step: &Generate,
    gen_dir: &Path,
    run: Vec<(Option<PathBuf>, Option<usize>)>,
    tool: Option<&(usize, PathBuf)>,
    includes: &[PathBuf],
) -> Node {
    let input = run.first().and_then(|(path, _)| path.clone());
    let mut producers: Vec<usize> = run.iter().filter_map(|(_, producer)| *producer).collect();
    let mut all_inputs: Vec<PathBuf> = run.into_iter().filter_map(|(path, _)| path).collect();
    let stem = match &input {
        Some(path) => path.file_stem().unwrap().to_string_lossy().into_owned(),
        None => step.label().to_string(),
    };
    let outputs: Vec<PathBuf> = step
        .outputs
        .iter()
        .map(|output| util::absolute(&gen_dir.join(output.replace("{stem}", &stem))))
        .collect();
    let input_str = input
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let depfile = gen_dir.join(format!("{stem}.{}.d", step.label()));
    let subst = |arg: &String| {
        arg.replace("{input}", &input_str)
            .replace("{output}", &outputs[0].to_string_lossy())
            .replace("{out_dir}", &gen_dir.to_string_lossy())
            .replace("{stem}", &stem)
            .replace("{depfile}", &depfile.to_string_lossy())
    };
    let (program, args, dep_style) = match tool {
        Some((node, path)) => {
            producers.push(*node);
            all_inputs.push(path.clone());
            let mut args = vec![
                "--out-dir".to_string(),
                gen_dir.to_string_lossy().into_owned(),
                "--depfile".to_string(),
                depfile.to_string_lossy().into_owned(),
            ];
            args.extend(includes.iter().map(|dir| format!("-I{}", dir.display())));
            args.push(input_str.clone());
            (path.clone(), args, DepStyle::Gcc(depfile))
        }
        None => {
            let uses_depfile = step.command.iter().any(|arg| arg.contains("{depfile}"));
            (
                PathBuf::from(&step.command[0]),
                step.command[1..].iter().map(subst).collect(),
                if uses_depfile {
                    DepStyle::Gcc(depfile)
                } else {
                    DepStyle::None
                },
            )
        }
    };
    Node {
        kind: NodeKind::Generate,
        desc: format!(
            "GEN {} {}",
            step.label(),
            input
                .as_ref()
                .map_or(String::new(), |path| rel_str(&pkg.root, path))
        ),
        program,
        args: Args::Fixed(args),
        cwd: pkg.root.clone(),
        inputs: all_inputs,
        outputs,
        deps: edges(producers, []),
        dep_style,
        rm_outputs: false,
        cutoff: true,
        hash: 0,
    }
}

/// Include directories and defines every compile of package `pkg_idx` sees; `gen_root` holds each package's generated files.
pub fn compile_scope(
    ws: &Workspace,
    pkg_idx: usize,
    gen_root: &Path,
) -> (Vec<PathBuf>, Vec<String>) {
    let gen_dir = |pkg: &Pkg| (!pkg.generate.is_empty()).then(|| gen_root.join(&pkg.name));
    let pkg = &ws.pkgs[pkg_idx];
    let mut includes: Vec<PathBuf> = pkg
        .include_private
        .iter()
        .chain(&pkg.include_public)
        .cloned()
        .collect();
    includes.extend(gen_dir(pkg));
    let mut defines: Vec<String> = pkg
        .defines
        .iter()
        .chain(&pkg.public_defines)
        .cloned()
        .collect();
    for dep in visible(ws, pkg_idx) {
        includes.extend(ws.pkgs[dep].include_public.iter().cloned());
        includes.extend(gen_dir(&ws.pkgs[dep]));
        defines.extend(ws.pkgs[dep].public_defines.iter().cloned());
    }
    (includes, defines)
}

pub fn build(
    ws: &Workspace,
    tc: &Toolchain,
    release: bool,
    root_dir: &Path,
    out_dir: &Path,
) -> Result<Graph> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut headers: Vec<Header> = Vec::new();
    let mut seen_headers: BTreeSet<PathBuf> = BTreeSet::new();
    let mut built: Vec<Built> = Vec::new();
    let tc_arc = Arc::new(tc.clone());
    let obj_ext = if tc.msvc_style() { "obj" } else { "o" };
    let crt = ws.crt()?;

    for (pkg_idx, pkg) in ws.pkgs.iter().enumerate() {
        let deps = closure(ws, pkg_idx);
        let gen_dir = out_dir.join("gen").join(&pkg.name);
        let (includes, defines) = compile_scope(ws, pkg_idx, &out_dir.join("gen"));

        let produced = generate_nodes(ws, pkg_idx, &built, &includes, out_dir, &mut nodes)?;
        let mut gens: Vec<usize> = produced.iter().map(|output| output.node).collect();
        gens.dedup();

        // Everything a compile can see: own gens and the gens of every visible dependency (their headers are exported).
        let order_only: Vec<usize> = gens
            .iter()
            .copied()
            .chain(
                visible(ws, pkg_idx)
                    .iter()
                    .flat_map(|&dep| built[dep].gens.clone()),
            )
            .collect();

        let mut sources: Vec<(PathBuf, Option<usize>)> = pkg
            .sources
            .iter()
            .map(|path| (path.clone(), None))
            .collect();
        for output in &produced {
            if crate::scanner::is_source(&output.path)
                && !sources.iter().any(|(path, _)| path == &output.path)
            {
                sources.push((output.path.clone(), Some(output.node)));
            }
        }

        let mut includes = includes;
        let pch = match &pkg.pch {
            Some(header) => {
                if tc.family == Family::Gcc {
                    bail!("package '{}': pch is not supported with g++ yet", pkg.name);
                }
                if !header.is_file() {
                    bail!(
                        "package '{}': pch header {} not found",
                        pkg.name,
                        header.display()
                    );
                }
                let name = header.file_name().unwrap().to_string_lossy().into_owned();
                let file = out_dir
                    .join("obj")
                    .join(&pkg.name)
                    .join(format!("{name}.pch"));
                if tc.msvc_style() {
                    // /Yc and /Yu match the header by the name written in the #include, so it is found through -I.
                    includes.push(header.parent().unwrap().to_path_buf());
                }
                Some((name, file))
            }
            None => None,
        };

        let mut cfg = CompileCfg {
            tc: tc_arc.clone(),
            std: pkg.std.clone(),
            includes,
            defines,
            flags: pkg.flags.clone(),
            release,
            crt,
            pch: pch.clone(),
            hash: 0,
        };
        let probe = tc.compile_args(&cfg.spec(Path::new(""), Path::new(""), Path::new(""), false));
        cfg.hash = util::hash(format!("{}\0{}", tc.cxx.display(), probe.join("\0")).as_bytes());
        let cfg = Arc::new(cfg);

        let mut found = BTreeSet::new();
        for dir in pkg.include_public.iter().chain(&pkg.include_private) {
            crate::scanner::collect_headers(dir, true, &mut found);
        }
        for dir in sources.iter().filter_map(|(src, _)| src.parent()) {
            crate::scanner::collect_headers(dir, false, &mut found);
        }
        found.extend(
            produced
                .iter()
                .filter(|output| crate::scanner::is_header(&output.path))
                .map(|output| output.path.clone()),
        );
        for path in found {
            if seen_headers.insert(path.clone()) {
                let name = path.file_name().unwrap().to_string_lossy();
                let obj = out_dir
                    .join("obj")
                    .join(&pkg.name)
                    .join(format!("{name}.{obj_ext}"));
                headers.push(Header {
                    path,
                    cfg: cfg.clone(),
                    obj,
                });
            }
        }

        let mut pch_node = None;
        let mut pch_obj = None;
        if let (Some(header), Some((name, file))) = (&pkg.pch, &pch) {
            let pch_dir = file.parent().unwrap();
            let (src, obj, outputs, dep_style) = if tc.msvc_style() {
                // cl can only create a PCH while compiling a source file, so a stub includes the header.
                let stub = pch_dir.join(format!("{name}.cpp"));
                let text = format!("#include \"{name}\"\n");
                if std::fs::read_to_string(&stub).map_or(true, |old| old != text) {
                    std::fs::create_dir_all(pch_dir)?;
                    std::fs::write(&stub, text)?;
                }
                let obj = pch_dir.join(format!("{name}.obj"));
                (stub, obj.clone(), vec![file.clone(), obj], DepStyle::Msvc)
            } else {
                (
                    header.clone(),
                    file.clone(),
                    vec![file.clone()],
                    DepStyle::Gcc(file.with_extension("pch.d")),
                )
            };
            let depfile = file.with_extension("pch.d");
            let mut spec = cfg.spec(&src, &obj, &depfile, false);
            spec.pch = Pch::Build { name, file };
            let node = Node {
                kind: NodeKind::Compile,
                desc: format!("PCH {}", rel_str(root_dir, header)),
                program: tc.cxx.clone(),
                args: Args::Fixed(tc.compile_args(&spec)),
                cwd: root_dir.to_path_buf(),
                inputs: vec![src],
                outputs,
                deps: edges([], order_only.iter().copied()),
                dep_style,
                rm_outputs: false,
                cutoff: true,
                hash: 0,
            };
            pch_node = Some(push(&mut nodes, node));
            // With /Z7 the PCH's debug types live in its object, so it is linked like any other.
            pch_obj = tc.msvc_style().then_some(obj);
        }

        let compiled: Vec<(Node, PathBuf)> = sources
            .par_iter()
            .map(|(src, producer)| {
                let rel = match producer {
                    Some(_) => format!("gen/{}", rel_str(&gen_dir, src)),
                    None => rel_str(&pkg.root, src),
                };
                let mut obj = out_dir.join("obj").join(&pkg.name);
                obj.extend(rel.split('/'));
                obj.set_extension(format!(
                    "{}.{obj_ext}",
                    src.extension().unwrap().to_string_lossy()
                ));
                let depfile = obj.with_extension(format!("{obj_ext}.d"));
                let is_c = src
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("c"));
                let node = Node {
                    kind: NodeKind::Compile,
                    desc: format!(
                        "{} {}",
                        if is_c { "CC" } else { "CXX" },
                        rel_str(root_dir, src)
                    ),
                    program: tc.cxx.clone(),
                    args: Args::Compile {
                        cfg: cfg.clone(),
                        is_c,
                    },
                    cwd: root_dir.to_path_buf(),
                    inputs: vec![src.clone()],
                    outputs: vec![obj.clone()],
                    deps: edges(
                        producer.iter().copied().chain(pch_node.filter(|_| !is_c)),
                        order_only
                            .iter()
                            .copied()
                            .filter(|gen| Some(*gen) != *producer),
                    ),
                    dep_style: if tc.msvc_style() {
                        DepStyle::Msvc
                    } else {
                        DepStyle::Gcc(depfile)
                    },
                    rm_outputs: false,
                    cutoff: true,
                    hash: 0,
                };
                (hashed(node), obj)
            })
            .collect();
        let mut objs = Vec::new();
        let mut obj_nodes = Vec::new();
        if let (Some(node), Some(obj)) = (pch_node, pch_obj) {
            obj_nodes.push(node);
            objs.push(obj);
        }
        for (node, obj) in compiled {
            nodes.push(node);
            obj_nodes.push(nodes.len() - 1);
            objs.push(obj);
        }

        let mut artifact = None;
        if pkg.kind == Kind::Executable && objs.is_empty() {
            bail!("package '{}' has no source files", pkg.name);
        }
        if !objs.is_empty() {
            let (file, link_file) = tc
                .artifact(pkg.kind, &pkg.name)
                .map_err(|e| format!("package '{}': {e}", pkg.name))?;
            let out = out_dir.join(file);
            let node = if pkg.kind == Kind::Static {
                Node {
                    kind: NodeKind::Archive,
                    desc: format!("AR {}", pkg.name),
                    program: tc.ar.clone(),
                    args: Args::Fixed(tc.archive_args(&out, &objs)),
                    cwd: root_dir.to_path_buf(),
                    inputs: objs.clone(),
                    outputs: vec![out.clone()],
                    deps: edges(obj_nodes.iter().copied(), []),
                    dep_style: DepStyle::None,
                    cutoff: true,
                    rm_outputs: true,
                    hash: 0,
                }
            } else {
                // Dependents before dependencies so static archives resolve left to right.
                let linked: Vec<usize> = deps
                    .iter()
                    .rev()
                    .copied()
                    .filter(|&dep| built[dep].artifact.is_some())
                    .collect();
                let lib_args: Vec<PathBuf> = linked
                    .iter()
                    .map(|&dep| built[dep].artifact.as_ref().unwrap().1.clone())
                    .collect();
                let mut inputs = objs.clone();
                inputs.extend(
                    linked
                        .iter()
                        .map(|&dep| built[dep].artifact.as_ref().unwrap().1.clone()),
                );
                let mut sys_libs = pkg.libs.clone();
                let mut flags = pkg.link_flags.clone();
                for &dep in &deps {
                    sys_libs.extend(ws.pkgs[dep].libs.iter().cloned());
                    flags.extend(ws.pkgs[dep].link_flags.iter().cloned());
                }
                inputs.extend(tc.link_inputs(&flags, root_dir));
                // A separate import library is what dependents consume, and it only changes when the exports do.
                let implib = pkg.kind == Kind::Shared && tc.linker.is_some();
                let mut outputs = if implib {
                    vec![out_dir.join(&link_file), out.clone()]
                } else {
                    vec![out.clone()]
                };
                outputs.extend(
                    tc.side_outputs(&pkg.name, &flags)
                        .into_iter()
                        .map(|file| out_dir.join(file)),
                );
                Node {
                    kind: NodeKind::Link,
                    desc: format!("LINK {}", pkg.name),
                    program: tc.link_program().to_path_buf(),
                    args: Args::Fixed(tc.link_args(&LinkSpec {
                        kind: pkg.kind,
                        out: &out,
                        objs: &objs,
                        libs: &lib_args,
                        sys_libs: &sys_libs,
                        flags: &flags,
                        release,
                        crt,
                        subsystem: pkg.subsystem.as_deref(),
                    })),
                    cwd: root_dir.to_path_buf(),
                    inputs,
                    outputs,
                    deps: edges(
                        obj_nodes.iter().copied().chain(
                            linked
                                .iter()
                                .map(|&dep| built[dep].artifact.as_ref().unwrap().0),
                        ),
                        [],
                    ),
                    dep_style: DepStyle::None,
                    rm_outputs: false,
                    cutoff: implib,
                    hash: 0,
                }
            };
            let idx = push(&mut nodes, node);
            artifact = Some((idx, out_dir.join(link_file)));
        }

        built.push(Built { gens, artifact });
    }

    let root = ws.root();
    let target = out_dir.join(tc.artifact(root.kind, &root.name)?.0);
    let targets = ws
        .roots
        .iter()
        .map(|&idx| {
            let pkg = &ws.pkgs[idx];
            Ok((
                pkg.root.clone(),
                out_dir.join(tc.artifact(pkg.kind, &pkg.name)?.0),
            ))
        })
        .collect::<Result<_>>()?;

    let mut dependents = vec![Vec::new(); nodes.len()];
    for (idx, node) in nodes.iter().enumerate() {
        for edge in &node.deps {
            debug_assert!(edge.node < idx);
            dependents[edge.node].push(idx);
        }
    }
    Ok(Graph {
        nodes,
        headers,
        dependents,
        env: tc.env.clone(),
        sys_includes: tc.system_includes(),
        root_dir: root_dir.to_path_buf(),
        target,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Os;
    use std::fs;

    fn lib(root: &Path, name: &str, deps: &str) {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("include")).unwrap();
        let kind = if name == "app" {
            "executable"
        } else {
            "static"
        };
        let manifest = format!("[package]\nname = \"{name}\"\ntype = \"{kind}\"\n[include]\npublic = [\"include\"]\n[dependencies]\n{deps}");
        fs::write(dir.join("nomake.toml"), manifest).unwrap();
    }

    #[test]
    fn only_direct_and_reexported_dependencies_are_visible() {
        let root = std::env::temp_dir().join(format!("nomake_scope_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        lib(&root, "hidden", "");
        lib(&root, "shown", "");
        lib(
            &root,
            "mid",
            "hidden = { path = \"../hidden\" }\nshown = { path = \"../shown\", public = true }",
        );
        lib(&root, "app", "mid = { path = \"../mid\" }");
        let ws = crate::scanner::load(&root.join("app"), Os::host()).unwrap();
        let app = ws.pkgs.len() - 1;
        let (includes, _) = compile_scope(&ws, app, &root.join("gen"));
        let has = |name: &str| {
            includes
                .iter()
                .any(|path| path.ends_with(Path::new(name).join("include")))
        };
        assert!(has("mid") && has("shown") && !has("hidden"));
        assert_eq!(
            closure(&ws, app).len(),
            3,
            "linking still sees every dependency"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scope_does_not_depend_on_which_roots_were_loaded() {
        let root = std::env::temp_dir().join(format!("nomake_order_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        lib(&root, "alpha", "");
        lib(&root, "zed", "");
        lib(
            &root,
            "app",
            "alpha = { path = \"../alpha\" }\nzed = { path = \"../zed\" }",
        );
        let scope = |ws: &Workspace| {
            let app = ws.pkgs.iter().position(|pkg| pkg.name == "app").unwrap();
            let (includes, _) = compile_scope(ws, app, &root.join("gen"));
            let linked: Vec<&str> = closure(ws, app)
                .into_iter()
                .map(|dep| ws.pkgs[dep].name.as_str())
                .collect();
            (includes, linked.join(" "))
        };
        let alone = crate::scanner::load(&root.join("app"), Os::host()).unwrap();
        // Loading zed first gives it the lower package index.
        let with_zed_first =
            crate::scanner::load_roots(&root, &[root.join("zed"), root.join("app")], Os::host())
                .unwrap();
        assert_eq!(scope(&alone), scope(&with_zed_first));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tool_steps_run_the_built_tool_per_input_and_are_not_linked() {
        let root = std::env::temp_dir().join(format!("nomake_tool_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["tool", "app/data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("tool/nomake.toml"),
            "[package]\nname = \"tool\"\ntype = \"executable\"\n[sources]\nfiles = [\"main.cpp\"]\n",
        )
        .unwrap();
        fs::write(root.join("tool/main.cpp"), "").unwrap();
        fs::write(
            root.join("app/nomake.toml"),
            "[package]\nname = \"app\"\ntype = \"executable\"\n[sources]\nfiles = [\"main.cpp\"]\n\
             [dependencies]\ntool = { path = \"../tool\" }\n\
             [[generate]]\ntool = \"tool\"\ninputs = [\"data/*.x\"]\noutputs = [\"{stem}.gen.cpp\"]\n",
        )
        .unwrap();
        fs::write(root.join("app/main.cpp"), "").unwrap();
        fs::write(root.join("app/data/a.x"), "").unwrap();
        fs::write(root.join("app/data/b.x"), "").unwrap();

        let ws = crate::scanner::load(&root.join("app"), Os::host()).unwrap();
        let clang = Toolchain::for_tests(Family::Clang);
        let graph = build(&ws, &clang, false, &root, &root.join("build")).unwrap();

        let gens: Vec<&Node> = graph
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Generate)
            .collect();
        assert_eq!(gens.len(), 2, "one run per matched input");
        let tool_link = graph
            .nodes
            .iter()
            .position(|node| node.desc == "LINK tool")
            .unwrap();
        for node in &gens {
            let args = node.args().join(" ");
            assert!(args.contains("--out-dir") && args.contains("--depfile"));
            assert!(node.deps.iter().any(|edge| edge.node == tool_link));
            assert!(matches!(node.dep_style, DepStyle::Gcc(_)));
        }
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.desc.starts_with("CXX") && node.desc.ends_with("b.gen.cpp")));
        let link = graph
            .nodes
            .iter()
            .find(|node| node.desc == "LINK app")
            .unwrap();
        assert!(link
            .inputs
            .iter()
            .all(|input| !input.to_string_lossy().contains("tool.exe")
                && input.file_stem().unwrap() != "tool"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pch_is_built_first_and_only_used_by_cpp_sources() {
        let root = std::env::temp_dir().join(format!("nomake_pch_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("nomake.toml"),
            "[package]\nname = \"app\"\ntype = \"executable\"\n[build]\npch = \"src/pch.h\"\n",
        )
        .unwrap();
        fs::write(root.join("src/main.cpp"), "").unwrap();
        fs::write(root.join("src/util.c"), "").unwrap();
        let ws = crate::scanner::load(&root, Os::host()).unwrap();
        let out = root.join("build");

        // A missing header is reported instead of failing inside the compiler.
        let clang = Toolchain::for_tests(Family::Clang);
        assert!(build(&ws, &clang, false, &root, &out).is_err());
        fs::write(root.join("src/pch.h"), "").unwrap();

        let graph = build(&ws, &clang, false, &root, &out).unwrap();
        let node = |desc: &str| {
            graph
                .nodes
                .iter()
                .position(|node| node.desc.ends_with(desc))
                .unwrap()
        };
        let pch = node("pch.h");
        assert!(
            graph.nodes[pch].desc.starts_with("PCH")
                && graph.nodes[pch].args().join(" ").contains("-x c++-header")
        );

        let cpp = &graph.nodes[node("main.cpp")];
        assert!(cpp.args().join(" ").contains("-include-pch"));
        assert!(
            cpp.deps
                .iter()
                .any(|edge| edge.node == pch && !edge.order_only),
            "a rebuilt pch must rebuild the sources"
        );

        let c = &graph.nodes[node("util.c")];
        assert!(
            !c.args().join(" ").contains("-include-pch")
                && c.deps.iter().all(|edge| edge.node != pch)
        );

        let gcc = Toolchain::for_tests(Family::Gcc);
        assert!(build(&ws, &gcc, false, &root, &out).is_err());

        // MSVC-style compilers precompile through a stub source and link its object.
        let msvc = Toolchain::for_tests(Family::Msvc);
        let graph = build(&ws, &msvc, false, &root, &out).unwrap();
        let pch = graph
            .nodes
            .iter()
            .position(|node| node.desc.starts_with("PCH"))
            .unwrap();
        assert!(graph.nodes[pch].args().join(" ").contains("/Ycpch.h"));
        assert!(graph.nodes[pch].outputs[1]
            .to_string_lossy()
            .ends_with("pch.h.obj"));
        let cpp = graph
            .nodes
            .iter()
            .find(|node| node.desc.ends_with("main.cpp"))
            .unwrap();
        let args = cpp.args().join(" ");
        assert!(args.contains("/Yupch.h") && args.contains("/FIpch.h"));
        let link = graph
            .nodes
            .iter()
            .find(|node| node.desc.starts_with("LINK"))
            .unwrap();
        assert!(link
            .inputs
            .iter()
            .any(|input| input.to_string_lossy().ends_with("pch.h.obj")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn emscripten_graph_tracks_side_outputs_and_rejects_shared_libs() {
        let root = std::env::temp_dir().join(format!("nomake_wasm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let app = root.join("app");
        fs::create_dir_all(app.join("src")).unwrap();
        fs::write(app.join("src/main.cpp"), "int main() { return 0; }").unwrap();
        fs::write(
            app.join("nomake.toml"),
            "[package]\nname = \"app\"\ntype = \"executable\"\n\
             [target.emscripten.build]\nlink_flags = [\"-O3\", \"--preload-file\", \"assets\"]\n",
        )
        .unwrap();
        let ws = crate::scanner::load(&app, Os::Emscripten).unwrap();
        let tc = Toolchain::for_tests(Family::Emscripten);
        let graph = build(&ws, &tc, true, &app, &root.join("out")).unwrap();

        let link = graph
            .nodes
            .iter()
            .find(|n| n.desc.starts_with("LINK"))
            .unwrap();
        let names: Vec<_> = link
            .outputs
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["app.html", "app.js", "app.wasm", "app.data"]);
        let link_args = link.args();
        assert!(
            link_args.contains(&"-O3".to_string()) && link_args.contains(&"assets".to_string())
        );

        fs::write(
            app.join("nomake.toml"),
            "[package]\nname = \"app\"\ntype = \"shared\"\n",
        )
        .unwrap();
        let ws = crate::scanner::load(&app, Os::Emscripten).unwrap();
        let error = build(&ws, &tc, false, &app, &root.join("out"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("cannot build shared libraries"));
        fs::remove_dir_all(root).unwrap();
    }
}
