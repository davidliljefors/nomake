use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::dag::{DepStyle, Graph, Node, NodeKind};
use crate::state::{Content, State};
use crate::util::{self, Map};
use crate::Result;

pub struct Summary {
    pub ok: bool,
    pub ran: usize,
}

fn nanos(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
}

/// Nanoseconds since the epoch; 0 means the file does not exist.
fn stat(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| nanos(&metadata))
}

/// Directories holding at least this many wanted files are listed instead of stat'ing each file. A listing carries
/// every entry's mtime without opening the files, which on Windows is much cheaper than a stat even for a few files.
const LIST_MIN: usize = if cfg!(windows) { 1 } else { 16 };

fn fold(name: &str) -> Cow<'_, str> {
    if cfg!(windows) {
        Cow::Owned(name.to_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

/// mtimes by path id, read once up front; a node refreshes its outputs' entries when it finishes.
struct Stats {
    paths: Vec<String>,
    mtimes: Vec<AtomicU64>,
    /// When a product's content last changed; 0 means "same as its mtime".
    stamps: Vec<AtomicU64>,
}

impl Stats {
    /// A directory listing carries every entry's mtime, which is far cheaper than one stat per file.
    fn scan(paths: Vec<String>, contents: Vec<(u32, Content)>) -> Stats {
        let mtimes: Vec<AtomicU64> = (0..paths.len()).map(|_| AtomicU64::new(0)).collect();
        let mut by_dir: Map<&str, Vec<(usize, &str)>> = Map::default();
        for (id, path) in paths.iter().enumerate() {
            let (dir, name) = path.rsplit_once(['\\', '/']).unwrap_or((".", path));
            by_dir.entry(dir).or_default().push((id, name));
        }
        let dirs: Vec<(&str, Vec<(usize, &str)>)> = by_dir.into_iter().collect();
        dirs.par_iter().for_each(|(dir, files)| {
            if files.len() < LIST_MIN {
                for &(id, _) in files {
                    mtimes[id].store(stat(Path::new(&paths[id])), SeqCst);
                }
                return;
            }
            let mut wanted: Map<Cow<str>, Vec<usize>> = Map::default();
            for &(id, name) in files {
                wanted.entry(fold(name)).or_default().push(id);
            }
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                if let Some(ids) = wanted.get(&*fold(&entry.file_name().to_string_lossy())) {
                    let mtime = match entry.file_type() {
                        // A listing reports a link's own time; builds care about its target's.
                        Ok(kind) if kind.is_symlink() => stat(&entry.path()),
                        _ => entry.metadata().map_or(0, |metadata| nanos(&metadata)),
                    };
                    for &id in ids {
                        mtimes[id].store(mtime, SeqCst);
                    }
                }
            }
        });
        drop(dirs);
        let stamps: Vec<AtomicU64> = (0..paths.len()).map(|_| AtomicU64::new(0)).collect();
        for (id, content) in contents {
            if mtimes[id as usize].load(SeqCst) == content.mtime {
                stamps[id as usize].store(content.stamp, SeqCst);
            }
        }
        Stats {
            paths,
            mtimes,
            stamps,
        }
    }

    fn mtime(&self, id: u32) -> u64 {
        self.mtimes[id as usize].load(SeqCst)
    }

    fn stamp(&self, id: u32) -> u64 {
        match self.stamps[id as usize].load(SeqCst) {
            0 => self.mtime(id),
            stamp => stamp,
        }
    }
}

struct Ctx<'a> {
    graph: &'a Graph,
    state: &'a State,
    stats: Stats,
    ins: Vec<Vec<u32>>,
    outs: Vec<Vec<u32>>,
    est: Vec<bool>,
    remaining: Vec<AtomicUsize>,
    rebuilt: Vec<AtomicBool>,
    failed: AtomicBool,
    started: AtomicUsize,
    total: AtomicUsize,
    print: Mutex<()>,
    verbose: bool,
}

/// Diagnostics behind `-d`.
#[derive(Clone, Copy, Default)]
pub struct Debug {
    /// Print why each step is out of date.
    pub explain: bool,
    /// Print how long each phase of the run took.
    pub stats: bool,
}

