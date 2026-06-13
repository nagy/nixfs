use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, ReplyAttr, ReplyData, ReplyEntry, Request};
use libc::{EIO, ENOENT};

const NIX_BUILD_EXECUTABLE: &str = "nix-build";
const NIX_ENV_EXECUTABLE: &str = "nix-env";
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
    nixpath: String,
    attr: String,
    /// The resolved Nix store path. None until first readlink triggers nix-build.
    out_path: Option<String>,
}

#[derive(Default)]
struct NixFS {
    entries: std::collections::HashMap<u64, Entry>,
}

/// Cheap existence check: runs `nix-env -qa --json -f '<nixpath>' -A <attr>`.
/// Returns true if the attribute exists (exit code 0), false otherwise.
/// This does NOT evaluate or build the derivation — just checks the name.
fn nix_attr_exists(nixpath: &str, attr: &str) -> bool {
    eprintln!("Checking existence: {attr:?} in {nixpath:?}");
    match std::process::Command::new(NIX_ENV_EXECUTABLE)
        .arg("-qa")
        .arg("--json")
        .arg("-f")
        .arg(nixpath)
        .arg("-A")
        .arg(attr)
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Runs `nix-build --no-out-link --attr <attr> <nixpath>` to actually resolve
/// the store path. This triggers builds if the derivation is not already in the
/// Nix store. Returns the store path on success.
fn nix_build_outpath(nixpath: &str, attr: &str) -> Option<String> {
    eprintln!("Building: {attr:?} from {nixpath:?}");
    let output = std::process::Command::new(NIX_BUILD_EXECUTABLE)
        .arg("--no-out-link")
        .arg(nixpath)
        .arg("--attr")
        .arg(attr)
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
        // Cheap existence check — does not build anything.
        if nix_attr_exists(&nixpath, &attr) {
            reply.entry(&Duration::MAX, &make_symlink_attr(hashinode), 0);
            self.entries.insert(
                hashinode,
                Entry {
                    nixpath,
                    attr,
                    out_path: None, // resolved lazily in readlink
                },
            );
        } else {
            reply.error(ENOENT);
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
        if let Some(entry) = self.entries.get_mut(&inode) {
            // Use cached path if already resolved, otherwise build now.
            if entry.out_path.is_none() {
                entry.out_path = nix_build_outpath(&entry.nixpath, &entry.attr);
            }
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
