use fuser::{
    FileAttr, FileType, MountOption, ReplyAttr, ReplyData, ReplyEntry, ReplyXattr, Request,
};
use libc::{EACCES, EINVAL, EIO, ENETUNREACH, ENODATA, ENOENT, ENOTDIR, ERANGE, ETIMEDOUT};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, UNIX_EPOCH};

const NIX_EXECUTABLE: &str = "nix";
const NIXPKGS: &str = "<nixpkgs>";
/// How long cached directory listings and resolved store paths remain valid.
const CACHE_TTL: Duration = Duration::from_mins(5); // 5 minutes
/// Upper bound on cached entries; the oldest are evicted (FIFO) beyond this.
const MAX_ENTRIES: usize = 10_000;

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
        /// Last failed build: (errno, message).  None on success or untried.
        /// `readlink` replies the errno; the `user.error` xattr shows the message.
        error: Option<(i32, String)>,
    },
    /// A Nix attribute set — appears as a directory.
    Dir {
        /// Dotted attr path, e.g. "python3Packages".
        attr_path: String,
    },
}

/// Shared cache: entries, FIFO eviction order, and in-flight resolution
/// slots. Everything is behind one mutex; blocking nix subprocesses must
/// never run while it is held.
struct Cache {
    entries: HashMap<u64, EntryKind>,
    /// Insertion order of `entries` (back = newest), for FIFO eviction.
    order: VecDeque<u64>,
    /// In-flight symlink resolutions: inode -> completion slot. The builder
    /// fills the slot, waiters block on the condvar instead of rebuilding.
    inflight: HashMap<u64, Arc<Slot>>,
}

/// Completion slot for one in-flight resolution: the result once known.
type Slot = (Mutex<Option<BuildResult>>, Condvar);

/// Outcome of resolving a symlink target.
type BuildResult = Result<String, NixError>;

struct NixFS {
    cache: Arc<Mutex<Cache>>,
    /// Nixpkgs expression to resolve attributes from (--nixpkgs, default <nixpkgs>).
    nixpkgs: String,
}

fn lock_cache(cache: &Arc<Mutex<Cache>>) -> MutexGuard<'_, Cache> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

impl NixFS {
    fn new(nixpkgs: String) -> Self {
        Self {
            cache: Arc::new(Mutex::new(Cache {
                entries: HashMap::new(),
                order: VecDeque::new(),
                inflight: HashMap::new(),
            })),
            nixpkgs,
        }
    }
}

impl Cache {
    /// Insert a new entry, evicting the oldest entries beyond `MAX_ENTRIES`.
    /// Callers must only insert inodes not already in `entries`.
    fn insert_entry(&mut self, inode: u64, entry: EntryKind) {
        self.entries.insert(inode, entry);
        self.order.push_back(inode);
        while self.entries.len() > MAX_ENTRIES {
            let Some(evict) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&evict);
        }
    }

    /// Remove an entry from the cache and the eviction order.
    fn remove_entry(&mut self, inode: u64) {
        self.entries.remove(&inode);
        self.order.retain(|&i| i != inode);
    }
}

/// Resolve a symlink entry, deduplicating concurrent resolutions of the same
/// inode. Runs on a worker thread off the FUSE request loop; the caller's
/// `build` closure must not touch the cache.
fn resolve_symlink<F>(cache: &Arc<Mutex<Cache>>, inode: u64, build: F) -> BuildResult
where
    F: FnOnce(bool, &str) -> BuildResult + Send,
{
    // Snapshot what the builder needs and join-or-create the completion slot.
    let (attr_path, src_only) = {
        let cache = lock_cache(cache);
        let Some(EntryKind::Symlink {
            attr_path,
            src_only,
            ..
        }) = cache.entries.get(&inode)
        else {
            return Err(NixError {
                errno: ENOENT,
                message: "entry evicted while resolving".to_string(),
            });
        };
        (attr_path.clone(), *src_only)
    };
    let (slot, is_builder) = {
        let mut cache = lock_cache(cache);
        match cache.inflight.entry(inode) {
            Entry::Vacant(entry) => {
                let slot = Arc::new((Mutex::new(None), Condvar::new()));
                entry.insert(slot.clone());
                (slot, true)
            }
            Entry::Occupied(entry) => (entry.get().clone(), false),
        }
    };

    let result = if is_builder {
        let resolved = build(src_only, &attr_path);
        let (lock, condvar) = &*slot;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = Some(resolved.clone());
        condvar.notify_all();
        resolved
    } else {
        let (lock, condvar) = &*slot;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while guard.is_none() {
            guard = condvar.wait(guard).unwrap_or_else(PoisonError::into_inner);
        }
        guard.as_ref().expect("woken only when filled").clone()
    };

    // Record the outcome in the cache and drop the slot (builder only).
    let mut cache = lock_cache(cache);
    if let Some(EntryKind::Symlink {
        out_path,
        created,
        error,
        ..
    }) = cache.entries.get_mut(&inode)
    {
        *created = Instant::now();
        match &result {
            Ok(path) => {
                *out_path = Some(path.clone());
                *error = None;
            }
            Err(err) => {
                *out_path = None;
                *error = Some((err.errno, err.message.clone()));
            }
        }
    }
    if is_builder {
        cache.inflight.remove(&inode);
    }
    result
}