pub fn run(
    graph: &Graph,
    state: &State,
    jobs: usize,
    verbose: bool,
    debug: Debug,
) -> Result<Summary> {
    let Debug {
        explain,
        stats: show_stats,
    } = debug;
    let clock = std::time::Instant::now();
    let lap = |what: &str| {
        if show_stats {
            println!("nomake: stats: {what} at {:.1?}", clock.elapsed());
        }
    };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;

    let paths_of = |list: fn(&Node) -> &[PathBuf]| {
        let lists: Vec<&[PathBuf]> = graph.nodes.iter().map(list).collect();
        pool.install(|| state.intern_lists(&lists))
    };
    let ins = paths_of(|node| &node.inputs);
    let outs = paths_of(|node| &node.outputs);
    lap("paths interned");
    let stats = pool.install(|| Stats::scan(state.paths(), state.contents()));
    lap(&format!("{} files stat'ed", stats.paths.len()));
    let count = graph.nodes.len();
    let mut ctx = Ctx {
        graph,
        state,
        stats,
        ins,
        outs,
        est: vec![false; count],
        remaining: graph
            .nodes
            .iter()
            .map(|node| AtomicUsize::new(node.deps.len()))
            .collect(),
        rebuilt: (0..count).map(|_| AtomicBool::new(false)).collect(),
        failed: AtomicBool::new(false),
        started: AtomicUsize::new(0),
        total: AtomicUsize::new(0),
        print: Mutex::new(()),
        verbose,
    };

    // Estimate the work up front so progress reads [n/total]; discoveries made at run time bump the total.
    let mut est = vec![false; count];
    for idx in 0..count {
        let reason = ctx.stale_reason(idx);
        if explain {
            if let Some(reason) = &reason {
                println!(
                    "nomake: explain: {}: {reason}",
                    graph.nodes[idx].outputs[0].display()
                );
            }
        }
        est[idx] = reason.is_some()
            || graph.nodes[idx]
                .deps
                .iter()
                .any(|edge| est[edge.node] && (!edge.order_only || ctx.consumes(idx, edge.node)));
    }
    ctx.total
        .store(est.iter().filter(|&&stale| stale).count(), SeqCst);
    ctx.est = est;
    lap("staleness checked");
    if ctx.total.load(SeqCst) == 0 {
        return Ok(Summary { ok: true, ran: 0 });
    }

    let roots: Vec<usize> = (0..count)
        .filter(|&idx| graph.nodes[idx].deps.is_empty())
        .collect();
    let ctx = &ctx;
    pool.scope(|scope| {
        for idx in roots {
            scope.spawn(move |scope| ctx.run_node(scope, idx));
        }
    });
    state.flush();

    Ok(Summary {
        ok: !ctx.failed.load(SeqCst),
        ran: ctx.started.load(SeqCst),
    })
}

impl<'a> Ctx<'a> {
    /// Whether node `idx` recorded one of node `dep`'s outputs as a header dependency last time.
    fn consumes(&self, idx: usize, dep: usize) -> bool {
        self.state.entry(self.outs[idx][0]).is_some_and(|entry| {
            entry
                .deps
                .iter()
                .any(|dep_id| self.outs[dep].contains(dep_id))
        })
    }

    fn is_stale(&self, idx: usize) -> bool {
        self.stale_reason(idx).is_some()
    }

    fn stale_reason(&self, idx: usize) -> Option<String> {
        let mut oldest_output = u64::MAX;
        for &out in &self.outs[idx] {
            let mtime = self.stats.mtime(out);
            if mtime == 0 {
                return Some(format!(
                    "output {} is missing",
                    self.stats.paths[out as usize]
                ));
            }
            oldest_output = oldest_output.min(mtime);
        }
        let Some(entry) = self.state.entry(self.outs[idx][0]) else {
            return Some("never built".into());
        };
        if entry.hash != self.graph.nodes[idx].hash {
            return Some("command line changed".into());
        }
        if entry.mtime != oldest_output {
            return Some("an output was modified outside the build".into());
        }
        self.ins[idx].iter().chain(&entry.deps).find_map(|&dep| {
            let stamp = self.stats.stamp(dep);
            let path = || &self.stats.paths[dep as usize];
            if stamp == 0 {
                Some(format!("input {} is missing", path()))
            } else if stamp > oldest_output {
                Some(format!("input {} is newer than the output", path()))
            } else {
                None
            }
        })
    }

