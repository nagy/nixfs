use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::time::{Duration, Instant, UNIX_EPOCH};

use fuser::{FileAttr, FileType, ReplyAttr, ReplyData, ReplyEntry, ReplyXattr, Request};
use libc::{EACCES, EINVAL, EIO, ENETUNREACH, ENODATA, ENOENT, ENOTDIR, ETIMEDOUT};

const NIX_EXECUTABLE: &str = "nix";
const NIXPKGS: &str = "<nixpkgs>";
/// How long cached directory listings and resolved store paths remain valid.
const CACHE_TTL: Duration = Duration::from_mins(5); // 5 minutes

fn make_attr(inode: u64, kind: FileType) -> FileAttr {
    let (perm, nlink) = match kind {
        FileType::Directory => (0o555, 2),
        _ => (0o444, 1),
    };
    FileAttr {
        ino: inode,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink,
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
        /// When this store path was last resolved (or last attempted).
        created: Instant,
        /// Whether to resolve via srcOnly (unpack source) instead of nix-build --attr.
        src_only: bool,
        /// Error message from the last failed build attempt.  None on success or untried.
        error: Option<String>,
    },
    /// A Nix attribute set — appears as a directory.
    Dir {
        /// Dotted attr path, e.g. "python3Packages".
        attr_path: String,
    },
}

struct Entry {
    kind: EntryKind,
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

/// What kind of Nix attribute exists at a given dotted path.
enum AttrKind {
    /// The attribute is a derivation.
    Derivation,
    /// The attribute is an attr set (i.e. a directory).
    Directory,
}

/// Runs `nix eval --raw -f '<nixpkgs>' '<attr_path>.outPath'`.
/// Evaluates the derivation (no build) — fast, but the resulting store path
/// may not exist yet if the derivation hasn't been built or substituted.
/// Used in `lookup` for existence checking.
fn nix_eval_attr(attr_path: &str) -> io::Result<AttrKind> {
    let expr = format!("{attr_path}.outPath");
    eprintln!("Evaluating: {expr:?} from {NIXPKGS:?}");
    let output = std::process::Command::new(NIX_EXECUTABLE)
        .arg("eval")
        .arg("--raw")
        .arg("-f")
        .arg(NIXPKGS)
        .arg(&expr)
        .output()
        .map_err(|e| {
            eprintln!("Failed to spawn nix: {e}");
            io::Error::new(io::ErrorKind::Other, format!("spawn nix: {e}"))
        })?;
    if output.status.success() {
        Ok(AttrKind::Derivation)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        eprintln!("nix_eval_attr failed (status {}): {stderr}", output.status);
        // If nix eval failed because it's a set, treat as a directory.
        // Two patterns:
        //   - "value is a set"  (old nix versions)
        //   - "attribute 'outpath' in selection path '...outpath' not found"
        //     (newer nix — means the attr exists but isn't a derivation)
        if stderr.contains("value is a set") || stderr.contains("'outpath' in selection path") {
            Ok(AttrKind::Directory)
        } else {
            Err(io::Error::from_raw_os_error(classify_eval_error(&stderr)))
        }
    }
}

/// Shared helper: spawns `nix-build --no-out-link` with extra arguments,
/// returns the trimmed store path or an errno.
fn nix_build(extra_args: &[&str]) -> io::Result<String> {
    let output = std::process::Command::new("nix-build")
        .arg("--no-out-link")
        .args(extra_args)
        .output()
        .map_err(|e| {
            eprintln!("Failed to spawn nix-build: {e}");
            io::Error::new(io::ErrorKind::Other, format!("spawn nix-build: {e}"))
        })?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map(|s| s.trim_end_matches('\n').to_string())
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("nix-build non-UTF-8 output: {e}"),
                )
            })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        eprintln!("nix_build failed: {stderr}");
        Err(io::Error::from_raw_os_error(classify_eval_error(&stderr)))
    }
}

