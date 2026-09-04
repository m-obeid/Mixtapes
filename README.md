<div align="center">
<img height="150" src="assets/icons/hicolor/scalable/apps/com.pocoguy.Muse.svg" />

<h3 style="font-size: 30px"> Mixtapes </h3>

A modern, Linux-first YouTube Music player built with GTK4 and Libadwaita.
<br><small>formerly known as Muse</small>

</div>

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/m-obeid/Mixtapes)](https://github.com/m-obeid/Mixtapes/stargazers)
[![GitHub issues](https://img.shields.io/github/issues/m-obeid/Mixtapes)](https://github.com/m-obeid/Mixtapes/issues)
[![AUR](https://img.shields.io/aur/version/mixtapes-git)](https://aur.archlinux.org/packages/mixtapes-git)
[![Flatpak CI](https://img.shields.io/github/actions/workflow/status/m-obeid/Mixtapes/build-flatpak.yml?label=Flatpak%20Build)](https://github.com/m-obeid/Mixtapes/actions/workflows/build-flatpak.yml)
[![Windows Build](https://img.shields.io/github/actions/workflow/status/m-obeid/Mixtapes/build-windows.yml?label=Windows%20Build)](https://github.com/m-obeid/Mixtapes/actions/workflows/build-windows.yml)
[![Windows Download](https://img.shields.io/badge/Windows-Download%20Installer-blue?logo=windows)](https://nightly.link/m-obeid/Mixtapes/workflows/build-windows/main/mixtapes-windows-x86_64-setup.zip)

<br clear="both"/>

> [!NOTE]
> This software is in alpha. Expect bugs and missing features.
> It is not affiliated with, funded, authorized, endorsed, or in any way associated with YouTube, Google LLC or any of their affiliates and subsidiaries.
> Help is always appreciated -- feel free to open an issue or a pull request!

---

<div align="center">
  <img src="screenshots/9.png" />
  <img src="screenshots/1.png" width="49%" /> <img src="screenshots/2.png" width="49%" />
  <img src="screenshots/3.png" width="49%" /> <img src="screenshots/4.png" width="49%" />
  <br/>
  <img src="screenshots/5.png" width="24%" /> <img src="screenshots/6.png" width="24%" /> <img src="screenshots/7.png" width="24%" /> 
  <img src="screenshots/8.png" width="24%" />
</div>

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Authentication](#authentication)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [Star History](#star-history)
- [Contributors](#contributors)
- [License](#license)

## Features

- **YouTube Music Integration** -- Connect with your account and access your full library
- **Library Access** -- Playlists, liked songs, artists, albums, and uploads
- **Search & Discovery** -- New releases, moods & moments, genres, trending, and charts
- **Full Playback Control** -- Play/pause, seeking, queue management, shuffle, repeat modes
- **Downloads** -- Download tracks for offline playback as local files
- **Scrobbling** -- Submit your plays to Last.fm and ListenBrainz, with an offline backlog
- **MPRIS Support** -- Control playback from system media controls (Linux)
- **Windows SMTC** -- System media transport controls integration (Windows)
- **Radio & Mixes** -- Start a radio station from any song or artist
- **Background Playback** -- Music keeps playing when the window is closed (system tray on Windows)
- **Playlist Editing** -- Reorder, multi-select edit, change covers, visibility, and metadata
- **Caching** -- Cached data for snappy performance
- **Responsive UI** -- Adaptive layout built with Libadwaita

## Installation

### Flatpak (Recommended)

This is the recommended way to install Mixtapes on Linux, as it avoids issues with your distribution's packaging.

Add the automated repository and install:

```bash
flatpak remote-add --user --if-not-exists mixtapes https://m-obeid.github.io/Mixtapes/mixtapes.flatpakrepo
flatpak install --user mixtapes com.pocoguy.Muse
```

> [!NOTE]
> If you previously installed under the old "Muse" repository name, remove the old remote first:
> `flatpak remote-delete --user muse`

<details>
<summary>Offline bundle install</summary>

Download the latest artifact from [GitHub Actions](https://github.com/m-obeid/Mixtapes/actions), then:

```bash
unzip Mixtapes-x86_64-flatpak.zip
flatpak install --user ./Mixtapes-x86_64.flatpak
```

Both `x86_64` and `aarch64` builds are available.

</details>

### Windows (Experimental)

Download and run the installer: **[MixtapesSetup.exe](https://nightly.link/m-obeid/Mixtapes/workflows/build-windows/main/mixtapes-windows-x86_64-setup.zip)**

A portable (no-install) ZIP is also available from [GitHub Actions](https://github.com/m-obeid/Mixtapes/actions/workflows/build-windows.yml).

> [!NOTE]
> The Windows build is experimental. Known limitations:
>
> - SMTC (media controls) works but may show "Unknown app" without the installer
> - Font rendering differs from Linux

### AUR (Arch Linux)

```bash
yay -S mixtapes-git
```

> [!WARNING]
> If you are using CachyOS, you will also need to reinstall webkitgtk-6.0 from the Arch 'extra' repo, not the CachyOS repo:
> `sudo pacman -S extra/webkitgtk-6.0`

### Nix

```
nix run github:m-obeid/Mixtapes       # run directly from GitHub
nix run                               # run from local checkout
nix develop                           # enter dev shell
```

### From Source

<details>
<summary>Install dependencies for your distro</summary>

**Arch Linux:**

```bash
sudo pacman -S git python-pip nodejs gtk4 libadwaita webkitgtk-6.0 gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly
```

**Fedora:**

```bash
sudo dnf install git python3 python3-pip nodejs gtk4-devel adwaita-gtk4-devel webkitgtk6.0-devel gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad gstreamer1-plugins-ugly
```

**Debian/Ubuntu:**

```bash
sudo apt install git python3 python3-pip nodejs libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly
```

> [!NOTE]
> On Debian/Ubuntu, consider using the Flatpak install to avoid outdated packages.

</details>

> [!NOTE]
> **PO-Token provider (recommended).** Seekable Opus and other
> PO-Token-gated formats need the `rustypipe-botguard` binary. The packaged
> builds (Flatpak, AUR, Windows installer) bundle it automatically, but when
> running **from source** you must install it yourself and keep it on your
> `PATH`:
>
> ```bash
> cargo install rustypipe-botguard
> ```
>
> or grab a prebuilt binary from <https://codeberg.org/ThetaDev/rustypipe-botguard>.
> Without it playback still works; only PO-Token-gated formats are skipped.

#### Option 1: Running from source

```bash
git clone https://github.com/m-obeid/Mixtapes.git
cd Mixtapes
python3 -m venv .venv --system-site-packages
source .venv/bin/activate
pip install -r requirements.txt
chmod +x start.sh
./start.sh
```

To update:

```bash
git pull
pip install -r requirements.txt
```

#### Option 2: build as binary with Nuitka

Ensure dependencies are installed

```bash
git clone https://github.com/m-obeid/Mixtapes.git
cd Mixtapes
chmod +x build.sh
./build.sh
```

Run the app with:

```bash
./src/mixtapes
```

<details>
<summary>Build on Windows (from source)</summary>

Requires [MSYS2](https://www.msys2.org/) with the UCRT64 environment:

```bash
# In MSYS2 UCRT64 terminal:
pacman -S mingw-w64-ucrt-x86_64-gtk4 mingw-w64-ucrt-x86_64-libadwaita \
  mingw-w64-ucrt-x86_64-python mingw-w64-ucrt-x86_64-python-pip \
  mingw-w64-ucrt-x86_64-python-gobject mingw-w64-ucrt-x86_64-gstreamer \
  mingw-w64-ucrt-x86_64-gst-plugins-base mingw-w64-ucrt-x86_64-gst-plugins-good \
  mingw-w64-ucrt-x86_64-gst-plugins-bad mingw-w64-ucrt-x86_64-gst-plugins-ugly \
  mingw-w64-ucrt-x86_64-glib2 mingw-w64-ucrt-x86_64-nodejs \
  mingw-w64-ucrt-x86_64-ffmpeg mingw-w64-ucrt-x86_64-python-pillow git

git clone https://github.com/m-obeid/Mixtapes.git && cd Mixtapes
pip install --break-system-packages -r requirements-windows.txt
pip install --break-system-packages pystray

# Compile GResources and run:
glib-compile-resources --sourcedir=. src/muse.gresource.xml --target=src/muse.gresource
python src/main.py
```

**SMTC bridge** (optional, for Windows media controls):

```bash
# In a regular PowerShell/CMD (not MSYS2), with Rust installed:
cd windows/bridge
cargo build --release
# Copy target/release/MixtapesBridge.exe to windows/ in the app directory
```

**Login helper** (optional, for browser-based login):

```bash
# In a regular PowerShell/CMD with Python 3.12:
pip install pywebview pyinstaller
pyinstaller --onefile --noconsole --name MixtapesLogin windows/login_helper.py
# Copy dist/MixtapesLogin.exe to windows/ in the app directory
```

</details>

<details>
<summary>Build with flatpak-builder</summary>

```bash
flatpak install flathub org.gnome.Platform//49 org.gnome.Sdk//49 org.freedesktop.Sdk.Extension.node24//24.08
git clone https://github.com/m-obeid/Mixtapes.git && cd Mixtapes
flatpak-builder --user --install --force-clean build-dir com.pocoguy.Muse.yaml
flatpak run com.pocoguy.Muse
```

</details>

### Prerequisites

| Dependency                                | Purpose                                         | Windows                 |
| ----------------------------------------- | ----------------------------------------------- | ----------------------- |
| Python 3.10+                              | Core runtime                                    | via MSYS2               |
| Node.js                                   | Required for yt-dlp-ejs (fixes playback issues) | via MSYS2               |
| GTK4 + dev headers                        | UI toolkit                                      | via MSYS2               |
| Libadwaita + dev headers                  | GNOME UI components                             | via MSYS2               |
| WebKitGTK 6.0 + dev headers               | Embedded browser for auth                       | N/A (uses Login Helper) |
| GStreamer plugins (base, good, bad, ugly) | Audio playback                                  | via MSYS2               |
| ffmpeg                                    | Audio muxing for downloads                      | via MSYS2               |

### Last.fm API Credentials

Nothing to do here for a normal build. This section explains where the key comes from.

Last.fm requires every client to ship its own API key. ListenBrainz needs no app credentials and works out of the box.

The key and secret live in `_EMBEDDED_LASTFM_API_KEY` and `_EMBEDDED_LASTFM_API_SECRET` at the top of [src/player/scrobbler.py](src/player/scrobbler.py), and they are committed on purpose. The AUR package builds from a `git clone` on the user's own machine, and a Flathub build runs on Flathub's infrastructure, so neither one receives a secret from CI. A credential shipped inside a desktop client is extractable from the binary no matter how it got there, so injecting it at build time would protect nothing while leaving AUR and Flathub users without Last.fm.

To build against your own Last.fm app, register one at [last.fm/api/account/create](https://www.last.fm/api/account/create), then either replace the two constants or set these before launching:

```bash
export MIXTAPES_LASTFM_API_KEY=your_key
export MIXTAPES_LASTFM_API_SECRET=your_secret
```

> [!IMPORTANT]
> The environment variables are read when the app starts, not when it is compiled. Exporting them during `makepkg` or `flatpak-builder` has no effect on the resulting package.

With no credentials the Last.fm row in Preferences stays disabled and says so. ListenBrainz is unaffected.

## Authentication

> [!TIP]
> **Linux:** You can authenticate directly in the app using the built-in WebKit browser -- no manual setup needed!
> **Windows:** Use the bundled Login Helper (Start Menu > Mixtapes > Login Helper) to sign in via Edge WebView2.

<details>
<summary>Manual authentication (legacy)</summary>

This app uses `ytmusicapi` for backend data. Authentication gives access to your library and higher quality streams.

1. Run: `ytmusicapi browser`
2. Follow instructions to log in via your browser and paste the headers. Use a private browser profile so you don't get logged out.
3. The output will be saved as `browser.json`.

**Flatpak users:** Open YouTube Music in your browser, copy request headers as described in the [ytmusicapi docs](https://ytmusicapi.readthedocs.io/en/stable/setup/browser.html), then:

```bash
flatpak run --command=sh com.pocoguy.Muse
mkdir -p ~/data/Muse && cd ~/data/Muse && ytmusicapi browser
```

Paste the headers and press `Ctrl-D`.

Without a `browser.json` file, the app falls back to the unauthenticated API, which may cause playback issues.

</details>

## Roadmap

✅️ = implemented · ☑️ = partially implemented · 🔜 = planned · ❎️ = unlikely

| Status | Feature                      | Details                                                                                                                                                                       |
| :----: | ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
|   ✅️   | **Authentication**           | Connect to YouTube Music (Browser cookies)                                                                                                                                    |
|   ✅️   | **Library**                  | ✅️ Playlists<br>✅️ Liked songs<br>✅️ Artists<br>✅️ Albums<br>✅️ Uploads                                                                                                       |
|   ✅️   | **Search**                   | Search for songs, albums, and artists                                                                                                                                         |
|   ✅️   | **Exploration**              | ✅️ New Releases<br>✅️ Moods & Moments<br>✅️ Genres<br>✅️ Trending<br>✅️ Charts<br>✅️ Home Page                                                                                |
|   ✅️   | **Artist Page**              | ✅️ Basic info<br>✅️ Related artists<br>✅️ Top tracks<br>✅️ Albums<br>✅️ Singles/EPs<br>✅️ Videos<br>✅️ Play<br>✅️ Shuffle<br>✅️ Subscribe/Unsubscribe                         |
|   ✅️   | **Playlist Page**            | ✅️ Info<br>✅️ Tracks<br>✅️ Play<br>✅️ Shuffle<br>✅️ Order<br>✅️ Multi-Selection Editing<br>✅️ Cover Change<br>✅️ Change Visibility<br>✅️ Change Description<br>✅️ Change Name |
|   ✅️   | **Album Page**               | ✅️ Basic info<br>✅️ Tracks<br>✅️ Play<br>✅️ Shuffle                                                                                                                           |
|   ✅️   | **Player**                   | ✅️ Play/Pause<br>✅️ Seeking<br>✅️ Volume<br>✅️ Queue (Previous/Next, Reorder, Shuffle, Repeat modes)                                                                          |
|   ✅️   | **History**                  | ✅️ View history<br> ✅️ Share history with Google account<br> ✅️ Delete songs from history                                                                                     |
|   ✅️   | **Caching**                  | Cache data to reduce latency                                                                                                                                                  |
|   ☑️   | **Responsive Design**        | Mobile-friendly layout with adaptive UI                                                                                                                                       |
|   ✅️   | **MPRIS Support**            | Control playback from system media controls                                                                                                                                   |
|   ✅️   | **Download Support**         | Download tracks for offline playback, even as local files                                                                                                                     |
|   ✅️   | **Radio / Mixes**            | Start a radio station from a song, album, playlist, or artist                                                                                                                 |
|   ✅️   | **Dedicated Data Directory** | Cookies, cache, etc. in a dedicated directory                                                                                                                                 |
|   ✅️   | **Background Playback**      | Music keeps playing when the window is closed                                                                                                                                 |
|   ✅️   | **AUR**                      | Available as `mixtapes-git`                                                                                                                                                   |
|   ☑️   | **Flatpak**                  | ✅️ Flatpak build<br>🔜 Flathub release<br>✅️ App icon                                                                                                                         |
|   ☑️   | **Settings**                 | Configure app preferences (theme, audio quality, etc.).                                                                                                                       |
|   ✅️   | **Cover Art Tint**           | Tint Libadwaita to match cover art, kinda like Material You                                                                                                                   |
|   ✅️   | **Scrobbling**               | Submit plays to Last.fm and ListenBrainz<br>✅️ Now Playing<br>✅️ Offline backlog with retries                                               |
|   ✅️   | **Discord RPC**              | Show your current track on Discord<br>✅️ Linux<br>✅️ Windows                                                                                                                  |
|   ✅️   | **Lyrics**                   | Synchronized lyrics using a bunch of providers (Apple Music, BetterLyrics, BiniLyrics, NetEase, LRCLIB, native YT Music)<br>✅️ Reorderable provider search queue<br>✅️ Second line: romanization, translation or background vocals<br>✅️ Word-level karaoke timing with duration-aware fades |
|   ✅️   | **Windows**                  | ✅️ GTK4/Libadwaita via MSYS2<br>✅️ GStreamer playback<br>✅️ SMTC media controls<br>✅️ System tray<br>✅️ Installer<br>✅️ Login helper (Edge WebView2)                          |
|   🔜   | **macOS**                    | Can build it for macOS, just need to test, there's a PR for auto-builds.                                                                                                      |
|   🔜   | **GNOME Circle**             | Still considering it, might not happen                                                                                                                                        |

Have an idea or found a bug? [Open an issue!](https://github.com/m-obeid/Mixtapes/issues)

## Contributing

Contributions are welcome! Feel free to open issues for bug reports or feature requests, and submit pull requests.

## Star History

<a href="https://star-history.dera.page/#m-obeid/Mixtapes&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://star-history.dera.page/svg?repos=m-obeid/Mixtapes&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://star-history.dera.page/svg?repos=m-obeid/Mixtapes&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://star-history.dera.page/svg?repos=m-obeid/Mixtapes&type=date&legend=top-left" />
 </picture>
</a>

## Contributors

<a href="https://github.com/m-obeid/Mixtapes/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=m-obeid/Mixtapes" width="600"/>
</a>

The app icon was sketched by [Jakub Steiner](https://gitlab.gnome.org/jimmac) and rendered by [gnoman](https://gitlab.gnome.org/gnoman).

## License

This project is licensed under the [GNU General Public License v3.0](LICENSE) or later.
