# AGENTS.md

AI agent guidance for the nixfs-rs project. Keep this file updated with each commit.

## Project summary

**nixfs-rs** is a FUSE filesystem that maps Nix package attributes to virtual symlinks.
Mount at `/nixfs` (or any path), then access e.g. `/nixfs/vim` to get a symlink
pointing to the Nix store path of `<nixpkgs>.vim`.

- Nix tooling required at runtime: `nix`, `nix-build`
- `fusermount3` (from `fuse3`) required at runtime for mounting — fuser uses the pure-rust mount backend (no libfuse; `default-features = false` in Cargo.toml)
- `nixfs` passes `--extra-experimental-features nix-command` to `nix eval` itself and runs a mount-time preflight (`nix`/`nix-build` on PATH, `nix eval` working against the configured nixpkgs) — no user `nix.conf` configuration needed
- See `Cargo.toml` for Rust edition, dependencies, and binary layout.

## Architecture

### Data flow (current)

```
user command              FUSE op            nixfs action
──────────────────────────────────────────────────────────────────
ls -l /nixfs/vim          lookup("vim",1)     nix_eval_attr → insert Entry (Dir or Symlink stub)
                           readlink(inode)    worker thread: nix_build_attr → cache store path, reply symlink target
cat /nixfs/vim/...        (follows link)     Nix daemon builds if needed (outside nixfs)
ls /nixfs/                readdir(1)         returns only "." and ".." (directories are empty)
ls /nixfs/python3/        readdir(dir_inode) same — explicit lookup required to see children
ls -l /nixfs/qemu.src@unpacked  lookup("qemu.src@unpacked",1)  strip @unpacked suffix, nix_eval_attr on base
                           readlink(inode)    worker thread: nix_build_src_only → unpack via pkgs.srcOnly
```

### Key types

- **`NixFS`** — holds `Arc<Mutex<Cache>>`; `Cache` = `HashMap<u64, Entry>` keyed by inode (hash of full
  dotted attr path) + FIFO eviction order (`MAX_ENTRIES`) + in-flight resolution slots.
- **`Entry`** — `Dir { attr_path }` or `Symlink { attr_path, out_path, created, src_only, error }`.
  Symlink `out_path` is `None` for stub entries created by `lookup` (resolved lazily in `readlink`).
  `src_only` is `true` when the filename ends in `@unpacked`, meaning `readlink` resolves via `pkgs.srcOnly` instead of `nix-build --attr`.
  `error` is `None` on success/untried; `Some((errno, msg))` after a failed build (retried after `CACHE_TTL`).
  `readlink` replies `errno` (instead of a generic `EIO`); `getxattr user.error` shows `msg`.
- **Concurrency:** the FUSE request loop is single-threaded, but `readlink` resolution (`nix-build`, can
  take minutes) runs on worker threads off the loop — the mount never stalls on a build. Concurrent
  `readlink`s of the same inode deduplicate via an in-flight slot (`resolve_symlink`); the cache mutex
  is never held across a subprocess wait.
- **Inode scheme:** FNV-1a 64-bit hash over the full dotted attr path → stable 64-bit inode (deterministic across processes/remounts, unlike `DefaultHasher`).
- **Root:** inode 1, always a `Dir`. All lookups target `<nixpkgs>` (hardcoded).

### Nix commands used

| Command | Purpose | Triggers build? |
|---|---|---|
| `nix eval --raw -f '<nixpkgs>' '<attr>.outPath'` | Existence check + type detection (lookup) | No |
| `nix-build --no-out-link --attr <attr> <nixpkgs>` | Build/substitute derivation → store path (readlink) | Yes |
| `nix-build --no-out-link --expr '… srcOnly { name = <attr>.name; src = <attr>; }'` | Unpack source archive (readlink for @unpacked entries) | Yes |

### Path resolution

Filenames are used directly as Nixpkgs attribute names. No path manipulation needed.
Names are validated against a strict allowlist before any `nix` invocation: every
dot-separated segment must match `[A-Za-z0-9_][A-Za-z0-9_'-]*` (measured against
39k+ nixpkgs attrs, incl. digit-leading `haskellPackages.2captcha`); junk names get
`EINVAL` without spawning `nix`. The `@unpacked` suffix is stripped before validation.

| Input | lookup resolves | readlink resolves |
|---|---|---|
| `vim` | `nix eval --raw -f '<nixpkgs>' 'vim.outPath'` | `nix-build --no-out-link --attr vim <nixpkgs>` |
| `python3Packages.numpy` | `nix eval --raw -f '<nixpkgs>' 'python3Packages.numpy.outPath'` | `nix-build --no-out-link --attr python3Packages.numpy <nixpkgs>` |
| `qemu.src@unpacked` | `nix eval --raw -f '<nixpkgs>' 'qemu.src.outPath'` | `nix-build --expr '… srcOnly { name = qemu.src.name; src = qemu.src; }'` |