// FNV-1a 64-bit: deterministic across processes and remounts, unlike
// DefaultHasher (which is randomly seeded per-process).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hash an attribute path to a deterministic 64-bit inode.
const fn inode_for_attr_path(attr_path: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    let bytes = attr_path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Note: `as u64`, not `u64::from` — From is not yet const-stable.
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

// Compile-time invariants of the inode scheme (mirror of the runtime tests):
// inode 0 is reserved in FUSE, inode 1 is the root, and the @unpacked
// suffix must change the inode. A violation fails the build.
const _: () = assert!(
    inode_for_attr_path("") != 0,
    "empty path must not hash to inode 0"
);
const _: () = assert!(
    inode_for_attr_path("vim") != 1,
    "paths must not collide with root"
);
const _: () = assert!(
    inode_for_attr_path("qemu.src") != inode_for_attr_path("qemu.src@unpacked"),
    "@unpacked suffix must change the inode"
);

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
fn nix_eval_attr(attr_path: &str, nixpkgs: &str) -> Result<AttrKind, NixError> {
    let expr = format!("{attr_path}.outPath");
    eprintln!("Evaluating: {expr:?} from {nixpkgs:?}");
    let output = std::process::Command::new(NIX_EXECUTABLE)
        .arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("eval")
        .arg("--raw")
        .arg("-f")
        .arg(nixpkgs)
        .arg(&expr)
        .output()
        .map_err(|e| {
            eprintln!("Failed to spawn nix: {e}");
            NixError {
                errno: EIO,
                message: format!("spawn nix: {e}"),
            }
        })?;
    if output.status.success() {
        Ok(AttrKind::Derivation)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("nix_eval_attr failed (status {}): {stderr}", output.status);
        // If nix eval failed because it's a set, treat as a directory
        // (classify_nix_stderr returns None for that case).
        classify_nix_stderr(&stderr).map_or(Ok(AttrKind::Directory), Err)
    }
}

/// Shared helper: spawns `nix-build --no-out-link` with extra arguments,
/// returns the trimmed store path or an errno.
fn nix_build(extra_args: &[&str]) -> Result<String, NixError> {
    let output = std::process::Command::new("nix-build")
        .arg("--no-out-link")
        .args(extra_args)
        .output()
        .map_err(|e| {
            eprintln!("Failed to spawn nix-build: {e}");
            NixError {
                errno: EIO,
                message: format!("spawn nix-build: {e}"),
            }
        })?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map(|s| s.trim_end_matches('\n').to_string())
            .map_err(|e| NixError {
                errno: EIO,
                message: format!("nix-build non-UTF-8 output: {e}"),
            })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("nix_build failed: {stderr}");
        Err(classify_nix_stderr(&stderr).unwrap_or_else(|| {
            // classify_nix_stderr returns None only for the "is a set" case,
            // which can't happen here (nix-build fails differently).
            NixError {
                errno: EIO,
                message: stderr.trim().to_string(),
            }
        }))
    }
}
/// Runs `nix-build --no-out-link --attr <attr_path> <nixpkgs>` to actually
/// build (or substitute) the derivation. Returns the store path on success,
/// or an errno on failure. Used in `readlink` so the symlink target exists.
fn nix_build_attr(attr_path: &str, nixpkgs: &str) -> Result<String, NixError> {
    eprintln!("Building: {attr_path:?} from {nixpkgs:?}");
    nix_build(&["--attr", attr_path, nixpkgs])
}

