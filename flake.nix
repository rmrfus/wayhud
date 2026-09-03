{
  description = "wayhud — heads-up text overlay for sway";

  # Indirect ref: on a machine whose flake registry already has nixpkgs
  # realised (e.g. the author's), this reuses that store path. Consumers get
  # whatever the lock pins — override with inputs.wayhud.inputs.nixpkgs.follows.
  inputs.nixpkgs.url = "flake:nixpkgs";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems
        (system: f nixpkgs.legacyPackages.${system});

      # C libs the gtk4-rs / libpulse crates link against. The gtk4-rs sibling
      # crates (cairo-rs, pango, gdk-pixbuf, graphene) each link their own C
      # library directly, so every one needs its .pc file at build time.
      nativeLibs = pkgs: with pkgs; [
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
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "wayhud";
          # Read straight from Cargo.toml so the two never drift apart.
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          # Cargo.lock is committed, so deps resolve straight from it — no
          # cargoHash to recompute on every dependency bump.
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [
            pkg-config
            # GTK looks up its GSettings schemas at run time; without the wrap
            # the binary works only on hosts that happen to have them exported.
            wrapGAppsHook4
          ];
          buildInputs = nativeLibs pkgs;

          postInstall = ''
            install -Dm644 man/man1/wayhud.1 $out/share/man/man1/wayhud.1
            install -Dm644 man/man5/wayhud.5 $out/share/man/man5/wayhud.5
          '';

          meta = with pkgs.lib; {
            description = "Heads-up text overlay for sway, with typewriter reveal";
            homepage = "https://github.com/rmrfus/wayhud";
            license = licenses.mit;
            mainProgram = "wayhud";
            platforms = platforms.linux;
          };
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          buildInputs = nativeLibs pkgs;
          nativeBuildInputs = with pkgs; [
            pkg-config
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            groff # man page lint: groff -man -Tutf8 -ww -z man/man{1,5}/wayhud.*
          ];
          # cargo doesn't RPATH the nix store, so binaries run straight from
          # ./target need the shared objects on the loader path.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (nativeLibs pkgs);
        };
      });
    };
}
