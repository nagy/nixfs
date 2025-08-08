use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyEntry, Request,
};

use libc::ENOENT;
use memoize::memoize;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

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
        perm: 0o644,
        nlink: 1,
        uid: 1000,
        gid: 100,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

#[derive(Default)]
struct HelloFS {
    hashmap: HashMap<u64, String>,
}

#[memoize]
fn nix_attr_to_outpath(attr: String) -> String {
    eprintln!("execute: {:?}", attr);
    let output = Command::new("nix-build")
        .arg("--no-out-link")
        .arg("<nixpkgs>")
        .arg("-A")
        .arg(attr)
        .output()
        .unwrap();
    let stdout: String = String::from_utf8(output.stdout)
        .unwrap()
        .strip_suffix("\n")
        .unwrap()
        .to_string();
    stdout
}

impl Filesystem for HelloFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        eprintln!("Lookup: {:?}", name);
        MEMOIZED_MAPPING_NIX_ATTR_TO_OUTPATH.with_borrow(|v| {
            eprintln!("storeident:: {:?}", v);
        });
        if parent != 1 {
            reply.error(ENOENT);
            return;
        }
        if !name.to_str().unwrap_or("").starts_with("_eval") {
            reply.error(ENOENT);
            return;
        }
        let attr = name.to_str().unwrap().strip_prefix("_eval").unwrap();
        eprintln!("inserting attr: {:?}", attr);
        let hashinode = {
            let mut hasher = DefaultHasher::new();
            attr.hash(&mut hasher);
            hasher.finish()
        };
        reply.entry(&TTL, &make_symlink_attr(hashinode), 0);
        self.hashmap.insert(hashinode, attr.to_string());
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        const HELLO_DIR_ATTR: FileAttr = FileAttr {
            ino: 1,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH, // 1970-01-01 00:00:00
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: 501,
            gid: 20,
            rdev: 0,
            flags: 0,
            blksize: 512,
        };
        /* parent */
        if ino == 1 {
            reply.attr(&TTL, &HELLO_DIR_ATTR);
            return;
        }
        if let Some(_) = self.hashmap.get(&ino) {
            reply.attr(&TTL, &make_symlink_attr(ino));
            return;
        }
        reply.error(ENOENT);
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        if let Some(found) = self.hashmap.get(&inode) {
            let found2 = nix_attr_to_outpath(found.to_string());
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
        let entries = vec![
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
    fuser::mount2(
        HelloFS::default(),
        "/tmp/t10/nixfs",
        &vec![
            MountOption::RO,
            MountOption::FSName("hello".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
        ],
    )
    .unwrap();
}
