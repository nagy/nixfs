use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, ReplyAttr, ReplyData, ReplyEntry, Request};
use libc::{EIO, ENOENT};

const NIX_EVAL_EXECUTABLE: &str = "nix";
const NIXPKGS_NAME: &str = "<nixpkgs>";
const NIXPATH_SPLIT_CHAR: &str = "_";

fn make_symlink_attr(inode: u64) -> FileAttr {
    FileAttr {
        ino: inode,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH, // 1970-01-01 00:00:00
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

struct Entry {
    // Retained for future cache invalidation / re-evaluation (recommendation #3).
    #[allow(dead_code)]
    nixpath: String,
    #[allow(dead_code)]
    attr: String,
    /// The resolved Nix store path (e.g. /nix/store/...-hello-2.12.1).
    /// Resolved eagerly in lookup via `nix eval` — evaluates the derivation
    /// but does NOT build it.
    out_path: Option<String>,
}

#[derive(Default)]
struct NixFS {
    entries: std::collections::HashMap<u64, Entry>,
}

/// Runs `nix eval --raw -f '<nixpath>' '<attr>.outPath'` to resolve the store
/// path of an attribute. This evaluates the derivation (computes the output
/// path) but does NOT build anything — the build only happens on-demand when
/// Nix needs to materialise the store path.
///
/// Returns the resolved store path on success (e.g. "/nix/store/...-hello-2.12.1"),
/// or None if the attribute doesn't exist or evaluation fails.
fn nix_eval_outpath(nixpath: &str, attr: &str) -> Option<String> {
    let attr_expr = format!("{attr}.outPath");
    eprintln!("Evaluating: {attr_expr:?} from {nixpath:?}");
    let output = std::process::Command::new(NIX_EVAL_EXECUTABLE)
        .arg("eval")
        .arg("--raw")
        .arg("-f")
        .arg(nixpath)
        .arg(&attr_expr)
        .output();
    match output {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8(output.stdout)
                    .ok()?
                    .trim_end_matches('\n')
                    .to_string();
                Some(stdout)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

fn split_nixpath_from_attr(filepath: String) -> (String, String) {
    match filepath.strip_prefix(NIXPATH_SPLIT_CHAR) {
        None => {
            // default case
            (NIXPKGS_NAME.to_string(), filepath)
        }
        Some(rest) => {
            let (nixpath, rest) = rest.split_once(NIXPATH_SPLIT_CHAR).unwrap();
            (format!("<{nixpath}>"), rest.to_string())
        }
    }
}

impl fuser::Filesystem for NixFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        // skip some know non-existing values
        if name.to_str().unwrap_or("").starts_with('.') {
            reply.error(ENOENT);
            return;
        }
        if name.to_str().unwrap_or("").ends_with('.') {
            reply.error(ENOENT);
            return;
        }
        let name = name.to_str().unwrap();
        eprintln!("Lookup: {name:?}");
        let (nixpath, attr) = split_nixpath_from_attr(name.to_string());
        if parent != 1 {
            reply.error(ENOENT);
            return;
        }
        eprintln!("Inserting attr: {attr:?}, {nixpath}");
        let hashinode = {
            let mut hasher = DefaultHasher::new();
            nixpath.hash(&mut hasher);
            attr.hash(&mut hasher);
            hasher.finish()
        };
        // Resolve store path via `nix eval` — evaluates the derivation
        // (fast, no build needed) so readlink returns instantly.
        match nix_eval_outpath(&nixpath, &attr) {
            Some(out_path) => {
                reply.entry(&Duration::MAX, &make_symlink_attr(hashinode), 0);
                self.entries.insert(
                    hashinode,
                    Entry {
                        nixpath,
                        attr,
                        out_path: Some(out_path),
                    },
                );
            }
            None => {
                reply.error(ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        /* parent */
        if ino == 1 {
            reply.attr(
                &Duration::MAX,
                &FileAttr {
                    ino: 1,
                    size: 0,
                    blocks: 0,
                    atime: UNIX_EPOCH, // 1970-01-01 00:00:00
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
                },
            );
            return;
        }
        if self.entries.contains_key(&ino) {
            reply.attr(&Duration::MAX, &make_symlink_attr(ino));
            return;
        }
        reply.error(ENOENT);
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        if let Some(entry) = self.entries.get(&inode) {
            // Path was already resolved in lookup via `nix eval`.
            // No build needed — just return the cached store path.
            match &entry.out_path {
                Some(path) => reply.data(path.as_bytes()),
                None => reply.error(EIO),
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
        if ino != 1 {
            reply.error(ENOENT);
            return;
        }
        let entries = [
            (1, FileType::Directory, "."),
            (1, FileType::Directory, ".."),
        ];
        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            // i + 1 means the index of the next entry
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
    fuser::mount2(
        NixFS::default(),
        mount_path,
        &[
            MountOption::RO,
            MountOption::FSName("nixfs".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
        ],
    )
    .unwrap();
}
