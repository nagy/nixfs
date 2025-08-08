use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};

use libc::ENOENT;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(30);

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
    hashmap: HashMap<(u64, String), String>,
    counter: u64,
}

impl Filesystem for HelloFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        eprintln!("Just some lookup: {:?}", name);
        if parent == 1 {
            if name.to_str().unwrap_or("").starts_with("_eval") {
                let attr = name.to_str().unwrap().strip_prefix("_eval").unwrap();
                eprintln!("inserting, Just calc: {:?}", attr);
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

                eprintln!("result: {:?}", stdout);
                self.hashmap
                    .insert((self.counter, attr.to_string()), stdout);
                self.counter += 1;
                eprintln!("result2: {:?}", self.hashmap);
            }

            for (key, _) in self.hashmap.iter() {
                if &key.1 == name.to_str().unwrap_or("") {
                    reply.entry(&TTL, &make_symlink_attr(key.0), 0);
                    return;
                }
            }
        }
        reply.error(ENOENT);
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
        match ino {
            1 => reply.attr(&TTL, &HELLO_DIR_ATTR),
            // 2 => reply.attr(&TTL, &HELLO_TXT_ATTR),
            3 => reply.attr(&TTL, &make_symlink_attr(3)),
            _ => reply.error(ENOENT),
        }
    }

    fn readlink(&mut self, _req: &Request, inode: u64, reply: ReplyData) {
        for (key, path) in self.hashmap.iter() {
            if key.0 == inode {
                reply.data(path.as_bytes());
                return;
            }
        }
        reply.error(ENOENT);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != 1 {
            reply.error(ENOENT);
            return;
        }

        let mut entries: Vec<(u64, FileType, String)> = vec![
            // let mut entries = vec![
            (1, FileType::Directory, ".".to_string()),
            (1, FileType::Directory, "..".to_string()),
            // (2, FileType::RegularFile, "hello.txt"),
            // (3, FileType::Symlink, "python3Hy"),
        ];
        for (key, _) in self.hashmap.iter() {
            let val = format!("_eval{}", key.1);
            entries.push((key.0, FileType::Symlink, val.to_string()));
        }

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
