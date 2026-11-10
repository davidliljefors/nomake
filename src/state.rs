//! Persistent build state: an append-only log of interned paths, build entries and content hashes, like Ninja's log and deps files in one.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, RwLock};
use std::thread::JoinHandle;

use rayon::prelude::*;

use crate::util::Map;
use crate::Result;

const MAGIC: &[u8; 8] = b"NOMKST02";

pub struct Entry {
    pub mtime: u64,
    /// The newest input stamp the step saw when it started.
    pub inputs: u64,
    pub hash: u64,
    pub deps: Vec<u32>,
}

/// A product's content hash and the mtime at which that content first appeared; `stamp` is what consumers compare against.
#[derive(Clone, Copy)]
pub struct Content {
    /// The file's own mtime when this was recorded; a mismatch means it was modified behind our back.
    pub mtime: u64,
    pub hash: u64,
    pub stamp: u64,
}

#[derive(Default)]
struct Tables {
    paths: Vec<String>,
    ids: Map<String, u32>,
    entries: Map<u32, Arc<Entry>>,
    contents: Map<u32, Content>,
}

pub struct State {
    tables: RwLock<Tables>,
    log: Option<mpsc::Sender<Message>>,
    writer: Option<JoinHandle<()>>,
}

enum Message {
    Append(Vec<u8>),
    Flush(mpsc::Sender<()>),
}

fn write_log(file: File, messages: mpsc::Receiver<Message>) {
    let mut out = BufWriter::new(file);
    let mut warned = false;
    let mut check = |result: std::io::Result<()>| {
        if let Err(err) = result {
            if !std::mem::replace(&mut warned, true) {
                eprintln!("nomake: warning: cannot write the build state, the next build will redo work: {err}");
            }
        }
    };
    for message in messages {
        match message {
            Message::Append(buf) => check(out.write_all(&buf)),
            Message::Flush(done) => {
                check(out.flush());
                let _ = done.send(());
            }
        }
    }
    check(out.flush());
}

fn put_path(buf: &mut Vec<u8>, path: &str) {
    buf.push(1);
    buf.extend((path.len() as u32).to_le_bytes());
    buf.extend(path.as_bytes());
}

fn put_entry(buf: &mut Vec<u8>, out: u32, entry: &Entry) {
    buf.push(2);
    buf.extend(out.to_le_bytes());
    buf.extend(entry.mtime.to_le_bytes());
    buf.extend(entry.inputs.to_le_bytes());
    buf.extend(entry.hash.to_le_bytes());
    buf.extend((entry.deps.len() as u32).to_le_bytes());
    for dep in &entry.deps {
        buf.extend(dep.to_le_bytes());
    }
}

