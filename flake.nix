{
  description = "wayhud — layer-shell text overlay for sway";

  # Indirect ref: resolves through the local flake registry, reusing the
  # system's already-realised nixpkgs store path (no tarball download).
  inputs.nixpkgs.url = "flake:nixpkgs";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems
        (system: f nixpkgs.legacyPackages.${system});
    in {
      devShells = forAll (pkgs:
        let
          # C libs the gtk4-rs / libpulse crates link against. gtk4-rs sibling
          # crates (cairo-rs, pango, gdk-pixbuf, graphene) link their C libs
          # directly, so each needs a .pc file at build time.
          nativeLibs = with pkgs; [
            gtk4
            gtk4-layer-shell
            glib
            cairo
            pango
            gdk-pixbuf
            graphene
            libpulseaudio
          ];
        in {
          default = pkgs.mkShell {
            buildInputs = nativeLibs;
            nativeBuildInputs = with pkgs; [
              pkg-config
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
            ];
            # cargo doesn't RPATH the nix store, so binaries run straight from
            # ./target need the shared objects on the loader path.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath nativeLibs;
          };
        });
    };
}
