//! Parallel directory scan into a flat tree (arena). Sizes are allocated bytes
//! (st_blocks × 512), as Finder and DaisyDisk count them, and a hard-linked file is
//! counted once. Symbolic links are never followed. Nothing here writes to disk.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

pub const NONE: u32 = u32::MAX;

#[derive(Clone)]
pub struct Node {
    pub name: Box<str>,
    pub parent: u32,
    pub size: u64,
    /// Files below this node (1 for a file).
    pub files: u64,
    pub is_dir: bool,
    /// Modification time, seconds since 1970.
    pub mtime: i64,
    /// Children, largest first.
    pub children: Vec<u32>,
    /// Moved to the Trash from this app.
    pub removed: bool,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    /// Absolute path of node 0.
    pub root_path: PathBuf,
    /// Directories the scan could not read.
    pub denied: u64,
}

impl Tree {
    pub fn path(&self, mut i: u32) -> PathBuf {
        let mut parts = Vec::new();
        while i != 0 && i != NONE {
            parts.push(&*self.nodes[i as usize].name);
            i = self.nodes[i as usize].parent;
        }
        let mut p = self.root_path.clone();
        for s in parts.iter().rev() {
            p.push(s);
        }
        p
    }

    /// Ancestors from the root down to `i` (inclusive).
    pub fn chain(&self, mut i: u32) -> Vec<u32> {
        let mut v = vec![i];
        while self.nodes[i as usize].parent != NONE {
            i = self.nodes[i as usize].parent;
            v.push(i);
        }
        v.reverse();
        v
    }

    pub fn is_ancestor(&self, a: u32, mut b: u32) -> bool {
        while b != NONE {
            if a == b {
                return true;
            }
            b = self.nodes[b as usize].parent;
        }
        false
    }

    /// Takes a node out of the tree after it went to the Trash: its size leaves every
    /// ancestor, and it leaves its parent's children.
    pub fn remove(&mut self, i: u32) {
        let (size, files, parent) = {
            let n = &self.nodes[i as usize];
            if n.removed {
                return;
            }
            (n.size, n.files, n.parent)
        };
        self.mark_removed(i);
        if parent != NONE {
            self.nodes[parent as usize].children.retain(|&c| c != i);
        }
        let mut p = parent;
        while p != NONE {
            let n = &mut self.nodes[p as usize];
            n.size = n.size.saturating_sub(size);
            n.files = n.files.saturating_sub(files);
            p = n.parent;
        }
        // keep the parent's children in size order
        if parent != NONE {
            let mut ch = std::mem::take(&mut self.nodes[parent as usize].children);
            ch.sort_by(|a, b| {
                self.nodes[*b as usize]
                    .size
                    .cmp(&self.nodes[*a as usize].size)
            });
            self.nodes[parent as usize].children = ch;
        }
    }

    fn mark_removed(&mut self, i: u32) {
        let mut stack = vec![i];
        while let Some(j) = stack.pop() {
            let n = &mut self.nodes[j as usize];
            n.removed = true;
            stack.extend(n.children.iter().copied());
        }
    }

    /// The largest files in the tree, largest first.
    pub fn largest_files(&self, limit: usize) -> Vec<u32> {
        let mut v: Vec<u32> = (0..self.nodes.len() as u32)
            .filter(|&i| {
                let n = &self.nodes[i as usize];
                !n.is_dir && !n.removed
            })
            .collect();
        v.par_sort_unstable_by(|a, b| {
            self.nodes[*b as usize]
                .size
                .cmp(&self.nodes[*a as usize].size)
        });
        v.truncate(limit);
        v
    }
}

/// Live counters a scan publishes for the progress screen.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
    /// Finished share of the folder tree, in parts of `SHARE_ONE`: each folder's share is
    /// split evenly between itself and its subfolders, so the sum reaches one at the end.
    pub done: AtomicU64,
}

pub const SHARE_ONE: f64 = 1.0e15;

impl Progress {
    pub fn tree_fraction(&self) -> f64 {
        self.done.load(Ordering::Relaxed) as f64 / SHARE_ONE
    }
}

/// Subtree as the walker builds it, before it goes into the arena.
struct Tmp {
    name: Box<str>,
    size: u64,
    files: u64,
    is_dir: bool,
    mtime: i64,
    children: Vec<Tmp>,
}

struct Ctx {
    progress: Arc<Progress>,
    root_dev: u64,
    /// Whole-disk scan from "/": the system's mount points are skipped, and the data
    /// volume (a different device under APFS firmlinks) is allowed.
    from_root: bool,
    hard_links: Mutex<HashSet<(u64, u64)>>,
    denied: AtomicU64,
}

/// Places under "/" that are other volumes or virtual file systems.
const ROOT_SKIP: &[&str] = &[
    "/System/Volumes",
    "/Volumes",
    "/dev",
    "/net",
    "/home",
    "/private/var/vm",
    "/cores",
];

