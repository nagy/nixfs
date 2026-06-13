use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, ReplyAttr, ReplyData, ReplyEntry, Request};
use libc::{EACCES, EINVAL, EIO, ENETUNREACH, ENOENT, ETIMEDOUT};

const NIX_EVAL_EXECUTABLE: &str = "nix";
const NIXPKGS: &str = "<nixpkgs>";

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

/// Runs `nix eval --raw -f '<nixpkgs>' '<attr>.outPath'` to resolve the store
/// path of an attribute. This evaluates the derivation (computes the output
/// path) but does NOT build anything — the build only happens on-demand when
/// Nix needs to materialise the store path.
///
/// On success returns `Ok(store_path)` (e.g. "/nix/store/...-hello-2.12.1").
/// On failure returns `Err(errno)` with a specific errno value so the caller
/// can map it to the appropriate FUSE error:
///   - ENOENT      — attribute doesn't exist in nixpkgs
///   - ETIMEDOUT   — nix eval timed out
///   - ENETUNREACH — network unreachable (can't fetch nixpkgs)
///   - EACCES      — permission denied
///   - EIO         — all other evaluation failures
fn nix_eval_outpath(attr: &str) -> Result<String, i32> {
    let attr_expr = format!("{attr}.outPath");
    eprintln!("Evaluating: {attr_expr:?} from {NIXPKGS:?}");
    let output = std::process::Command::new(NIX_EVAL_EXECUTABLE)
        .arg("eval")
        .arg("--raw")
        .arg("-f")
        .arg(NIXPKGS)
        .arg(&attr_expr)
        .output();
    match output {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8(output.stdout)
                    .map_err(|_| EIO)?
                    .trim_end_matches('\n')
                    .to_string();
                Ok(stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
                Err(classify_eval_error(&stderr))
            }
        }
        Err(e) => {
            // Couldn't even spawn nix — treat as I/O error
            eprintln!("Failed to spawn nix: {e}");
            Err(EIO)
        }
    }
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
        // Reject names that look like dotfiles — invalid as Nix attr names.
        if name.to_str().unwrap_or("").starts_with('.') {
            reply.error(EINVAL);
            return;
        }
        if name.to_str().unwrap_or("").ends_with('.') {
            reply.error(EINVAL);
            return;
        }
        let attr = name.to_str().unwrap();
        eprintln!("Lookup: {attr:?}");
        if parent != 1 {
            reply.error(ENOENT);
            return;
        }
        eprintln!("Inserting attr: {attr:?}");
        let hashinode = {
            let mut hasher = DefaultHasher::new();
            attr.hash(&mut hasher);
            hasher.finish()
        };
        // Resolve store path via `nix eval` — evaluates the derivation
        // (fast, no build needed) so readlink returns instantly.
        match nix_eval_outpath(attr) {
            Ok(out_path) => {
                reply.entry(&Duration::MAX, &make_symlink_attr(hashinode), 0);
                self.entries.insert(
                    hashinode,
                    Entry {
                        attr: attr.to_string(),
                        out_path: Some(out_path),
                    },
                );
            }
            Err(errno) => {
                reply.error(errno);
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
