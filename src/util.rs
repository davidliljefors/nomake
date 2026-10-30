use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// SipHash costs more than the lookups it guards when a build touches tens of thousands of paths.
pub type Map<K, V> = HashMap<K, V, xxhash_rust::xxh3::Xxh3DefaultBuilder>;

/// XXH3-64: stable across versions and platforms, so it is safe to persist.
pub fn hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

pub fn hash_seeded(bytes: &[u8], seed: u64) -> u64 {
    xxhash_rust::xxh3::xxh3_64_with_seed(bytes, seed)
}

/// Joins onto the cwd and lexically resolves `.` and `..` (no symlink resolution, no `\\?\` prefix).
pub fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Path relative to `base` with forward slashes, for display and glob matching.
pub fn rel_str(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn seg_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut pat_pos, mut text_pos) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while text_pos < text.len() {
        if pat_pos < pattern.len()
            && (pattern[pat_pos] == b'?' || pattern[pat_pos] == text[text_pos])
        {
            pat_pos += 1;
            text_pos += 1;
        } else if pat_pos < pattern.len() && pattern[pat_pos] == b'*' {
            star = pat_pos;
            mark = text_pos;
            pat_pos += 1;
        } else if star != usize::MAX {
            pat_pos = star + 1;
            mark += 1;
            text_pos = mark;
        } else {
            return false;
        }
    }
    while pat_pos < pattern.len() && pattern[pat_pos] == b'*' {
        pat_pos += 1;
    }
    pat_pos == pattern.len()
}

fn path_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| path_match(rest, &path[skip..])),
        Some((segment, rest)) => {
            !path.is_empty()
                && seg_match(segment.as_bytes(), path[0].as_bytes())
                && path_match(rest, &path[1..])
        }
    }
}

/// Glob match of a `/`-separated relative path. `*` and `?` stay within a segment, `**` spans segments.
pub fn matches(pattern: &str, rel: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = rel
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    path_match(&pattern_segments, &path_segments)
}

/// Expands a glob against the filesystem, only descending where the pattern requires it.
pub fn glob(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let comps: Vec<&str> = pattern.split('/').collect();
    let mut out = Vec::new();
    expand(root, &comps, &mut out);
    out
}

fn expand(dir: &Path, comps: &[&str], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = comps.split_first() else {
        if dir.is_file() {
            out.push(dir.to_path_buf());
        }
        return;
    };
    if *first == "**" {
        expand(dir, rest, out);
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if !hidden && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                expand(&entry.path(), comps, out);
            }
        }
    } else if first.contains(['*', '?']) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if seg_match(
                first.as_bytes(),
                entry.file_name().to_string_lossy().as_bytes(),
            ) {
                expand(&entry.path(), rest, out);
            }
        }
    } else {
        expand(&dir.join(first), rest, out);
    }
}
