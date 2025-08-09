use fuser::{FileAttr, FileType, ReplyAttr, ReplyData, ReplyEntry, Request};

use libc::ENOENT;
use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, UNIX_EPOCH};

// TODO make whole NIX_PATH accessible

const NIX_BUILD_EXECUTABLE: &'static str = "nix-build";

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

#[derive(Default)]
struct NixFS {
    hashmap: std::collections::HashMap<u64, String>,
}

#[memoize::memoize]
fn nix_attr_to_outpath(attr: String) -> Option<String> {
    eprintln!("execute: {:?}", attr);
    let output = std::process::Command::new(NIX_BUILD_EXECUTABLE)
        .arg("--no-out-link")
        .arg("<nixpkgs>")
        .arg("--attr")
        .arg(attr)
        .output();
    if output.is_err() {
        return None;
    }
    match output {
        Ok(output2) => {
            if output2.status.success() {
                let stdout: String = String::from_utf8(output2.stdout)
                    .unwrap()
                    .strip_suffix("\n")
                    .unwrap()
                    .to_string();
                Some(stdout)
            } else {
                return None;
            }
        }
        Err(_) => None,
    }
}

impl fuser::Filesystem for NixFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        // skip some know non-existing values
        if name.to_str().unwrap_or("").starts_with(".") {
            reply.error(ENOENT);
            return;
        }
        if name.to_str().unwrap_or("").ends_with(".") {
            reply.error(ENOENT);
            return;
        }
        eprintln!("Lookup: {:?}", name);
        let name = name.to_str().unwrap();
        // MEMOIZED_MAPPING_NIX_ATTR_TO_OUTPATH.with_borrow(|v| {
        //     eprintln!("storeident:: {:?}", v);
        // });
        if parent != 1 {
            reply.error(ENOENT);
            return;
        }
        let attr = name;
        eprintln!("inserting attr: {:?}", attr);
        let hashinode = {
            let mut hasher = DefaultHasher::new();
            attr.hash(&mut hasher);
            hasher.finish()
        };
        let output = nix_attr_to_outpath(attr.to_string());
        match output {
            Some(_) => {
                reply.entry(&Duration::MAX, &make_symlink_attr(hashinode), 0);
                self.hashmap.insert(hashinode, attr.to_string());
            }
            None => {
                reply.error(ENOENT);
                return;
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
        if let Some(_) = self.hashmap.get(&ino) {
            reply.attr(&Duration::MAX, &make_symlink_attr(ino));
            return;
        }
        reply.error(ENOENT);
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        if let Some(found) = self.hashmap.get(&inode) {
            let found2 = nix_attr_to_outpath(found.to_string()).unwrap();
            reply.data(found2.as_bytes());
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
    let mount_path = &args[1];
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