/// Runs `nix-build --no-out-link --expr 'with import <nixpkgs> {}; srcOnly { src = <attr_path>; }'`.
/// Unpacks a source archive (with patches applied) via nixpkgs' srcOnly.
/// Returns the store path to the unpacked source directory.
fn nix_build_src_only(attr_path: &str, nixpkgs: &str) -> Result<String, NixError> {
    let expr = format!(
        "with import {nixpkgs} {{}}; srcOnly {{ name = {attr_path}.name; src = {attr_path}; }}"
    );
    eprintln!("Building srcOnly: {attr_path:?}");
    nix_build(&["--expr", &expr])
}

/// Classified result of a failed `nix eval`/`nix-build` invocation:
/// the errno to reply to FUSE, plus the actual stderr message.
#[derive(Debug, PartialEq, Clone)]
struct NixError {
    errno: i32,
    message: String,
}

/// Maps `nix eval`/`nix-build` stderr to a specific errno.
fn classify_eval_error(stderr: &str) -> i32 {
    // Match the final `error:` line, not build-log noise: bare words like
    // "network" appear in unrelated messages (e.g. network namespaces).
    if stderr.contains("does not provide attribute")
        // Parenthesized: && binds tighter than || in this chain.
        || (stderr.contains("attribute '") && stderr.contains("' missing"))
        || stderr.contains("not found")
        || stderr.contains("does not exist")
        // `nix-build --expr '… srcOnly { … }'` reports a missing attr as
        // an undefined variable rather than "not found".
        || stderr.contains("undefined variable")
    {
        ENOENT
    } else if stderr.contains("timed out") || stderr.contains("timeout") {
        ETIMEDOUT
    } else if stderr.contains("could not resolve")
        || stderr.contains("connection refused")
        || stderr.contains("name or service not known")
        // Full errno strings (ENETUNREACH / EHOSTUNREACH) instead of the
        // bare "network"/"unreachable": those match unrelated build noise.
        || stderr.contains("network is unreachable")
        || stderr.contains("network unreachable")
        || stderr.contains("no route to host")
        || stderr.contains("host is unreachable")
        || stderr.contains("host unreachable")
    {
        ENETUNREACH
    } else if stderr.contains("permission denied") || stderr.contains("access denied") {
        EACCES
    } else {
        EIO
    }
}

/// Returns a `NixError` classified from `stderr`, or `None` if the stderr
/// indicates a *successful* evaluation of a non-derivation (an attr set),
/// which `nix_eval_attr` must treat as a directory.
fn classify_nix_stderr(stderr: &str) -> Option<NixError> {
    let message = stderr.trim().to_string();
    // If nix eval failed because it's a set, treat as a directory:
    //   - "value is a set"  (old nix versions)
    //   - "attribute 'outPath' in selection path '...outPath' not found"
    //     (modern nix — means the attr exists but isn't a derivation)
    let is_directory = stderr.contains("value is a set")
        || stderr.contains("attribute 'outPath' in selection path")
        || stderr.contains("'outpath' in selection path");
    if is_directory {
        None
    } else {
        Some(NixError {
            errno: classify_eval_error(stderr),
            message,
        })
    }
}

/// How to answer an xattr request for a payload of `len` bytes against the
/// kernel's `size` probe, per the FUSE xattr protocol.
#[derive(Debug, PartialEq)]
enum XattrReply {
    /// size == 0: reply the total length so the caller can allocate.
    Size,
    /// The payload fits: send it.
    Data,
    /// The payload does not fit: reply ERANGE.
    Range,
}

fn xattr_outcome(size: u32, len: usize) -> XattrReply {
    if size == 0 {
        XattrReply::Size
    } else if len > size as usize {
        XattrReply::Range
    } else {
        XattrReply::Data
    }
}

/// Reply with `data` honoring the kernel's xattr size protocol.
fn reply_xattr(size: u32, data: &[u8], reply: ReplyXattr) {
    match xattr_outcome(size, data.len()) {
        XattrReply::Size => reply.size(u32::try_from(data.len()).unwrap_or(u32::MAX)),
        XattrReply::Data => reply.data(data),
        XattrReply::Range => reply.error(ERANGE),
    }
}

/// Whether `name` is a valid Nix attr-path: non-empty dot-separated segments,
/// each starting with an ASCII alphanumeric or `_` and continuing with
/// alphanumeric, `_`, `'` or `-`. Matches every attr found in the measured
/// nixpkgs sets (27,772 top-level, 11,791 python3Packages, 19,523
/// haskellPackages, incl. digit-leading names like `2captcha`) while
/// rejecting junk (spaces, `@`, `;`, `}`, quotes, empty segments) before any
/// nix subprocess is spawned.
fn is_valid_attr_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name.split('.').all(|seg| {
            let mut chars = seg.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '\'' | '-'))
        })
}

