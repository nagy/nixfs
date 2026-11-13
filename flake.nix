{
  description = "FUSE filesystem exposing Nix package attributes as virtual symlinks (crane)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      crane,
      treefmt-nix,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ treefmt-nix.flakeModule ];
      systems = [ "x86_64-linux" ];

      perSystem =
        {
          system,
          pkgs,
          lib,
          config,
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;

          # Pure Rust: fuser uses the pure-rust mount backend
          # (default-features = false), so nothing is linked at build time.
          libInputs = [ ];

          # fusermount3 (fuse3) is spawned at mount time; nix/nix-build are
          # spawned per lookup/readlink. Propagated so the nixfs closure
          # carries them.
          runtimeInputs = [
            pkgs.fuse3
            pkgs.nix
          ];

          # Source as seen by cargo: everything tracked by git.
          src = craneLib.cleanCargoSource ./.;

          # Cargo.lock has no network access during the build; fetch deps
          # first, then feed them to cargo through the registry cache.
          cargoArtifacts = craneLib.buildDepsOnly {
            inherit src;
          };

          pkg = craneLib.buildPackage {
            inherit src cargoArtifacts;
            propagatedBuildInputs = runtimeInputs;

            # versionCheckHook runs `nixfs --version` in installCheck and
            # greps the Cargo.toml version string; keep versions in sync.
            nativeInstallCheckInputs = [ pkgs.versionCheckHook ];
            doInstallCheck = true;
            meta.description = "FUSE filesystem exposing Nix package attributes as virtual symlinks";
          };

          # Static rustdoc HTML (what `cargo doc` writes to ./target/doc/)
          # installed under $out/share/doc. Reuses the same cargoArtifacts,
          # so only doc crates compile; --no-deps (crane's default) keeps
          # third-party crates out of the search index. Browse offline:
          # nix run nixpkgs#python3 -- -m http.server -d <doc-out>/share/doc
          docs = craneLib.cargoDoc {
            inherit src cargoArtifacts;
            meta.description = "nixfs API documentation";
          };

          # Doctests: code blocks in doc comments compiled and run against
          # the library (`cargo test --doc`), same artifact set as above.
          doctests = craneLib.cargoDocTest {
            inherit src cargoArtifacts;
            meta.description = "nixfs doctests";
          };

          # NixOS VM test. Self-reference is lazy: forcing `outPath` never
          # forces passthru, so the test can install the final package.
          nixos-lib = import (pkgs.path + "/nixos/lib") { inherit (pkgs) lib; };
          vmTest =
            nixfsPkg:
            # `lib.addMetaAttrs` attaches the description: the nixosTest
            # `meta` submodule has no `description` option.
            lib.addMetaAttrs { description = "nixfs NixOS VM test"; } (
              nixos-lib.runTest {
                hostPkgs = pkgs;

                name = "nixfs";

                nodes.machine =
                  { pkgs, ... }:
                  {
                    environment.systemPackages = [
                      nixfsPkg
                      pkgs.hello
                      pkgs.fuse3
                    ];
                    boot.kernelModules = [ "fuse" ];
                    nix.settings.experimental-features = [ "nix-command" ];
                    nix.nixPath = [ "nixpkgs=${pkgs.path}" ];
                    virtualisation.diskSize = 1024;
                  };

                testScript = ''
                  machine.succeed("mkdir -p /tmp/mnt")
                  machine.succeed("nixfs /tmp/mnt > /dev/null 2>&1 &")
                  machine.wait_until_succeeds("test -L /tmp/mnt/hello")
                  machine.succeed("readlink /tmp/mnt/hello | grep '/nix/store/'")
                  machine.succeed("$(readlink /tmp/mnt/hello)/bin/hello | grep 'Hello'")
                  machine.succeed("fusermount3 -u /tmp/mnt")
                '';
              }
            );
          pkgWithTests = pkg.overrideAttrs (old: {
            passthru = old.passthru or { } // {
              tests.nixfs = vmTest pkgWithTests;
            };
          });
        in
        {
          packages.nixfs = pkgWithTests;
          packages.default = config.packages.nixfs;
          packages.nixfs-doc = docs;

          checks.default = craneLib.cargoTest {
            inherit src cargoArtifacts;
            meta.description = "nixfs test suite";
          };

          checks.doctests = doctests;

          # Clippy lint check, warnings as errors. Reuses cargoArtifacts, so
          # only this crate re-compiles under the clippy driver. The crate
          # declares no features, so --all-features costs nothing.
          checks.clippy = craneLib.cargoClippy {
            inherit src cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
            meta.description = "nixfs Clippy check";
          };

          apps.default = {
            type = "app";
            program = "${pkg}/bin/nixfs";
            meta.description = "FUSE filesystem exposing Nix package attributes as virtual symlinks";
          };

          treefmt = {
            programs.nixfmt.enable = true;
            programs.rustfmt.enable = true;
            programs.rustfmt.edition = "2024";
            programs.taplo.enable = true;
            settings.formatter.rustfmt.options = lib.mkAfter [
              "--config"
              "max_width=100,comment_width=100,wrap_comments=true,group_imports=StdExternalCrate,imports_granularity=Crate,condense_wildcard_suffixes=true,error_on_line_overflow=true,error_on_unformatted=true,format_code_in_doc_comments=true,format_macro_matchers=true,format_macro_bodies=true,format_strings=true,hex_literal_case=Lower,normalize_comments=true,use_field_init_shorthand=true"
            ];
          };

          devShells.default = craneLib.devShell {
            # nixfs shells out to nix/nix-build/fusermount3 at runtime;
            # interactive `cargo run` needs them on PATH.
            packages = [
              config.treefmt.build.wrapper
              pkgs.fuse3
              pkgs.nix
            ];
            buildInputs = libInputs;
            meta.description = "nixfs development shell";
          };
        };
    };
}
