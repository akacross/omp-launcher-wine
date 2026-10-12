# open.mp launcher v1.6.1, rebuilt for Wine/Proton

Branch `wine` of a clone of https://github.com/openmultiplayer/launcher at tag
v1.6.1. Upstream's launcher installs and opens under Proton (with the Microsoft
Edge WebView2 runtime), but four problems make it unusable there. This branch
fixes them in source.

## The fixes

| problem under Wine | cause | fix |
|---|---|---|
| Crash about a second after start, exit code 5 (0xC0000005) | wry swaps in its own drag-and-drop handler by calling `RevokeDragDrop` on WebView2's window, which belongs to `msedgewebview2.exe`. Windows refuses that; Wine's `RevokeDragDrop` lacks the cross-process check `RegisterDragDrop` has, and releases a pointer from the other process. | `fileDropEnabled: false` on the main window (`tauri.conf.json`) and `disable_file_drop_handler()` on the in-game overlay window (`src-tauri/src/ipc.rs`). |
| Typing and resizing lag; two cores busy while idle; wineserver ~70% | Server queries used tokio's `UdpSocket`. Wine's AFD poll can flag a connected UDP socket `AFD_POLL_CONNECT_FAIL` (0x100); tokio turns that into a sticky READ_CLOSED readiness, while Wine's `recv` answers "would block" instead of the error, so tokio retries `recv` in a tight loop until the 2 s query timeout. The query socket is cached and reused, so it never stopped. | `src-tauri/src/query.rs`: a blocking `std::net::UdpSocket` with a 2 s read timeout; each receive runs on tokio's blocking pool. Idle CPU went from 17 s per 10 s to ~0.05 s. |
| Resize cursor sticks after the pointer crosses an edge | wry (frameless windows) sets resize cursors from the launcher process but never the arrow, leaving it to Chromium; under Wine Chromium (another process) cannot change the window's cursor. | Vendored wry 0.24.10 (`src-tauri/vendor/`, wired in with `[patch.crates-io]`): always sets the cursor, the arrow included. |
| Black box around the window | The window is transparent and the page leaves a 4 px margin around a rounded panel; Wine cannot show per-pixel transparency there. | `src/App.tsx`: no margin, no rounded corners. |

Also known, not fixed here: the launcher's own download of the open.mp plugin
(`tauri-plugin-upload`, reqwest on tokio) stalls ~24 KB before the end under
Wine. Place `omp-client.dll` (from the URL in https://api.open.mp/launcher,
`ompPluginDownload`; check its MD5 against `ompPluginChecksum`) at
`%LOCALAPPDATA%\mp.open.launcher\omp\omp-client.dll` and the launcher skips the
download.

## Building (from Linux)

Upstream builds with `nightly-2024-01-01` (its `dll-syringe` dependency uses
unstable features) for `i686-pc-windows-msvc`. This branch cross-builds for
`i686-pc-windows-gnu` with MinGW instead; `src-tauri/rust-toolchain.toml` pins
the nightly and `src-tauri/.cargo/config.toml` points Cargo at the MinGW tools.

Needs: rustup (the pinned toolchain and target install on first use), the
32-bit MinGW cross compiler (`i686-w64-mingw32-gcc`, `-windres`), Node.js, and
yarn 1.

```bash
npx yarn@1.22.22 install --frozen-lockfile
npx yarn@1.22.22 build
cd src-tauri
~/.cargo/bin/cargo build --release --target i686-pc-windows-gnu --features custom-protocol
```

Output: `src-tauri/target/i686-pc-windows-gnu/release/omp-launcher.exe` and
`WebView2Loader.dll` (the GNU build loads WebView2 from that DLL, so ship it
next to the exe). Replace the installed `omp-launcher.exe` with both.
