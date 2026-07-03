# AGENTS.md

AI agent guidance for the nixfs-rs project. Keep this file updated with each commit.

## Project summary

**nixfs-rs** is a FUSE filesystem that maps Nix package attributes to virtual symlinks.
Mount at `/nixfs` (or any path), then access e.g. `/nixfs/vim` to get a symlink
pointing to the Nix store path of `<nixpkgs>.vim`.

- Nix tooling required at runtime: `nix`, `nix-build`
- See `Cargo.toml` for Rust edition, dependencies, and binary layout.

## Architecture

### Data flow (current)

```
user command         FUSE op            nixfs action
─────────────────────────────────────────────────────────
ls -l /nixfs/vim     lookup("vim",1)     nix_eval_attr → insert Entry (Dir or Symlink stub)
                      readlink(inode)    nix_build_attr → cache store path, reply symlink target
cat /nixfs/vim/...   (follows link)     Nix daemon builds if needed (outside nixfs)
ls /nixfs/           readdir(1)         returns only "." and ".." (directories are empty)
ls /nixfs/python3/   readdir(dir_inode) same — explicit lookup required to see children
```

### Key types

- **`NixFS`** — holds `HashMap<u64, Entry>` keyed by inode (hash of full dotted attr path).
- **`Entry`** — `Dir { attr_path }` or `Symlink { attr_path, out_path }`.
  Symlink `out_path` is `None` for stub entries created by `lookup` (resolved lazily in `readlink`).
- **Inode scheme:** `DefaultHasher` over the full dotted attr path → deterministic 64-bit inode.
- **Root:** inode 1, always a `Dir`. All lookups target `<nixpkgs>` (hardcoded).

### Nix commands used

| Command | Purpose | Triggers build? |
|---|---|---|
| `nix eval --raw -f '<nixpkgs>' '<attr>.outPath'` | Existence check + type detection (lookup) | No |
| `nix-build --no-out-link --attr <attr> <nixpkgs>` | Build/substitute derivation → store path (readlink) | Yes |

### Path resolution

Filenames are used directly as Nixpkgs attribute names. No path manipulation needed.

| Input | lookup resolves | readlink resolves |
|---|---|---|
| `vim` | `nix eval --raw -f '<nixpkgs>' 'vim.outPath'` | `nix-build --no-out-link --attr vim <nixpkgs>` |
| `python3Packages.numpy` | `nix eval --raw -f '<nixpkgs>' 'python3Packages.numpy.outPath'` | `nix-build --no-out-link --attr python3Packages.numpy <nixpkgs>` |

## Build & test

Build with `cargo build --release`. Runtime (requires root for `/nixfs`, or pass a user-writable mountpoint):

```bash
./target/release/nixfs /tmp/nixfs &    # mount
ls -l /tmp/nixfs/vim                   # test lookup + readlink
fusermount -u /tmp/nixfs               # unmount
```

## Style notes

- Single file for now; modules planned.
- `eprintln!` used for debug logging (visible on stderr of the mount process).
- No async runtime — FUSE ops are synchronous and single-threaded.