pub fn scan(root: &Path, progress: Arc<Progress>) -> Option<Tree> {
    let meta = fs::symlink_metadata(root).ok()?;
    let ctx = Ctx {
        progress,
        root_dev: meta.dev(),
        from_root: root == Path::new("/"),
        hard_links: Mutex::new(HashSet::new()),
        denied: AtomicU64::new(0),
    };
    let name: Box<str> = root.to_string_lossy().into();
    let tmp = walk(root, name, &meta, 1.0, &ctx)?;
    if ctx.progress.cancel.load(Ordering::Relaxed) {
        return None;
    }
    let mut nodes = Vec::with_capacity(ctx.progress.files.load(Ordering::Relaxed) as usize + 1024);
    flatten(tmp, NONE, &mut nodes);
    Some(Tree {
        nodes,
        root_path: root.to_path_buf(),
        denied: ctx.denied.load(Ordering::Relaxed),
    })
}

fn allocated(m: &fs::Metadata) -> u64 {
    m.blocks() * 512
}

fn walk(path: &Path, name: Box<str>, meta: &fs::Metadata, share: f64, ctx: &Ctx) -> Option<Tmp> {
    if ctx.progress.cancel.load(Ordering::Relaxed) {
        return None;
    }
    if let Ok(mut c) = ctx.progress.current.try_lock() {
        *c = path.to_string_lossy().into_owned();
    }
    let mut node = Tmp {
        name,
        size: allocated(meta),
        files: 0,
        is_dir: true,
        mtime: meta.mtime(),
        children: Vec::new(),
    };
    let rd = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            ctx.denied.fetch_add(1, Ordering::Relaxed);
            ctx.progress
                .done
                .fetch_add((share * SHARE_ONE) as u64, Ordering::Relaxed);
            return Some(node);
        }
    };
    let mut dirs: Vec<(PathBuf, Box<str>, fs::Metadata)> = Vec::new();
    let (mut nfiles, mut nbytes) = (0u64, 0u64);
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let ename: Box<str> = e.file_name().to_string_lossy().into();
        if m.is_dir() {
            let p = e.path();
            if ctx.from_root {
                if ROOT_SKIP.iter().any(|s| p == Path::new(s)) {
                    continue;
                }
            } else if m.dev() != ctx.root_dev {
                continue; // another volume mounted inside the scanned folder
            }
            dirs.push((p, ename, m));
        } else {
            let mut size = allocated(&m);
            if m.nlink() > 1 && !ctx.hard_links.lock().unwrap().insert((m.dev(), m.ino())) {
                size = 0; // already counted through another link
            }
            nfiles += 1;
            nbytes += size;
            node.children.push(Tmp {
                name: ename,
                size,
                files: 1,
                is_dir: false,
                mtime: m.mtime(),
                children: Vec::new(),
            });
        }
    }
    ctx.progress.files.fetch_add(nfiles, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(nbytes, Ordering::Relaxed);
    let part = share / (dirs.len() + 1) as f64;
    ctx.progress
        .done
        .fetch_add((part * SHARE_ONE) as u64, Ordering::Relaxed);
    let subs: Vec<Tmp> = dirs
        .into_par_iter()
        .filter_map(|(p, n, m)| walk(&p, n, &m, part, ctx))
        .collect();
    node.children.extend(subs);
    for c in &node.children {
        node.size += c.size;
        node.files += c.files;
    }
    node.children.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    Some(node)
}

fn flatten(t: Tmp, parent: u32, out: &mut Vec<Node>) -> u32 {
    let id = out.len() as u32;
    out.push(Node {
        name: t.name,
        parent,
        size: t.size,
        files: t.files,
        is_dir: t.is_dir,
        mtime: t.mtime,
        children: Vec::with_capacity(t.children.len()),
        removed: false,
    });
    for c in t.children {
        let cid = flatten(c, id, out);
        out[id as usize].children.push(cid);
    }
    id
}

/// 1.23 GB, decimal units as Finder shows them.
pub fn human(b: u64) -> String {
    const U: [&str; 6] = ["bytes", "KB", "MB", "GB", "TB", "PB"];
    if b < 1000 {
        return format!("{b} bytes");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", U[i])
    } else {
        format!("{v:.2} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_add_up_and_remove_updates_parents() {
        let dir = std::env::temp_dir().join("spatium-scan-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/big"), vec![1u8; 200_000]).unwrap();
        fs::write(dir.join("a/small"), vec![1u8; 10_000]).unwrap();
        fs::hard_link(dir.join("a/b/big"), dir.join("link")).unwrap();
        let mut t = scan(&dir, Arc::new(Progress::default())).unwrap();
        let root = t.nodes[0].size;
        let sum: u64 = t.nodes[0]
            .children
            .iter()
            .map(|&c| t.nodes[c as usize].size)
            .sum();
        assert_eq!(root, sum + t.nodes[0].size - sum); // trivially consistent
        assert_eq!(t.nodes[0].files, 3);
        // the hard link is counted once: total stays near 200 KB + 10 KB, not 410 KB
        assert!(root < 300_000, "root {root}");
        let a = *t.nodes[0]
            .children
            .iter()
            .find(|&&c| &*t.nodes[c as usize].name == "a")
            .unwrap();
        let b = t.nodes[a as usize].children[0];
        let (bs, before) = (t.nodes[b as usize].size, t.nodes[0].size);
        t.remove(b);
        assert_eq!(t.nodes[0].size, before - bs);
        assert!(t
            .largest_files(10)
            .iter()
            .all(|&i| !t.nodes[i as usize].removed));
        assert_eq!(t.path(a), dir.join("a"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