impl fuser::Filesystem for NixFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        // Reject non-UTF-8 names — Nix attr names are always valid UTF-8.
        let Some(orig_name) = name.to_str() else {
            reply.error(EINVAL);
            return;
        };
        // Reject names that look like dotfiles — invalid as Nix attr names.
        // Allow '@unpacked' suffix for extended operations.
        let (child_name, src_only) = if let Some(base) = orig_name.strip_suffix("@unpacked") {
            (base, true)
        } else {
            (orig_name, false)
        };
        // Validate after stripping suffix: strict allowlist so junk names
        // never reach a nix subprocess (misleading ENOENT + eval cost).
        if !is_valid_attr_name(child_name) {
            reply.error(EINVAL);
            return;
        }
        eprintln!(
            "Lookup: {child_name:?} in parent {parent}{}",
            if src_only { " [src_only]" } else { "" }
        );

        // Resolve parent attr path for non-root lookups.
        let parent_attr = {
            let cache = lock_cache(&self.cache);
            if parent == 1 {
                None
            } else {
                let Some(parent_entry) = cache.entries.get(&parent) else {
                    reply.error(ENOENT);
                    return;
                };
                let parent_path = if let EntryKind::Dir { attr_path, .. } = parent_entry {
                    attr_path.as_str()
                } else {
                    reply.error(ENOTDIR);
                    return;
                };
                Some(parent_path.to_string())
            }
        };

        // Build the full dotted attr path (without @suffix) for nix eval/building.
        let child_path = if let Some(ref parent_path) = parent_attr {
            format!("{parent_path}.{child_name}")
        } else {
            child_name.to_string()
        };

        // Inode must include the @unpacked suffix (if any) for uniqueness,
        // so hash the original name, not the stripped child_name.
        let full_inode_path = if let Some(ref parent_path) = parent_attr {
            format!("{parent_path}.{orig_name}")
        } else {
            orig_name.to_string()
        };
        let inode = inode_for_attr_path(&full_inode_path);

        // If we already have an entry, just reply with it.
        {
            let cache = lock_cache(&self.cache);
            if let Some(entry) = cache.entries.get(&inode) {
                let attr = match entry {
                    EntryKind::Symlink { .. } => make_attr(inode, FileType::Symlink),
                    EntryKind::Dir { .. } => make_attr(inode, FileType::Directory),
                };
                reply.entry(&CACHE_TTL, &attr, 0);
                return;
            }
        }

        match nix_eval_attr(&child_path, &self.nixpkgs) {
            Ok(AttrKind::Derivation) => {
                // Create a stub — the actual build happens lazily in readlink
                // so the symlink target is guaranteed to exist when accessed.
                reply.entry(&CACHE_TTL, &make_attr(inode, FileType::Symlink), 0);
                lock_cache(&self.cache).insert_entry(
                    inode,
                    EntryKind::Symlink {
                        attr_path: child_path,
                        out_path: None, // built on first readlink
                        created: Instant::now(),
                        src_only,
                        error: None,
                    },
                );
            }
            Ok(AttrKind::Directory) => {
                reply.entry(&CACHE_TTL, &make_attr(inode, FileType::Directory), 0);
                lock_cache(&self.cache).insert_entry(
                    inode,
                    EntryKind::Dir {
                        attr_path: child_path,
                    },
                );
            }
            Err(e) => {
                reply.error(e.errno);
                // No entry is inserted on failure, so the message cannot be
                // surfaced via the user.error xattr: the path does not exist,
                // so getfattr never reaches getxattr. It is only in the
                // daemon log above (nix_eval_attr eprintln).
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&CACHE_TTL, &make_attr(1, FileType::Directory));
            return;
        }
        let cache = lock_cache(&self.cache);
        if let Some(entry) = cache.entries.get(&ino) {
            let attr = match entry {
                EntryKind::Symlink { .. } => make_attr(ino, FileType::Symlink),
                EntryKind::Dir { .. } => make_attr(ino, FileType::Directory),
            };
            reply.attr(&CACHE_TTL, &attr);
            return;
        }
        reply.error(ENOENT);
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        let cache = self.cache.clone();
        let nixpkgs = self.nixpkgs.clone();

        // Fast path: resolved (or failed) recently — reply from the cache
        // without spawning a thread.
        {
            let cache = lock_cache(&cache);
            match cache.entries.get(&inode) {
                None => {
                    reply.error(ENOENT);
                    return;
                }
                Some(EntryKind::Dir { .. }) => {
                    reply.error(EINVAL);
                    return;
                }
                Some(EntryKind::Symlink {
                    out_path,
                    created,
                    error,
                    ..
                }) => {
                    let fresh =
                        (out_path.is_some() || error.is_some()) && created.elapsed() <= CACHE_TTL;
                    if fresh {
                        // Build failed — surface the real errno instead of a
                        // blanket EIO (message via the user.error xattr).
                        if let Some(path) = out_path {
                            reply.data(path.as_bytes());
                            return;
                        }
                        if let Some((errno, _)) = error {
                            reply.error(*errno);
                            return;
                        }
                        reply.error(EIO);
                        return;
                    }
                }
            }
        }

        // Slow path: resolve off the request loop so a multi-minute build
        // never stalls other operations. Deduplication happens inside
        // resolve_symlink (in-flight slot per inode).
        std::thread::spawn(move || {
            let result = resolve_symlink(&cache, inode, |src_only, attr_path| {
                if src_only {
                    nix_build_src_only(attr_path, &nixpkgs)
                } else {
                    nix_build_attr(attr_path, &nixpkgs)
                }
            });
            match result {
                Ok(path) => reply.data(path.as_bytes()),
                Err(err) => reply.error(err.errno),
            }
        });
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
        let parent_inode = {
            let cache = lock_cache(&self.cache);
            if ino == 1 {
                1
            } else if let Some(EntryKind::Dir { attr_path }) = cache.entries.get(&ino) {
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
            }
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
        lock_cache(&self.cache).remove_entry(ino);
    }

    fn getxattr(&mut self, _req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(name_str) = name.to_str() else {
            reply.error(ENODATA);
            return;
        };
        if name_str != "user.error" {
            reply.error(ENODATA);
            return;
        }
        let msg = {
            let cache = lock_cache(&self.cache);
            if let Some(EntryKind::Symlink {
                error: Some((_, msg)),
                ..
            }) = cache.entries.get(&ino)
            {
                msg.clone()
            } else {
                reply.error(ENODATA);
                return;
            }
        };
        reply_xattr(size, msg.as_bytes(), reply);
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        let has_error = {
            let cache = lock_cache(&self.cache);
            matches!(
                cache.entries.get(&ino),
                Some(EntryKind::Symlink { error: Some(_), .. })
            )
        };
        let data: &[u8] = if has_error { b"user.error\0" } else { b"" };
        reply_xattr(size, data, reply);
    }
}