    fn run_node<'s>(&'s self, scope: &rayon::Scope<'s>, idx: usize) {
        if self.failed.load(SeqCst) {
            return;
        }
        match self.execute(idx) {
            Some(rebuilt) => self.rebuilt[idx].store(rebuilt, SeqCst),
            None => {
                self.failed.store(true, SeqCst);
                return;
            }
        }
        for &dependent in &self.graph.dependents[idx] {
            if self.remaining[dependent].fetch_sub(1, SeqCst) == 1 {
                scope.spawn(move |scope| self.run_node(scope, dependent));
            }
        }
    }

    fn report(&self, msg: String) {
        let _guard = self.print.lock().unwrap();
        eprint!("{msg}");
    }

    /// `Some(true)` if the node ran and its product changed; `Some(false)` if it was up to date or rewrote identical bytes; `None` on failure.
    fn execute(&self, idx: usize) -> Option<bool> {
        let node = &self.graph.nodes[idx];
        let dirty = self.is_stale(idx)
            || node
                .deps
                .iter()
                .any(|edge| !edge.order_only && self.rebuilt[edge.node].load(SeqCst));
        if !dirty {
            return Some(false);
        }
        if !self.est[idx] {
            self.total.fetch_add(1, SeqCst);
        }
        let number = self.started.fetch_add(1, SeqCst) + 1;
        {
            let _guard = self.print.lock().unwrap();
            println!("[{}/{}] {}", number, self.total.load(SeqCst), node.desc);
            if self.verbose {
                println!("{}", node.command_line());
            }
        }

        for output in &node.outputs {
            if let Some(dir) = output.parent() {
                if let Err(err) = fs::create_dir_all(dir) {
                    self.report(format!(
                        "FAILED: {}\ncannot create {}: {err}\n",
                        node.desc,
                        dir.display()
                    ));
                    return None;
                }
            }
        }
        if node.rm_outputs {
            let _ = fs::remove_file(&node.outputs[0]);
        }

        let rsp = match response_file(node) {
            Ok(rsp) => rsp,
            Err(err) => {
                self.report(format!(
                    "FAILED: {}\ncannot write the response file: {err}\n",
                    node.desc
                ));
                return None;
            }
        };
        let mut cmd = Command::new(&node.program);
        match &rsp {
            Some(path) => cmd.arg(format!("@{}", path.display())),
            None => cmd.args(node.args().iter()),
        };
        cmd.current_dir(&node.cwd)
            .envs(self.graph.env.iter().cloned());
        let output = match cmd.output() {
            Ok(output) => output,
            Err(err) => {
                self.report(format!(
                    "FAILED: {}\n{}\ncould not run {}: {err}\n",
                    node.desc,
                    node.command_line(),
                    node.program.display()
                ));
                return None;
            }
        };
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(path) = rsp {
            let _ = fs::remove_file(path);
        }

        let deps: Vec<PathBuf> = match &node.dep_style {
            DepStyle::None => Vec::new(),
            DepStyle::Gcc(depfile) => {
                let deps = fs::read_to_string(depfile)
                    .map(|text| parse_depfile(&text))
                    .unwrap_or_default();
                let _ = fs::remove_file(depfile);
                deps
            }
            DepStyle::Msvc => {
                let src_name = node
                    .inputs
                    .first()
                    .and_then(|input| input.file_name())
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let (text, deps) =
                    split_show_includes(&stdout, &self.graph.sys_includes, &src_name);
                stdout = text;
                deps
            }
        };

        if !output.status.success() {
            self.report(format!(
                "FAILED: {}\n{}\n{}{}",
                node.desc,
                node.command_line(),
                stdout,
                stderr
            ));
            return None;
        }
        if !stdout.trim().is_empty() || !stderr.trim().is_empty() {
            self.report(format!("{}{}", stdout, stderr));
        }

        let mut oldest_output = u64::MAX;
        for &out in &self.outs[idx] {
            let mtime = stat(Path::new(&self.stats.paths[out as usize]));
            if mtime == 0 {
                self.report(format!(
                    "FAILED: {}\nexpected output was not produced: {}\n",
                    node.desc, self.stats.paths[out as usize]
                ));
                return None;
            }
            self.stats.mtimes[out as usize].store(mtime, SeqCst);
            oldest_output = oldest_output.min(mtime);
        }
        let dep_ids = deps.iter().map(|path| self.state.intern(path)).collect();
        self.state
            .record(self.outs[idx][0], oldest_output, node.hash, dep_ids);
        if !node.cutoff {
            return Some(true);
        }
        match self.settle(self.outs[idx][0]) {
            Ok(changed) => Some(changed),
            Err(err) => {
                self.report(format!(
                    "FAILED: {}\ncannot read {}: {err}\n",
                    node.desc, self.stats.paths[self.outs[idx][0] as usize]
                ));
                None
            }
        }
    }

    /// Records the product's content hash; false when the bytes match the previous run, so dependents can skip.
    fn settle(&self, id: u32) -> std::io::Result<bool> {
        let mtime = self.stats.mtime(id);
        let hash = util::hash(&fs::read(&self.stats.paths[id as usize])?);
        let same = self
            .state
            .content(id)
            .filter(|content| content.hash == hash);
        let stamp = same.map_or(mtime, |content| content.stamp);
        self.state
            .record_content(id, Content { mtime, hash, stamp });
        self.stats.stamps[id as usize].store(stamp, SeqCst);
        Ok(same.is_none())
    }
}

