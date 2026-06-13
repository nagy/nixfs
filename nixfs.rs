use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant, UNIX_EPOCH};

use fuser::{FileAttr, FileType, ReplyAttr, ReplyData, ReplyEntry, Request};
use libc::{EACCES, EINVAL, EIO, ENETUNREACH, ENOENT, ENOTDIR, ETIMEDOUT};

const NIX_EXECUTABLE: &str = "nix";
const NIXPKGS: &str = "<nixpkgs>";
/// How long cached directory listings and resolved store paths remain valid.
const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

fn make_symlink_attr(inode: u64) -> FileAttr {
    FileAttr {
        ino: inode,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Symlink,
        perm: 0o444,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

fn make_dir_attr(inode: u64) -> FileAttr {
    FileAttr {
        ino: inode,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o555,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

enum EntryKind {
    /// A Nix derivation — appears as a symlink.
    Symlink {
        /// Dotted attr path, e.g. "python3Packages.numpy". Used for lazy resolution.
        attr_path: String,
        /// Cached store path. None if created by readdir (resolved lazily).
        out_path: Option<String>,
    },
    /// A Nix attribute set — appears as a directory.
    Dir {
        /// Dotted attr path, e.g. "python3Packages".
        attr_path: String,
        /// Cached child (name, inode, type) list. None = not yet loaded.
        children: Option<Vec<(String, u64, FileType)>>,
    },
}

struct Entry {
    kind: EntryKind,
    /// When this entry was created or last had its cached data refreshed.
    created: Instant,
}

impl Entry {
    fn new(kind: EntryKind) -> Self {
        Entry {
            kind,
            created: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.created.elapsed() > CACHE_TTL
    }
}

#[derive(Default)]
struct NixFS {
    entries: std::collections::HashMap<u64, Entry>,
}

/// Hash an attribute path to a deterministic 64-bit inode.
fn inode_for_attr_path(attr_path: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    attr_path.hash(&mut hasher);
    hasher.finish()
}

/// Result of evaluating a Nix attr at a given dotted path.
enum EvalResult {
    /// The attribute is a derivation; contains its store path.
    Symlink(String),
    /// The attribute is an attr set (i.e. a directory).
    Directory,
    /// Evaluation failed; contains the appropriate errno.
    Err(i32),
}

/// Runs `nix eval --raw -f '<nixpkgs>' '<attr_path>.outPath'`.
///
/// Three possible outcomes:
/// - Success → Symlink(store_path) — it's a derivation.
/// - Fails with "value is a set" → Directory — it's an attr set.
/// - Any other failure → Err(errno).
fn nix_eval_attr(attr_path: &str) -> EvalResult {
    let expr = format!("{attr_path}.outPath");
    eprintln!("Evaluating: {expr:?} from {NIXPKGS:?}");
    let output = std::process::Command::new(NIX_EXECUTABLE)
        .arg("eval")
        .arg("--raw")
        .arg("-f")
        .arg(NIXPKGS)
        .arg(&expr)
        .output();
    match output {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8(output.stdout).map_err(|_| EIO);
                match stdout {
                    Ok(s) => EvalResult::Symlink(s.trim_end_matches('\n').to_string()),
                    Err(e) => EvalResult::Err(e),
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
                eprintln!("nix_eval_attr failed (status {}): {stderr}", output.status);
                // If nix eval failed because it's a set, treat as a directory.
                // Two patterns:
                //   - "value is a set"  (old nix versions)
                //   - "attribute 'outpath' in selection path '...outpath' not found"
                //     (newer nix — means the attr exists but isn't a derivation)
                if stderr.contains("value is a set")
                    || stderr.contains("'outpath' in selection path")
                {
                    EvalResult::Directory
                } else {
                    EvalResult::Err(classify_eval_error(&stderr))
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to spawn nix: {e}");
            EvalResult::Err(EIO)
        }
    }
}

/// Lists the children of a Nix attr set at `attr_path`.
/// Returns a vec of (name, inode, FileType) on success, or errno on failure.
fn nix_list_directory(attr_path: &str) -> Result<Vec<(String, u64, FileType)>, i32> {
    let set_expr = if attr_path.is_empty() {
        "pkgs".to_string()
    } else {
        format!("pkgs.{attr_path}")
    };
    // Build a Nix expression that returns {"name": "drv"|"dir"|"broken", ...}.
    // Uses tryEval to skip derivations/values that fail evaluation.
    // Note: trailing spaces before each \ are intentional — they become
    // token separators after the line continuation removes the newline.
    let expr = format!(
        "let pkgs = import <nixpkgs> {{}}; \
        classify = v: let r = builtins.tryEval v; in if !r.success then \"broken\" \
        else if builtins.isAttrs r.value && r.value ? type then \"drv\" \
        else if builtins.isAttrs r.value then \"dir\" \
        else \"other\"; in builtins.mapAttrs (n: v: classify v) {set_expr}"
    );
    eprintln!("Listing directory: {attr_path:?}");
    eprintln!("  nix expr: {expr}");
    let output = std::process::Command::new(NIX_EXECUTABLE)
        .arg("eval")
        .arg("--impure")
        .arg("--json")
        .arg("--expr")
        .arg(&expr)
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to spawn nix: {e}");
            return Err(EIO);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("nix_list_directory failed: {stderr}");
        return Err(classify_eval_error(&stderr.to_lowercase()));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|e| {
        eprintln!("nix output not valid UTF-8: {e}");
        EIO
    })?;
    // Parse JSON: {"hello": "drv", "python3Packages": "dir", ...}
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&stdout).map_err(|e| {
            eprintln!("Failed to parse nix JSON output: {e}");
            eprintln!("  raw output: {stdout}");
            EIO
        })?;
    let mut children = Vec::with_capacity(map.len());
    for (name, kind_val) in map {
        let kind_str = kind_val.as_str().unwrap_or("other");
        let file_type = match kind_str {
            "drv" => FileType::Symlink,
            "dir" => FileType::Directory,
            _ => continue,
        };
        let child_path = if attr_path.is_empty() {
            name.clone()
        } else {
            format!("{attr_path}.{name}")
        };
        let inode = inode_for_attr_path(&child_path);
        children.push((name, inode, file_type));
    }
    Ok(children)
}

/// Maps `nix eval` stderr to a specific errno.
fn classify_eval_error(stderr: &str) -> i32 {
    if stderr.contains("does not provide attribute")
        || stderr.contains("attribute '") && stderr.contains("' missing")
        || stderr.contains("does not exist")
    {
        ENOENT
    } else if stderr.contains("timed out") || stderr.contains("timeout") {
        ETIMEDOUT
    } else if stderr.contains("could not resolve")
        || stderr.contains("unreachable")
        || stderr.contains("network")
        || stderr.contains("connection refused")
        || stderr.contains("name or service not known")
    {
        ENETUNREACH
    } else if stderr.contains("permission denied") || stderr.contains("access denied") {
        EACCES
    } else {
        EIO
    }
}

impl fuser::Filesystem for NixFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        // Reject non-UTF-8 names — Nix attr names are always valid UTF-8.
        let Some(child_name) = name.to_str() else {
            reply.error(EINVAL);
            return;
        };
        // Reject names that look like dotfiles — invalid as Nix attr names.
        if child_name.starts_with('.') || child_name.ends_with('.') {
            reply.error(EINVAL);
            return;
        }
        eprintln!("Lookup: {child_name:?} in parent {parent}");

        // Build the full dotted attr path for this child.
        let child_path = if parent == 1 {
            // Root directory — attr path is just the name.
            child_name.to_string()
        } else {
            // Subdirectory — join parent path + "." + name.
            let parent_entry = match self.entries.get(&parent) {
                Some(e) => e,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let parent_path = match &parent_entry.kind {
                EntryKind::Dir { attr_path, .. } => attr_path.as_str(),
                _ => {
                    reply.error(ENOTDIR);
                    return;
                }
            };
            format!("{parent_path}.{child_name}")
        };

        let inode = inode_for_attr_path(&child_path);

        // If we already have an entry (created by readdir), just reply with it.
        if let Some(entry) = self.entries.get(&inode) {
            let attr = match &entry.kind {
                EntryKind::Symlink { .. } => make_symlink_attr(inode),
                EntryKind::Dir { .. } => make_dir_attr(inode),
            };
            reply.entry(&Duration::MAX, &attr, 0);
            return;
        }

        match nix_eval_attr(&child_path) {
            EvalResult::Symlink(out_path) => {
                reply.entry(&Duration::MAX, &make_symlink_attr(inode), 0);
                self.entries.insert(
                    inode,
                    Entry::new(EntryKind::Symlink {
                        attr_path: child_path,
                        out_path: Some(out_path),
                    }),
                );
            }
            EvalResult::Directory => {
                reply.entry(&Duration::MAX, &make_dir_attr(inode), 0);
                self.entries.insert(
                    inode,
                    Entry::new(EntryKind::Dir {
                        attr_path: child_path,
                        children: None,
                    }),
                );
            }
            EvalResult::Err(errno) => {
                reply.error(errno);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&Duration::MAX, &make_dir_attr(1));
            return;
        }
        if let Some(entry) = self.entries.get(&ino) {
            let attr = match &entry.kind {
                EntryKind::Symlink { .. } => make_symlink_attr(ino),
                EntryKind::Dir { .. } => make_dir_attr(ino),
            };
            reply.attr(&Duration::MAX, &attr);
            return;
        }
        reply.error(ENOENT);
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        if let Some(entry) = self.entries.get_mut(&inode) {
            // Capture whether we need a fresh resolve before borrowing kind.
            let expired = entry.is_expired();
            match &mut entry.kind {
                EntryKind::Symlink {
                    attr_path,
                    out_path,
                } => {
                    let need_resolve = out_path.is_none() || expired;
                    if need_resolve {
                        if let EvalResult::Symlink(path) = nix_eval_attr(attr_path) {
                            entry.created = Instant::now();
                            *out_path = Some(path);
                        }
                    }
                    match out_path {
                        Some(path) => reply.data(path.as_bytes()),
                        None => reply.error(EIO),
                    }
                }
                EntryKind::Dir { .. } => {
                    reply.error(EINVAL);
                }
            }
            return;
        }
        reply.error(ENOENT);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        // Determine the attr path for this directory.
        let (dir_path, children): (String, Vec<(String, u64, FileType)>) = if ino == 1 {
            // Root: attr path is empty.
            let attr_path = String::new();
            let need_load = match self.entries.get(&1) {
                Some(e) => match &e.kind {
                    EntryKind::Dir {
                        children: Some(_), ..
                    } => e.is_expired(),
                    _ => true,
                },
                None => true,
            };
            let children = if need_load {
                match nix_list_directory("") {
                    Ok(c) => {
                        self.entries.insert(
                            1,
                            Entry::new(EntryKind::Dir {
                                attr_path: attr_path.clone(),
                                children: Some(c.clone()),
                            }),
                        );
                        c
                    }
                    Err(errno) => {
                        // If load fails but we had stale data, return the stale data.
                        match self.entries.get(&1).and_then(|e| match &e.kind {
                            EntryKind::Dir { children, .. } => children.clone(),
                            _ => None,
                        }) {
                            Some(stale) => stale,
                            None => {
                                reply.error(errno);
                                return;
                            }
                        }
                    }
                }
            } else {
                self.entries
                    .get(&1)
                    .and_then(|e| match &e.kind {
                        EntryKind::Dir { children, .. } => children.clone(),
                        _ => None,
                    })
                    .unwrap_or_default()
            };
            (attr_path, children)
        } else {
            let entry = match self.entries.get_mut(&ino) {
                Some(e) => e,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let expired = entry.is_expired();
            let (attr_path, children) = match &mut entry.kind {
                EntryKind::Dir {
                    attr_path,
                    children: cached,
                } => {
                    let need_load = cached.is_none() || expired;
                    let children = if need_load {
                        match nix_list_directory(attr_path) {
                            Ok(c) => {
                                entry.created = Instant::now();
                                *cached = Some(c.clone());
                                c
                            }
                            Err(errno) => {
                                // If reload fails but we had stale data, return that.
                                if let Some(stale) = cached.clone() {
                                    stale
                                } else {
                                    reply.error(errno);
                                    return;
                                }
                            }
                        }
                    } else {
                        cached.clone().unwrap_or_default()
                    };
                    (attr_path.clone(), children)
                }
                _ => {
                    reply.error(ENOTDIR);
                    return;
                }
            };
            (attr_path, children)
        };

        // Insert stub entries for children not already in the map,
        // so getattr and readlink can find them without a prior lookup.
        for (name, child_inode, ftype) in &children {
            if !self.entries.contains_key(child_inode) {
                let child_path = if dir_path.is_empty() {
                    name.clone()
                } else {
                    format!("{dir_path}.{name}")
                };
                let kind = match ftype {
                    FileType::Symlink => EntryKind::Symlink {
                        attr_path: child_path,
                        out_path: None, // resolved lazily by readlink
                    },
                    FileType::Directory => EntryKind::Dir {
                        attr_path: child_path,
                        children: None,
                    },
                    _ => continue,
                };
                self.entries.insert(*child_inode, Entry::new(kind));
            }
        }

        // Build the full entry list: "." + ".." + children.
        let parent_inode = if ino == 1 {
            1
        } else {
            self.entries
                .get(&ino)
                .and_then(|e| match &e.kind {
                    EntryKind::Dir { attr_path, .. } => attr_path.rfind('.').map(|pos| {
                        let parent_path = &attr_path[..pos];
                        if parent_path.is_empty() {
                            1
                        } else {
                            inode_for_attr_path(parent_path)
                        }
                    }),
                    _ => None,
                })
                .unwrap_or(1)
        };

        let mut all: Vec<(u64, FileType, &str)> = Vec::with_capacity(children.len() + 2);
        all.push((ino, FileType::Directory, "."));
        all.push((parent_inode, FileType::Directory, ".."));
        for (name, child_inode, ftype) in &children {
            all.push((*child_inode, *ftype, name.as_str()));
        }
        for (i, entry) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
    }
}

fn main() {
    use fuser::MountOption;
    let args: Vec<String> = std::env::args().collect();
    let default_mount_path = &"/nixfs".to_string();
    let mount_path = &args.get(1).unwrap_or(default_mount_path);
    if let Err(e) = fuser::mount2(
        NixFS::default(),
        mount_path,
        &[
            MountOption::RO,
            MountOption::FSName("nixfs".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
        ],
    ) {
        eprintln!("Failed to mount {}: {e}", mount_path);
        std::process::exit(1);
    }
}