/// Runs `nix-build --no-out-link --attr <attr_path> <nixpkgs>` to actually
/// build (or substitute) the derivation. Returns the store path on success,
/// or an errno on failure. Used in `readlink` so the symlink target exists.
fn nix_build_attr(attr_path: &str) -> io::Result<String> {
    eprintln!("Building: {attr_path:?} from {NIXPKGS:?}");
    nix_build(&["--attr", attr_path, NIXPKGS])
}

/// Runs `nix-build --no-out-link --expr 'with import <nixpkgs> {}; srcOnly { src = <attr_path>; }'`.
/// Unpacks a source archive (with patches applied) via nixpkgs' srcOnly.
/// Returns the store path to the unpacked source directory.
fn nix_build_src_only(attr_path: &str) -> io::Result<String> {
    let expr = format!(
        "with import <nixpkgs> {{}}; srcOnly {{ name = {attr_path}.name; src = {attr_path}; }}"
    );
    eprintln!("Building srcOnly: {attr_path:?}");
    nix_build(&["--expr", &expr])
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
        // Allow '@unpacked' suffix for extended operations.
        let (child_name, src_only) = if let Some(base) = child_name.strip_suffix("@unpacked") {
            if base.is_empty() || base.starts_with('.') || base.ends_with('.') {
                reply.error(EINVAL);
                return;
            }
            (base, true)
        } else {
            if child_name.starts_with('.') || child_name.ends_with('.') {
                reply.error(EINVAL);
                return;
            }
            (child_name, false)
        };
        eprintln!(
            "Lookup: {child_name:?} in parent {parent}{}",
            if src_only { " [src_only]" } else { "" }
        );

        // Resolve parent attr path for non-root lookups.
        let parent_attr = if parent == 1 {
            None
        } else {
            let Some(parent_entry) = self.entries.get(&parent) else {
                reply.error(ENOENT);
                return;
            };
            let parent_path = if let EntryKind::Dir { attr_path, .. } = &parent_entry.kind {
                attr_path.as_str()
            } else {
                reply.error(ENOTDIR);
                return;
            };
            Some(parent_path.to_string())
        };

        // Build the full dotted attr path (without @suffix) for nix eval/building.
        let child_path = if let Some(ref parent_path) = parent_attr {
            format!("{parent_path}.{child_name}")
        } else {
            child_name.to_string()
        };

        // Inode must include the @unpacked suffix (if any) for uniqueness,
        // so hash the original name, not the stripped child_name.
        let orig_name = name.to_str().unwrap();
        let full_inode_path = if let Some(ref parent_path) = parent_attr {
            format!("{parent_path}.{orig_name}")
        } else {
            orig_name.to_string()
        };
        let inode = inode_for_attr_path(&full_inode_path);

        // If we already have an entry, just reply with it.
        if let Some(entry) = self.entries.get(&inode) {
            let attr = match &entry.kind {
                EntryKind::Symlink { .. } => make_attr(inode, FileType::Symlink),
                EntryKind::Dir { .. } => make_attr(inode, FileType::Directory),
            };
            reply.entry(&Duration::MAX, &attr, 0);
            return;
        }

        match nix_eval_attr(&child_path) {
            Ok(AttrKind::Derivation) => {
                // Create a stub — the actual build happens lazily in readlink
                // so the symlink target is guaranteed to exist when accessed.
                reply.entry(&Duration::MAX, &make_attr(inode, FileType::Symlink), 0);
                self.entries.insert(
                    inode,
                    Entry {
                        kind: EntryKind::Symlink {
                            attr_path: child_path,
                            out_path: None, // built on first readlink
                            created: Instant::now(),
                            src_only,
                            error: None,
                        },
                    },
                );
            }
            Ok(AttrKind::Directory) => {
                reply.entry(&Duration::MAX, &make_attr(inode, FileType::Directory), 0);
                self.entries.insert(
                    inode,
                    Entry {
                        kind: EntryKind::Dir {
                            attr_path: child_path,
                        },
                    },
                );
            }
            Err(e) => {
                reply.error(e.raw_os_error().unwrap_or(EIO));
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&Duration::MAX, &make_attr(1, FileType::Directory));
            return;
        }
        if let Some(entry) = self.entries.get(&ino) {
            let attr = match &entry.kind {
                EntryKind::Symlink { .. } => make_attr(ino, FileType::Symlink),
                EntryKind::Dir { .. } => make_attr(ino, FileType::Directory),
            };
            reply.attr(&Duration::MAX, &attr);
            return;
        }
        reply.error(ENOENT);
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        if let Some(entry) = self.entries.get_mut(&inode) {
            match &mut entry.kind {
                EntryKind::Symlink {
                    attr_path,
                    out_path,
                    created,
                    src_only,
                    error,
                } => {
                    // Resolve if untried (no out_path yet, no error recorded)
                    // or the last attempt is stale.
                    let need_resolve =
                        (out_path.is_none() && error.is_none()) || created.elapsed() > CACHE_TTL;
                    if need_resolve {
                        let result = if *src_only {
                            nix_build_src_only(attr_path)
                        } else {
                            nix_build_attr(attr_path)
                        };
                        match result {
                            Ok(path) => {
                                *created = Instant::now();
                                *out_path = Some(path);
                                *error = None;
                            }
                            Err(e) => {
                                *created = Instant::now();
                                *error = Some(e.to_string());
                            }
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
        // Directories are always empty — Nix attribute discovery is not
        // provided via readdir.  Packages are resolved only through explicit
        // lookup + readlink (e.g.  ls -l /nixfs/vim).
        let parent_inode = if ino == 1 {
            1
        } else if let Some(Entry {
            kind: EntryKind::Dir { attr_path },
            ..
        }) = self.entries.get(&ino)
        {
            attr_path.rsplit_once('.').map_or(1, |(parent_path, _)| {
                if parent_path.is_empty() {
                    1
                } else {
                    inode_for_attr_path(parent_path)
                }
            })
        } else {
            reply.error(ENOTDIR);
            return;
        };

        let entries = [
            (ino, FileType::Directory, "."),
            (parent_inode, FileType::Directory, ".."),
        ];
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap
        )]
        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
    }

    fn forget(&mut self, _req: &Request, ino: u64, _nlookup: u64) {
        self.entries.remove(&ino);
    }

    fn getxattr(&mut self, _req: &Request, ino: u64, name: &OsStr, _size: u32, reply: ReplyXattr) {
        let Some(name_str) = name.to_str() else {
            reply.error(ENODATA);
            return;
        };
        if name_str != "user.error" {
            reply.error(ENODATA);
            return;
        }
        let msg = match self.entries.get(&ino) {
            Some(Entry {
                kind:
                    EntryKind::Symlink {
                        error: Some(msg), ..
                    },
            }) => msg.clone(),
            _ => {
                reply.error(ENODATA);
                return;
            }
        };
        reply.data(msg.as_bytes());
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, _size: u32, reply: ReplyXattr) {
        let has_error = matches!(
            self.entries.get(&ino),
            Some(Entry {
                kind: EntryKind::Symlink { error: Some(_), .. },
            })
        );
        if has_error {
            reply.data(b"user.error\0");
        } else {
            reply.data(b"");
        }
    }
}

fn main() {
    use fuser::MountOption;
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "Usage: {} [mountpoint]\n\
             \n\
             Mount Nix package attributes as a FUSE filesystem.\n\
             \n\
             If no mountpoint is given, defaults to /nixfs.\n",
            args.first().map_or("nixfs", String::as_str)
        );
        std::process::exit(0);
    }

    let mount_path = args.get(1).map_or("/nixfs", String::as_str);
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
        eprintln!("Failed to mount {mount_path}: {e}");
        std::process::exit(1);
    }
}