/// Parsed command line.
#[derive(Debug, PartialEq)]
struct Cli {
    mount_path: String,
    nixpkgs: String,
    action: CliAction,
}

/// What the process should do, derived from the parsed command line.
#[derive(Debug, PartialEq)]
enum CliAction {
    Mount,
    Help,
    Version,
}

/// Parse command-line arguments (argv[1..]). Usage errors are returned as a
/// message; the caller prints usage and exits 2.
fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut mount_path = None;
    let mut nixpkgs = NIXPKGS.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                return Ok(Cli {
                    mount_path: "/nixfs".to_string(),
                    nixpkgs,
                    action: CliAction::Help,
                });
            }
            "--version" => {
                return Ok(Cli {
                    mount_path: "/nixfs".to_string(),
                    nixpkgs,
                    action: CliAction::Version,
                });
            }
            "--nixpkgs" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("option '--nixpkgs' requires an argument".to_string());
                };
                nixpkgs.clone_from(value);
            }
            _ => {
                if let Some(value) = args[i].strip_prefix("--nixpkgs=") {
                    if value.is_empty() {
                        return Err("option '--nixpkgs' requires an argument".to_string());
                    }
                    nixpkgs = value.to_string();
                } else if args[i].starts_with('-') && args[i].len() > 1 {
                    return Err(format!("unknown option '{}'", args[i]));
                } else if mount_path.is_some() {
                    return Err(format!("unexpected extra argument '{}'", args[i]));
                } else {
                    mount_path = Some(args[i].clone());
                }
            }
        }
        i += 1;
    }
    Ok(Cli {
        mount_path: mount_path.unwrap_or_else(|| "/nixfs".to_string()),
        nixpkgs,
        action: CliAction::Mount,
    })
}

