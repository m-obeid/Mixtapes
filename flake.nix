{
  description = "Mixtapes, a Linux-first YouTube Music player written in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        version = pkgs.lib.pipe (self + "/com.pocoguy.Muse.metainfo.xml") [
          builtins.readFile
          (builtins.match ".*<releases>[^<]*<release version=\"([^\"]+)\"[^>]*>.*")
          builtins.head
        ];

        gstPlugins = with pkgs.gst_all_1; [
          gstreamer
          gst-plugins-base
          gst-plugins-good
          gst-plugins-bad
        ];

        # yt-dlp is the fallback stream resolver and the downloader. It wants a
        # JavaScript runtime for YouTube's player code, and ffmpeg to convert.
        runtimeTools = [ pkgs.yt-dlp pkgs.nodejs pkgs.ffmpeg ];

        mixtapes = pkgs.rustPlatform.buildRustPackage {
          pname = "mixtapes";
          inherit version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.wrapGAppsHook4
            # build.rs runs glib-compile-resources for the stylesheet and icons.
            pkgs.glib
          ];

          buildInputs = [
            pkgs.gtk4
            pkgs.libadwaita
            pkgs.webkitgtk_6_0
            pkgs.sqlite
            pkgs.glib-networking
          ] ++ gstPlugins;

          # The tests that matter need the network or a signed-in session.
          doCheck = false;

          postInstall = ''
            ln -s mixtapes $out/bin/muse
            install -Dm644 com.pocoguy.Muse.desktop $out/share/applications/com.pocoguy.Muse.desktop
            install -Dm644 com.pocoguy.Muse.metainfo.xml $out/share/metainfo/com.pocoguy.Muse.metainfo.xml
            install -Dm644 assets/icons/hicolor/scalable/apps/com.pocoguy.Muse.svg $out/share/icons/hicolor/scalable/apps/com.pocoguy.Muse.svg
            install -Dm644 assets/icons/hicolor/symbolic/apps/com.pocoguy.Muse-symbolic.svg $out/share/icons/hicolor/symbolic/apps/com.pocoguy.Muse-symbolic.svg
          '';

          preFixup = ''
            gappsWrapperArgs+=(--prefix PATH : ${pkgs.lib.makeBinPath runtimeTools})
          '';

          meta = {
            description = "A modern, Linux-first YouTube Music player";
            homepage = "https://github.com/m-obeid/Mixtapes";
            license = pkgs.lib.licenses.gpl3Plus;
            mainProgram = "mixtapes";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      in {
        packages.default = mixtapes;

        devShells.default = pkgs.mkShell {
          inputsFrom = [ mixtapes ];
          packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ] ++ runtimeTools;
          # GStreamer finds its plugins through this outside a wrapped binary.
          GST_PLUGIN_SYSTEM_PATH_1_0 = pkgs.lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins;
        };
      }
    );
}