fn put_content(buf: &mut Vec<u8>, path: u32, content: &Content) {
    buf.push(3);
    buf.extend(path.to_le_bytes());
    buf.extend(content.mtime.to_le_bytes());
    buf.extend(content.hash.to_le_bytes());
    buf.extend(content.stamp.to_le_bytes());
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(self.pos..self.pos.checked_add(count)?)?;
        self.pos += count;
        Some(bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// Returns the tables, the number of records read, and whether the whole file parsed cleanly.
fn parse(data: &[u8]) -> (Tables, usize, bool) {
    let mut tables = Tables::default();
    if !data.starts_with(MAGIC) {
        return (tables, 0, false);
    }
    let mut reader = Reader {
        data,
        pos: MAGIC.len(),
    };
    let mut records = 0;
    while reader.pos < data.len() {
        let ok = (|| {
            match reader.take(1)?[0] {
                1 => {
                    let len = reader.u32()? as usize;
                    let path = String::from_utf8_lossy(reader.take(len)?).into_owned();
                    tables.ids.insert(path.clone(), tables.paths.len() as u32);
                    tables.paths.push(path);
                }
                2 => {
                    let out = reader.u32()?;
                    let mtime = reader.u64()?;
                    let inputs = reader.u64()?;
                    let hash = reader.u64()?;
                    let count = reader.u32()? as usize;
                    let deps = (0..count)
                        .map(|_| reader.u32())
                        .collect::<Option<Vec<_>>>()?;
                    tables.entries.insert(
                        out,
                        Arc::new(Entry {
                            mtime,
                            inputs,
                            hash,
                            deps,
                        }),
                    );
                }
                3 => {
                    let path = reader.u32()?;
                    let content = Content {
                        mtime: reader.u64()?,
                        hash: reader.u64()?,
                        stamp: reader.u64()?,
                    };
                    tables.contents.insert(path, content);
                }
                _ => return None,
            }
            Some(())
        })();
        if ok.is_none() {
            return (tables, records, false);
        }
        records += 1;
    }
    (tables, records, true)
}

/// Re-serialises only live entries and the paths they reference.
fn compact(old: &Tables) -> (Tables, Vec<u8>) {
    let mut tables = Tables::default();
    let mut buf = MAGIC.to_vec();
    let intern = |tables: &mut Tables, buf: &mut Vec<u8>, id: u32| -> u32 {
        let path = &old.paths[id as usize];
        if let Some(&new_id) = tables.ids.get(path) {
            return new_id;
        }
        let new_id = tables.paths.len() as u32;
        tables.paths.push(path.clone());
        tables.ids.insert(path.clone(), new_id);
        put_path(buf, path);
        new_id
    };
    for (&out, entry) in &old.entries {
        let out = intern(&mut tables, &mut buf, out);
        let deps = entry
            .deps
            .iter()
            .map(|&dep| intern(&mut tables, &mut buf, dep))
            .collect();
        let entry = Entry {
            mtime: entry.mtime,
            inputs: entry.inputs,
            hash: entry.hash,
            deps,
        };
        put_entry(&mut buf, out, &entry);
        tables.entries.insert(out, Arc::new(entry));
    }
    for (&path, content) in &old.contents {
        let path = intern(&mut tables, &mut buf, path);
        put_content(&mut buf, path, content);
        tables.contents.insert(path, *content);
    }
    (tables, buf)
}

impl State {
    pub fn open(path: &Path) -> Result<State> {
        let data = if path.exists() {
            fs::read(path)?
        } else {
            Vec::new()
        };
        let (mut tables, records, clean) = parse(&data);
        let dead = records
            > 2 * (tables.entries.len() + tables.contents.len() + tables.paths.len()) + 1024;
        if !clean || dead {
            let (compacted, buf) = compact(&tables);
            fs::write(path, buf)?;
            tables = compacted;
        }
        let file = OpenOptions::new().append(true).open(path)?;
        let (log, messages) = mpsc::channel();
        let writer = std::thread::Builder::new()
            .name("state-log".into())
            .spawn(move || write_log(file, messages))?;
        Ok(State {
            tables: RwLock::new(tables),
            log: Some(log),
            writer: Some(writer),
        })
    }

    fn append(&self, buf: Vec<u8>) {
        if let Some(log) = &self.log {
            let _ = log.send(Message::Append(buf));
        }
    }

    pub fn intern(&self, path: &Path) -> u32 {
        let key = path.to_string_lossy();
        if let Some(&id) = self.tables.read().unwrap().ids.get(key.as_ref()) {
            return id;
        }
        let mut tables = self.tables.write().unwrap();
        if let Some(&id) = tables.ids.get(key.as_ref()) {
            return id;
        }
        let id = tables.paths.len() as u32;
        tables.paths.push(key.to_string());
        tables.ids.insert(key.to_string(), id);
        let mut buf = Vec::new();
        put_path(&mut buf, &key);
        self.append(buf);
        id
    }

    /// Ids for many path lists at once: hits are resolved in parallel under a single read lock, misses are interned after.
    pub fn intern_lists(&self, lists: &[&[PathBuf]]) -> Vec<Vec<u32>> {
        let tables = self.tables.read().unwrap();
        let mut ids: Vec<Vec<u32>> = lists
            .par_iter()
            .map(|paths| {
                paths
                    .iter()
                    .map(|path| {
                        tables
                            .ids
                            .get(path.to_string_lossy().as_ref())
                            .copied()
                            .unwrap_or(u32::MAX)
                    })
                    .collect()
            })
            .collect();
        drop(tables);
        for (paths, row) in lists.iter().zip(&mut ids) {
            for (path, id) in paths.iter().zip(row) {
                if *id == u32::MAX {
                    *id = self.intern(path);
                }
            }
        }
        ids
    }

    pub fn paths(&self) -> Vec<String> {
        self.tables.read().unwrap().paths.clone()
    }

    pub fn entry(&self, out: u32) -> Option<Arc<Entry>> {
        self.tables.read().unwrap().entries.get(&out).cloned()
    }

    pub fn record(&self, out: u32, mtime: u64, inputs: u64, hash: u64, deps: Vec<u32>) {
        let entry = Entry {
            mtime,
            inputs,
            hash,
            deps,
        };
        let mut buf = Vec::new();
        put_entry(&mut buf, out, &entry);
        let mut tables = self.tables.write().unwrap();
        tables.entries.insert(out, Arc::new(entry));
        self.append(buf);
    }

    pub fn contents(&self) -> Vec<(u32, Content)> {
        self.tables
            .read()
            .unwrap()
            .contents
            .iter()
            .map(|(&id, &content)| (id, content))
            .collect()
    }

    pub fn content(&self, path: u32) -> Option<Content> {
        self.tables.read().unwrap().contents.get(&path).copied()
    }

    pub fn record_content(&self, path: u32, content: Content) {
        let mut buf = Vec::new();
        put_content(&mut buf, path, &content);
        self.tables.write().unwrap().contents.insert(path, content);
        self.append(buf);
    }

    /// Returns once everything recorded so far is written to the file.
    pub fn flush(&self) {
        let Some(log) = &self.log else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if log.send(Message::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.log = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}