/// Verify runtime prerequisites before mounting: `nix` and `nix-build` must
/// be runnable, and `nix eval` must work against the configured nixpkgs.
/// This also proves the `nix-command` experimental feature, which nixfs
/// requests explicitly via `--extra-experimental-features`.
fn preflight(nixpkgs: &str) -> Result<(), String> {
    for tool in [NIX_EXECUTABLE, "nix-build"] {
        let output = std::process::Command::new(tool)
            .arg("--version")
            .output()
            .map_err(|e| format!("{tool} not runnable: {e} (is it on PATH?)"))?;
        if !output.status.success() {
            return Err(format!(
                "{tool} --version failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    let output = std::process::Command::new(NIX_EXECUTABLE)
        .arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("eval")
        .arg("--raw")
        .arg("-f")
        .arg(nixpkgs)
        .arg("system")
        .output()
        .map_err(|e| format!("{NIX_EXECUTABLE} not runnable: {e} (is it on PATH?)"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "nix eval against {nixpkgs} failed: {stderr}\n\
             Hint: check NIX_PATH (or pass --nixpkgs) and that the nix-command feature is available."
        ));
    }
    Ok(())
}

fn print_usage(program: &str) {
    eprintln!(
        "Usage: {program} [OPTIONS] [MOUNTPOINT]\n\n\
         Mount Nix package attributes as a FUSE filesystem.\n\n\
         Options:\n  --nixpkgs EXPR   resolve attributes from EXPR (default: <nixpkgs>)\n  \
         -h, --help       show this help and exit\n  --version        print version and exit\n\n\
         If no mountpoint is given, defaults to /nixfs.\n"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "nixfs".to_string());

    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("{program}: {err}");
            print_usage(&program);
            std::process::exit(2);
        }
    };
    match cli.action {
        CliAction::Help => {
            print_usage(&program);
            std::process::exit(0);
        }
        CliAction::Version => {
            println!("{program} {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        CliAction::Mount => {
            if let Err(err) = preflight(&cli.nixpkgs) {
                eprintln!("{program}: preflight failed: {err}");
                std::process::exit(1);
            }
        }
    }

    if let Err(e) = fuser::mount2(
        NixFS::new(cli.nixpkgs),
        &cli.mount_path,
        &[
            MountOption::RO,
            MountOption::FSName("nixfs".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
        ],
    ) {
        eprintln!("Failed to mount {}: {e}", cli.mount_path);
        eprintln!(
            "Hint: make sure {} exists and is not already mounted (try `fusermount3 -u {}`).",
            cli.mount_path, cli.mount_path
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_eval_error_maps_real_nix_stderr() {
        // Missing attribute (nix eval / nix-build --attr)
        assert_eq!(
            classify_eval_error(
                "error: attribute 'noSuchAttr' in selection path 'noSuchAttr.outPath' not found"
            ),
            ENOENT
        );
        // Missing attribute via srcOnly path (undefined variable)
        assert_eq!(
            classify_eval_error("error: undefined variable 'noSuchAttr'"),
            ENOENT
        );
        // Network unreachable
        assert_eq!(
            classify_eval_error("error: network is unreachable"),
            ENETUNREACH
        );
        // Permission denied
        assert_eq!(classify_eval_error("error: permission denied"), EACCES);
        // Unknown failure → generic EIO
        assert_eq!(classify_eval_error("error: some random failure"), EIO);
    }

    #[test]
    fn classify_eval_error_ignores_bare_words_in_noise() {
        // Bare "network"/"unreachable" must NOT match unrelated build noise
        assert_eq!(
            classify_eval_error("error: executing 'make' failed (network of the build sandbox)"),
            EIO
        );
        assert_eq!(classify_eval_error("error: foo unreachable bar"), EIO);
        // "timeout" still matches
        assert_eq!(classify_eval_error("error: timed out"), ETIMEDOUT);
    }

    #[test]
    fn classify_nix_stderr_distinguishes_directory() {
        // A set (non-derivation) → None (nix_eval_attr treats it as a directory)
        assert_eq!(
            classify_nix_stderr(
                "error: attribute 'outPath' in selection path 'python3Packages.outPath' not found"
            ),
            None
        );
        // A real error → Some with classified errno + raw stderr message
        let err = classify_nix_stderr(
            "error: attribute 'noSuchAttr' in selection path 'noSuchAttr.outPath' not found",
        )
        .expect("missing attr should be an error, not a directory");
        assert_eq!(err.errno, ENOENT);
        assert!(err.message.contains("noSuchAttr"));
        assert!(err.message.contains("not found"));
    }

    #[test]
    fn inode_for_attr_path_is_deterministic_and_distinct() {
        let a = inode_for_attr_path("vim");
        let b = inode_for_attr_path("vim");
        assert_eq!(a, b, "same path must hash to same inode");

        let c = inode_for_attr_path("python3Packages.numpy");
        assert_ne!(a, c, "different paths must hash to different inodes");

        // Parent/child paths differ (dot separator), so distinct inodes
        assert_ne!(inode_for_attr_path("python3Packages"), c);

        // The '@unpacked' suffix must change the inode (lookup hashes the
        // full name incl. suffix for uniqueness)
        assert_ne!(
            inode_for_attr_path("qemu.src"),
            inode_for_attr_path("qemu.src@unpacked")
        );
    }

    #[test]
    fn inode_for_attr_path_avoids_zero() {
        // Inode 0 is invalid in FUSE (reserved); the empty string must not hash to it
        assert_ne!(inode_for_attr_path(""), 0);
        // Non-empty real paths also must not collide with root (inode 1 is root)
        assert_ne!(inode_for_attr_path("vim"), 1);
    }

    #[test]
    fn parse_args_defaults_to_nixfs_and_nixpkgs() {
        let cli = parse_args(&[]).unwrap();
        assert_eq!(cli.action, CliAction::Mount);
        assert_eq!(cli.mount_path, "/nixfs");
        assert_eq!(cli.nixpkgs, NIXPKGS);
    }

    #[test]
    fn parse_args_accepts_mountpoint_and_nixpkgs() {
        let cli = parse_args(&[
            "/tmp/mnt".to_string(),
            "--nixpkgs".to_string(),
            "/custom".to_string(),
        ])
        .unwrap();
        assert_eq!(cli.mount_path, "/tmp/mnt");
        assert_eq!(cli.nixpkgs, "/custom");

        let cli = parse_args(&["--nixpkgs=/custom".to_string()]).unwrap();
        assert_eq!(cli.nixpkgs, "/custom");

        let cli = parse_args(&["--nixpkgs=/custom".to_string(), "/tmp/mnt".to_string()]).unwrap();
        assert_eq!(cli.mount_path, "/tmp/mnt");
        assert_eq!(cli.nixpkgs, "/custom");
    }

    #[test]
    fn parse_args_handles_help_and_version() {
        assert_eq!(
            parse_args(&["--version".to_string()]).unwrap().action,
            CliAction::Version
        );
        assert_eq!(
            parse_args(&["-h".to_string()]).unwrap().action,
            CliAction::Help
        );
        assert_eq!(
            parse_args(&["/mnt".to_string(), "--help".to_string()])
                .unwrap()
                .action,
            CliAction::Help
        );
    }

    #[test]
    fn parse_args_rejects_unknown_and_extra_args() {
        assert!(parse_args(&["--bogus".to_string()]).is_err());
        assert!(parse_args(&["-x".to_string()]).is_err());
        assert!(parse_args(&["/mnt".to_string(), "/extra".to_string()]).is_err());
        assert!(parse_args(&["--nixpkgs".to_string()]).is_err());
        assert!(parse_args(&["--nixpkgs=".to_string()]).is_err());
    }

    #[test]
    fn xattr_outcome_implements_size_protocol() {
        assert_eq!(xattr_outcome(0, 123), XattrReply::Size);
        assert_eq!(xattr_outcome(0, 0), XattrReply::Size);
        assert_eq!(xattr_outcome(123, 123), XattrReply::Data);
        assert_eq!(xattr_outcome(1, 0), XattrReply::Data);
        assert_eq!(xattr_outcome(100, 123), XattrReply::Range);
    }

    #[test]
    fn is_valid_attr_name_accepts_real_nix_attrs() {
        for name in [
            "vim",
            "python3Packages.numpy",
            "qemu.src",
            "haskellPackages.2captcha", // digit-leading: reachable, must stay
            "3d-graphics-examples",
            "foo_bar-baz'qux",
            "a",
        ] {
            assert!(is_valid_attr_name(name), "{name:?} must be accepted");
        }
    }

    #[test]
    fn is_valid_attr_name_rejects_junk() {
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "foo.",
            "foo..bar",
            "x} ; id",
            "foo bar",
            "@unpacked",
            "foo@unpacked@unpacked",
            "foo;bar",
            "foo(bar)",
            "a\"b",
            "-dash",
            "'quote",
            "π",
            "a b.c",
        ] {
            assert!(!is_valid_attr_name(name), "{name:?} must be rejected");
        }
    }

    #[test]
    fn insert_entry_evicts_oldest_beyond_cap() {
        let fs = NixFS::new(NIXPKGS.to_string());
        let total = MAX_ENTRIES + 50;
        {
            let mut cache = lock_cache(&fs.cache);
            for i in 0..total {
                cache.insert_entry(
                    i as u64,
                    EntryKind::Dir {
                        attr_path: format!("attr{i}"),
                    },
                );
            }
            assert_eq!(cache.entries.len(), MAX_ENTRIES, "map must stay at the cap");
            assert_eq!(cache.order.len(), MAX_ENTRIES, "order must mirror the map");
            // Oldest 50 evicted, newest present.
            assert!(!cache.entries.contains_key(&0));
            assert!(!cache.entries.contains_key(&49));
            assert!(cache.entries.contains_key(&(MAX_ENTRIES as u64 + 49)));
            // remove_entry (what forget calls) drops it from both structures.
            let inode = MAX_ENTRIES as u64 + 49;
            cache.remove_entry(inode);
            assert!(!cache.entries.contains_key(&inode));
            assert!(!cache.order.contains(&inode));
        }
    }

    #[test]
    fn resolve_symlink_dedups_concurrent_builds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fs = NixFS::new(NIXPKGS.to_string());
        {
            let mut cache = lock_cache(&fs.cache);
            cache.entries.insert(
                42,
                EntryKind::Symlink {
                    attr_path: "foo".to_string(),
                    out_path: None,
                    created: Instant::now().checked_sub(Duration::from_hours(1)).unwrap(), // stale
                    src_only: false,
                    error: None,
                },
            );
        }
        let builds = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let cache = fs.cache.clone();
            let builds = builds.clone();
            handles.push(std::thread::spawn(move || {
                let result = resolve_symlink(&cache, 42, |_, _| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    Ok("/nix/store/foo".to_string())
                });
                assert_eq!(result, Ok("/nix/store/foo".to_string()));
            }));
        }
        for handle in handles {
            handle.join().expect("worker must not panic");
        }
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "four concurrent readlinks must trigger one build"
        );
        let cache = lock_cache(&fs.cache);
        assert!(
            matches!(
                cache.entries.get(&42),
                Some(EntryKind::Symlink { out_path: Some(path), error: None, .. })
                    if path == "/nix/store/foo"
            ),
            "cache must record the resolved path"
        );
        assert!(
            cache.inflight.is_empty(),
            "completion slot must be cleaned up"
        );
    }

    #[test]
    fn resolve_symlink_records_failure_and_cleans_slot() {
        let fs = NixFS::new(NIXPKGS.to_string());
        {
            let mut cache = lock_cache(&fs.cache);
            cache.entries.insert(
                7,
                EntryKind::Symlink {
                    attr_path: "bar".to_string(),
                    out_path: None,
                    created: Instant::now().checked_sub(Duration::from_hours(1)).unwrap(),
                    src_only: false,
                    error: None,
                },
            );
        }
        let result = resolve_symlink(&fs.cache, 7, |_, _| {
            Err(NixError {
                errno: EIO,
                message: "build boom".to_string(),
            })
        });
        assert_eq!(
            result,
            Err(NixError {
                errno: EIO,
                message: "build boom".to_string()
            })
        );
        let cache = lock_cache(&fs.cache);
        assert!(
            matches!(
                cache.entries.get(&7),
                Some(EntryKind::Symlink { out_path: None, error: Some((EIO, msg)), .. })
                    if msg == "build boom"
            ),
            "failure must be recorded for the user.error xattr"
        );
        assert!(cache.inflight.is_empty());
    }

    #[test]
    fn resolve_symlink_returns_enoent_for_evicted_entry() {
        let fs = NixFS::new(NIXPKGS.to_string());
        let result = resolve_symlink(&fs.cache, 99, |_, _| Ok("/nix/store/x".to_string()));
        assert_eq!(result.unwrap_err().errno, ENOENT);
        assert!(lock_cache(&fs.cache).inflight.is_empty());
    }
}
