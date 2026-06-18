# AGENTS.md

AI agent guidance for the nixfs-rs project. Keep this file updated with each commit.

## Project summary

**nixfs-rs** is a FUSE filesystem that maps Nix package attributes to virtual symlinks.
Mount at `/nixfs` (or any path), then access e.g. `/nixfs/vim` to get a symlink
pointing to the Nix store path of `<nixpkgs>.vim`.

- Nix tooling required at runtime: `nix` (for `nix eval`)
- See `Cargo.toml` for Rust edition, dependencies, and binary layout.

## Architecture

### Data flow (current)

```
user command         FUSE op            nixfs action
─────────────────────────────────────────────────────────
ls /nixfs/           readdir(1)         nix_list_directory("") → caches in root Entry
                     (per child)        inserts stub Entry (Dir or Symlink) in HashMap
ls /nixfs/vim        lookup("vim",1)     checks HashMap → found stub → reply entry
                     getattr(inode)      returns symlink attrs from stub
                      readlink(inode)    lazy: nix_eval_attr → caches out_path in stub
ls /nixfs/python3/   readdir(dir_inode) nix_list_directory("python3Packages")
                                        → caches & inserts stubs
cat /nixfs/vim/...   (follows link)     Nix daemon builds if needed (outside nixfs)
```

### Key types

- **`NixFS`** — holds `HashMap<u64, Entry>` keyed by inode (hash of full dotted attr path).
- **`Entry`** — `Dir { attr_path, children }` or `Symlink { attr_path, out_path }`.
  Symlink `out_path` is `None` for stub entries created by `readdir` (resolved lazily in `readlink`).
- **Inode scheme:** `DefaultHasher` over the full dotted attr path → deterministic 64-bit inode.
- **Root:** inode 1, stores its own `Dir` entry with cached children after first `readdir`.
  All lookups target `<nixpkgs>` (hardcoded).

### Nix commands used

| Command | Purpose | Triggers build? |
|---|---|---|
| `nix eval --raw -f '<nixpkgs>' '<attr>.outPath'` | Resolve store path (derivations) | No |
| `nix eval --impure --json --expr 'builtins.mapAttrs ... pkgs.<path>'` | List directory children | No |

Both use `builtins.tryEval` where needed to handle broken packages. `--impure` is
only used for `--expr` (required for `import <nixpkgs>` in newer Nix); `-f` works
in pure mode.

### Path parsing

Filenames are used directly as Nixpkgs attribute names. No path manipulation needed.

| Input | Resolves to |
|---|---|
| `vim` | `nix eval --raw -f '<nixpkgs>' 'vim.outPath'` |
| `python3Packages.numpy` | `nix eval --raw -f '<nixpkgs>' 'python3Packages.numpy.outPath'` |

## Recommendations status

See `RECOMMENDATIONS.md` for details on pending items.

Already implemented: #1 (directories), #2 (cached paths), #4 (removed `_` prefix),
#6 (error types), #7 (eval vs build), #8 (no unwrap).

Pending: #3 (cache TTL), #5 (non-blocking), #9 (modules), #10 (CLI).

## Build & test

Build with `cargo build --release`. Runtime (requires root for `/nixfs`, or pass a user-writable mountpoint):

```bash
./target/release/nixfs /tmp/nixfs &    # mount
ls -l /tmp/nixfs/vim                   # test lookup + readlink
fusermount -u /tmp/nixfs               # unmount
```

## Style notes

- Single file for now; splitting into modules is recommendation #9.
- `eprintln!` used for debug logging (visible on stderr of the mount process).
- No async runtime — FUSE ops are synchronous and single-threaded (see recommendation #5).
- `#[allow(dead_code)]` on `Entry.attr` — retained for cache invalidation (recommendation #3).

## Commit conventions

Prefix commits with the recommendation number when applicable, e.g.:
```
Implement recommendation #4: fix path parsing
```