/// Link and archive steps with huge object lists exceed the Windows 32k command-line limit; pass them via `@file`.
fn response_file(node: &Node) -> std::io::Result<Option<PathBuf>> {
    const LIMIT: usize = 8000;
    let args = node.args();
    if !matches!(node.kind, NodeKind::Archive | NodeKind::Link)
        || args.iter().map(|arg| arg.len() + 1).sum::<usize>() <= LIMIT
    {
        return Ok(None);
    }
    // Forward slashes on Windows: clang's response-file parser treats backslashes as escapes, the others accept both.
    let quote = |arg: &String| {
        let arg = if cfg!(windows) {
            arg.replace('\\', "/")
        } else {
            arg.replace('\\', "\\\\")
        };
        if !cfg!(windows) || arg.contains([' ', '"']) {
            format!("\"{}\"", arg.replace('"', "\\\""))
        } else {
            arg
        }
    };
    let path = node.outputs[0].with_extension("rsp");
    fs::write(&path, args.iter().map(quote).collect::<Vec<_>>().join("\n"))?;
    Ok(Some(path))
}

/// First rule of a Makefile depfile: `target: dep dep \<newline> dep`. `-MP` phantom rules follow and are ignored.
fn parse_depfile(text: &str) -> Vec<PathBuf> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let line = joined.lines().next().unwrap_or("");
    let bytes = line.as_bytes();
    // Skip the target; a drive colon like `C:/` is not followed by whitespace.
    let Some(colon) = (0..bytes.len()).find(|&idx| {
        bytes[idx] == b':'
            && bytes
                .get(idx + 1)
                .is_none_or(|next| next.is_ascii_whitespace())
    }) else {
        return Vec::new();
    };
    let mut deps = Vec::new();
    let mut current = String::new();
    let mut chars = line[colon + 1..].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            '$' if chars.peek() == Some(&'$') => {
                current.push('$');
                chars.next();
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    deps.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        deps.push(PathBuf::from(current));
    }
    deps
}

/// Removes `/showIncludes` noise from compiler output and returns the user-header dependencies.
fn split_show_includes(
    stdout: &str,
    sys_includes: &[String],
    src_name: &str,
) -> (String, Vec<PathBuf>) {
    const PREFIX: &str = "Note: including file:";
    let mut text = String::new();
    let mut deps = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(PREFIX) {
            let path = rest.trim();
            let lower = path.to_lowercase();
            if !sys_includes
                .iter()
                .any(|dir| lower.starts_with(dir.as_str()))
            {
                deps.push(PathBuf::from(path));
            }
        } else if line.trim() != src_name {
            text.push_str(line);
            text.push('\n');
        }
    }
    (text, deps)
}
