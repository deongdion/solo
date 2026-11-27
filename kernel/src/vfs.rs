//! Virtual file system confined to a rootfs directory.
//!
//! Guest paths are normalized before they reach the host: `..` never climbs above `/`, so no guest
//! path can name a host file outside the rootfs. Files are read whole into memory on open; writes stay
//! in an in-memory overlay and never touch the host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Normalizes an absolute guest path: collapses `.`, `..` and repeated slashes; `..` stops at `/`.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Joins a possibly relative guest path onto a directory and normalizes it.
pub fn join(directory: &str, path: &str) -> String {
    if path.starts_with('/') { normalize(path) } else { normalize(&format!("{directory}/{path}")) }
}

#[derive(Debug, Clone)]
pub enum Node {
    File(Vec<u8>),
    Directory,
}

#[derive(Clone)]
pub struct Vfs {
    root: Option<PathBuf>,
    /// Files created or written by the guest, and virtual files the runtime provides.
    overlay: HashMap<String, Vec<u8>>,
    removed: Vec<String>,
}

impl Vfs {
    pub fn new(root: Option<PathBuf>) -> Vfs {
        Vfs { root, overlay: HashMap::new(), removed: Vec::new() }
    }

    pub fn add_file(&mut self, path: &str, data: Vec<u8>) {
        let path = normalize(path);
        self.removed.retain(|p| *p != path);
        self.overlay.insert(path, data);
    }

    pub fn remove(&mut self, path: &str) -> bool {
        let path = normalize(path);
        let existed = self.lookup(&path).is_some();
        self.overlay.remove(&path);
        self.removed.push(path);
        existed
    }

    fn host_path(&self, path: &str) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        let mut host = root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            host.push(part);
        }
        // normalize() already removed every `..`; this guards against symlinks inside the rootfs.
        let canonical_root = root.canonicalize().ok()?;
        match host.canonicalize() {
            Ok(real) if real.starts_with(&canonical_root) => Some(real),
            Ok(_) => None,
            Err(_) => Some(host),
        }
    }

    /// Looks a normalized guest path up: overlay first, then the rootfs.
    pub fn lookup(&self, path: &str) -> Option<Node> {
        if self.removed.iter().any(|p| p == path) {
            return None;
        }
        if let Some(data) = self.overlay.get(path) {
            return Some(Node::File(data.clone()));
        }
        if path == "/" || self.overlay.keys().any(|k| k.starts_with(&format!("{path}/"))) {
            return Some(Node::Directory);
        }
        let host = self.host_path(path)?;
        let meta = std::fs::metadata(&host).ok()?;
        if meta.is_dir() {
            Some(Node::Directory)
        } else {
            std::fs::read(&host).ok().map(Node::File)
        }
    }

    pub fn store(&mut self, path: &str, data: Vec<u8>) {
        self.overlay.insert(path.to_string(), data);
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_dot_never_leaves_the_root() {
        assert_eq!(normalize("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize("/data/../../.."), "/");
        assert_eq!(join("/data/app", "../../../x"), "/x");
        assert_eq!(join("/a", "b//./c"), "/a/b/c");
    }

    #[test]
    fn overlay_shadows_and_removes() {
        let mut vfs = Vfs::new(None);
        vfs.add_file("/proc/self/cmdline", b"app\0".to_vec());
        assert!(matches!(vfs.lookup("/proc/self/cmdline"), Some(Node::File(d)) if d == b"app\0"));
        assert!(matches!(vfs.lookup("/proc/self"), Some(Node::Directory)));
        assert!(vfs.remove("/proc/self/cmdline"));
        assert!(vfs.lookup("/proc/self/cmdline").is_none());
    }
}
