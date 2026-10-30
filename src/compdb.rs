//! `compile_commands.json` for clangd and friends.

use std::fs;
use std::path::Path;

use crate::dag::{Graph, NodeKind};
use crate::util;
use crate::Result;

const FILE: &str = "compile_commands.json";

fn json(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Everything the file depends on; node hashes already cover each compile command line.
fn fingerprint(graph: &Graph) -> u64 {
    let mut bytes = Vec::new();
    for node in graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Compile)
    {
        bytes.extend(node.hash.to_le_bytes());
        bytes.extend(node.outputs[0].as_os_str().as_encoded_bytes());
    }
    util::hash(&bytes)
}

/// Writes the database and records its fingerprint in `stamp`; returns the number of entries.
pub fn generate(graph: &Graph, stamp: &Path) -> Result<usize> {
    let entries: Vec<String> = graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Compile)
        .map(|node| {
            let args: Vec<String> = std::iter::once(node.program.to_string_lossy().into_owned())
                .chain(node.args().iter().cloned())
                .map(|arg| json(&arg))
                .collect();
            format!(
                "  {{\"directory\": {}, \"file\": {}, \"arguments\": [{}]}}",
                json(&graph.root_dir.to_string_lossy()),
                json(&node.inputs[0].to_string_lossy()),
                args.join(", ")
            )
        })
        .collect();
    fs::write(
        graph.root_dir.join(FILE),
        format!("[\n{}\n]\n", entries.join(",\n")),
    )?;
    fs::create_dir_all(stamp.parent().unwrap())?;
    fs::write(stamp, fingerprint(graph).to_string())?;
    Ok(entries.len())
}

/// Keeps an existing database current; projects that never generated one are left alone.
pub fn refresh(graph: &Graph, stamp: &Path) -> Result<()> {
    if !graph.root_dir.join(FILE).is_file() {
        return Ok(());
    }
    if fs::read_to_string(stamp).ok().as_deref() != Some(fingerprint(graph).to_string().as_str()) {
        generate(graph, stamp)?;
    }
    Ok(())
}