## Build & test

Build with `cargo build --release`. Runtime (requires root for `/nixfs`, or pass a user-writable mountpoint):

```bash
./target/release/nixfs /tmp/nixfs &    # mount
ls -l /tmp/nixfs/vim                   # test lookup + readlink
fusermount -u /tmp/nixfs               # unmount
```

CLI: `nixfs [OPTIONS] [MOUNTPOINT]` — options: `--nixpkgs EXPR` (default `<nixpkgs>`), `-h/--help`, `--version` (prints the crate version). Unknown options / extra args exit 2 with usage; mount failures exit 1 with a `fusermount3 -u` hint. Parsing is hand-rolled in `parse_args` (tested) — switch to clap only if subcommands or `-o` options arrive.

### Nix build

```bash
nix-build --expr 'let pkgs = import <nixpkgs> {}; in pkgs.callPackage ./default.nix {}'
```

`versionCheckHook` runs `nixfs --version` during installCheck and requires the package `version` string in its output — keep `default.nix`'s `version` in sync with `Cargo.toml` (both `0.1.0`).

### NixOS VM test

```bash
nix-build --expr 'let pkgs = import <nixpkgs> {}; in pkgs.callPackage ./default.nix {}' \
  -A passthru.tests.nixfs
```

Runs nixfs in a QEMU VM: mounts `/tmp/mnt`, resolves `hello`, verifies symlink + binary output, unmounts.

## Style notes

- Single file for now; modules planned.
- `eprintln!` used for debug logging (visible on stderr of the mount process).
- No async runtime. The FUSE request loop is single-threaded; `readlink`'s
  `nix-build` runs on worker threads off the loop (never hold the cache mutex
  across a subprocess wait), so builds cannot stall the mount.
- Unit tests for `classify_eval_error`, `classify_nix_stderr`, `inode_for_attr_path`,
  `parse_args`, `is_valid_attr_name`, cache eviction, and `resolve_symlink` (incl.
  concurrent dedup) live in a `#[cfg(test)] mod tests` at the bottom of `nixfs.rs`;
  run with `cargo test`.
## Future investigation

- **Nix daemon protocol instead of subprocesses.** Every `lookup` spawns `nix eval`, every `readlink` spawns `nix-build`. Talking to the Nix daemon socket directly (or using a crate) would eliminate fork/exec overhead and give structured error handling instead of scraping stderr. Investigate `nix-sys` or similar.
- **Surface build errors to users (done).** The `error` field stashes `(errno, msg)`; `readlink` replies the errno (no more blanket `EIO`), and `getxattr`/`listxattr` expose the message via `user.error`. `getxattr`/`listxattr` implement the kernel size protocol (`reply_xattr`/`xattr_outcome`: size-0 probe → `reply.size`, payload too large → `ERANGE`), so `getfattr` works. Nix helpers return `Result<_, NixError>` carrying both the errno and the actual stderr text (so `user.error` shows the real Nix output, not "Unknown error N"). `classify_eval_error` matches specific errno phrases (not bare words like "network") and recognizes `undefined variable` for missing attrs via the srcOnly path. Remaining: optionally symlink to a synthetic error file for users without `getfattr`.
- **Bounded cache with eviction — implemented (2026-08-08).** `entries` is capped at `MAX_ENTRIES` (10,000) with FIFO eviction: a `VecDeque` insertion-order queue, `insert_entry` pushes and pops the front when over the cap, `remove_entry` (called by `forget`) keeps both in sync. `reply.entry`/`reply.attr` now use the finite `CACHE_TTL` so the kernel expires idle dentries and calls `forget` (previously `Duration::MAX` meant it never did). Known tradeoff: the cap can evict an inode the kernel still references → transient `ENOENT` on `getattr` until the next path-based re-lookup. Upgrade to LRU by moving to the back on access instead of just on insert.
- **readdir with attrNames — REJECTED (2026-08-08), do not resurrect.** `builtins.attrNames` is pure eval (verified: 27,772 top-level attrs in ~0.2 s, store unchanged; 11,791 in `python3Packages`) — readdir itself builds nothing. The proposal fails because readdir cannot control what the caller does with entries: any tool that resolves symlinks (`ls -l`, `find -L`, `realpath`) fires `readlink` → `nix-build` per entry, i.e. up to 27,772 builds for a root listing. There is no FUSE-level way to distinguish plain `ls` from `ls -l`; `out_path=None` stubs only move the storm to first resolution. Pinned design: readdir stays `.`/`..` only; discovery is explicit `lookup`. If discovery is ever needed it belongs in a CLI query tool (`nix eval --json 'builtins.attrNames …'`), not FUSE readdir. Rationale + measurements: `RECOMMENDATIONS.org`, "Decided — readdir stays minimal".
