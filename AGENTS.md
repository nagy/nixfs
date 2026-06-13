# AGENTS.md

AI agent guidance for the nixfs-rs project. Keep this file updated with each commit.

## Project summary

**nixfs-rs** is a FUSE filesystem that maps Nix package attributes to virtual symlinks.
Mount at `/nixfs` (or any path), then access e.g. `/nixfs/vim` to get a symlink
pointing to the Nix store path of `<nixpkgs>.vim`.

- Language: Rust (edition 2024)
- Single file: `nixfs.rs` (binary, not a library)
- Dependencies: `fuser` 0.15.1 (FUSE), `libc` 0.2.174 (errno constants)
- Nix tooling required at runtime: `nix` (for `nix eval`)

## Architecture

### Data flow (post-recommendation #7)

```
user command         FUSE op            nixfs action
─────────────────────────────────────────────────────────
ls /nixfs/           readdir            returns "." and ".." (flat fs for now)
ls /nixfs/vim        lookup("vim")      nix eval --raw -f '<nixpkgs>' 'vim.outPath'
                                        → cache result in Entry.out_path
                                        → create inode (hash of nixpath+attr)
ls -l /nixfs/vim     getattr(inode)     return symlink attrs from cache
                      readlink(inode)    return cached out_path (no subprocess)
cat /nixfs/vim/...   (follows link)     Nix daemon builds if needed (outside nixfs)
```

### Key types

- **`NixFS`** — holds `HashMap<u64, Entry>` keyed by inode (hash of nixpath+attr).
- **`Entry`** — stores `nixpath`, `attr`, and `out_path: Option<String>` (the resolved store path).
- **Inode scheme:** `DefaultHasher` over `(nixpath, attr)` → deterministic 64-bit inode.
- **Root inode:** fixed at `1` (a directory).

### Nix command used

| Command | Purpose | Triggers build? |
|---|---|---|
| `nix eval --raw -f '<nixpath>' '<attr>.outPath'` | Resolve store path | No (evaluation only) |

`nix-build` is no longer used in the hot path (removed in commit 1e4bc4d).

### Path parsing (`_` prefix convention)

| Input | nixpath | attr |
|---|---|---|
| `hello` | `<nixpkgs>` | `hello` |
| `_foo_bar` | `<foo>` | `bar` |

Collected in `split_nixpath_from_attr`. Known issues: `.unwrap()` panic on malformed input,
underscores in attr names collide with delimiter (recommendation #4).

## Recommendations status

See `RECOMMENDATIONS.md` for full details. Implemented items:

| # | Recommendation | Status |
|---|---|---|
| 7 | Separate existence checks from builds | ✅ Done |
| 2 | Store resolved paths in cache | ✅ Done (as part of #7) |

Not yet implemented: #1, #3, #4, #5, #6, #8, #9, #10.

## Build & test

```bash
cargo check          # fast compile check
cargo build          # debug build
cargo build --release
```

Runtime (requires root for `/nixfs`, or pass a user-writable mountpoint):

```bash
cargo build --release
./target/release/nixfs /tmp/nixfs &    # mount
ls -l /tmp/nixfs/vim                   # test lookup + readlink
fusermount -u /tmp/nixfs               # unmount
```

## Style notes

- Single file for now; splitting into modules is recommendation #9.
- `eprintln!` used for debug logging (visible on stderr of the mount process).
- No async runtime — FUSE ops are synchronous and single-threaded (see recommendation #5).
- `#[allow(dead_code)]` on `Entry.nixpath` and `Entry.attr` — retained for cache invalidation (recommendation #3).

## Commit conventions

Prefix commits with the recommendation number when applicable, e.g.:
```
Implement recommendation #4: fix path parsing
```
