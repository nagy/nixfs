use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};

use lazy_static::lazy_static;
use libc::ENOENT;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(30);

// const HELLO_TXT_CONTENT: &str = "Hello World!\n";

// const HELLO_TXT_ATTR: FileAttr = FileAttr {
//     ino: 2,
//     size: 13,
//     blocks: 1,
//     atime: UNIX_EPOCH, // 1970-01-01 00:00:00
//     mtime: UNIX_EPOCH,
//     ctime: UNIX_EPOCH,
//     crtime: UNIX_EPOCH,
//     kind: FileType::RegularFile,
//     perm: 0o644,
//     nlink: 1,
//     uid: 501,
//     gid: 20,
//     rdev: 0,
//     flags: 0,
//     blksize: 512,
// };

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

lazy_static! {
    static ref HASH_MAP: Arc<Mutex<HashMap<(u64, String), String>>> = {
        let map = HashMap::new();
        // map.insert(
        //     (3, "python3Hy"),
        //     "/nix/store/m76fm92nar23bc6fnpwgwkiiikzlkvrj-python3-3.13.5-env",
        // );
        Arc::new(Mutex::new(map))
    };
    static ref COUNTER: Arc<Mutex<u64>> = {
        let counter = 3;
        Arc::new(Mutex::new(counter))
    };
}

struct HelloFS;

impl Filesystem for HelloFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        eprintln!("Just some lookup: {:?}", name);
        if parent == 1 {
            let mut hm = HASH_MAP.lock().unwrap();
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
                let mut cntr = COUNTER.lock().unwrap();
                hm.insert((*cntr, attr.to_string()), stdout);
                *cntr += 1;
                eprintln!("result2: {:?}", hm);
            }

            for (key, _) in hm.iter() {
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
        let hm = HASH_MAP.lock().unwrap();
        for (key, path) in hm.iter() {
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
        let hm = HASH_MAP.lock().unwrap();
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
        for (key, _) in hm.iter() {
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
        HelloFS,
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
