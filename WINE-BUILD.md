# open.mp launcher v1.7.0, rebuilt for Wine/Proton

A Wine/Proton compatibility pass on https://github.com/openmultiplayer/launcher
at tag v1.7.0. Upstream's launcher installs and opens under Proton (with the
Microsoft Edge WebView2 runtime), but crashes, lags and breaks the in-game
server browser there. These changes fix that in source. Everything Wine-only
is behind a check for `wine_get_version` in ntdll, so on Windows the launcher
behaves as upstream's. The full setup (prefix, WebView2, launch script) is
[samp-openmp-linux](https://github.com/akacross/samp-openmp-linux).

## The fixes

| problem under Wine | cause | fix |
|---|---|---|
| Crash about a second after start, exit code 5 (0xC0000005) | wry swaps in its own drag-and-drop handler by calling `RevokeDragDrop` on WebView2's window, which belongs to `msedgewebview2.exe`. Windows refuses that; Wine's `RevokeDragDrop` lacks the cross-process check `RegisterDragDrop` has, and releases a pointer from the other process. | `fileDropEnabled: false` on the main window (`tauri.conf.json`) and `disable_file_drop_handler()` on the in-game overlay window (`src-tauri/src/ipc.rs`). |
| Typing and resizing lag; two cores busy while idle; wineserver ~70% | Server queries used tokio's `UdpSocket`. Wine's AFD poll can flag a connected UDP socket `AFD_POLL_CONNECT_FAIL` (0x100); tokio turns that into a sticky READ_CLOSED readiness, while Wine's `recv` answers "would block" instead of the error, so tokio retries `recv` in a tight loop until the 2 s query timeout. The query socket is cached and reused, so it never stopped. | `src-tauri/src/query.rs`: a blocking `std::net::UdpSocket` with a 2 s read timeout; each receive runs on tokio's blocking pool. Idle CPU went from 17 s per 10 s to ~0.05 s. |
| Resize cursor sticks after the pointer crosses an edge | wry (frameless windows) sets resize cursors from the launcher process but never the arrow, leaving it to Chromium; under Wine Chromium (another process) cannot change the window's cursor. | Vendored wry 0.24.10 (`src-tauri/vendor/`, wired in with `[patch.crates-io]`): always sets the cursor, the arrow included. |
| Black box around the window | The window is transparent and the page leaves a 4 px margin around a rounded panel; Wine cannot show per-pixel transparency there. | `src/App.tsx`: no margin, no rounded corners. |
| Generic Wine icon in the desktop's taskbar | Neither Tauri nor tao sets a window icon; Windows' taskbar falls back to the exe's icon, but under Wine the desktop only sees the window's. | `src-tauri/src/main.rs`: set the exe's icon resource (group 32512) on the main window with `WM_SETICON`. |
| Loading screen stuck on "Downloading OMP plugin..." (and the SA-MP files download) | tauri-plugin-upload downloads with reqwest on tokio, whose sockets sit on Wine's AFD poll; the read for the last few kilobytes never wakes (1982464 of omp-client-7.dll's 2008816 bytes arrived, then nothing, at no CPU). The loading screen also ignored the download's own result, so a failure would hang too. | `src-tauri/src/wine_download.rs`: under Wine, downloads go through WinHTTP (Wine's own HTTP stack, blocking sockets) with the plugin's progress reports, through `src/utils/download.ts`; `LoadingScreen` rejects on a failed download. The 2 MB plugin then arrives in about half a second. |
| The open.mp plugin gets "updated" to an older one (v0.1.4) | Upstream race: the loading screen's plugin check fetches the update info itself when `fetchUpdateInfo()` hasn't stored it yet, and then uses the top-level fields (the version-5 launcher's plugin) instead of this build's `versions["6"]` entry. | Fixed upstream in v1.7.0 (`processOmpPluginVerification` uses this build's entry, falling back to the latest). |

## Chromium flags and rendering

The vendored wry appends the contents of `OMP_WEBVIEW2_ARGS` to WebView2's
browser arguments (WebView2's own `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` and
its registry policy are both ignored once the app passes arguments, which wry
always does). Under Wine, run it with `OMP_WEBVIEW2_ARGS=--disable-gpu`:
WebView2 then draws in software and hands Wine plain GDI output, instead of a
DXVK swapchain in a child window owned by another process. Measured with
Chromium's event timing (`--remote-debugging-port` through the same variable):
GPU mode ran 60 frames/s and painted 4 of 24 keystrokes later than 16 ms
(worst 40 ms); software mode ran at the monitor's rate and painted all 24
within 16 ms.

Native Wayland (`PROTON_ENABLE_WAYLAND=1`) does not work: Wine's Wayland driver
shows nothing from a child window owned by another process, and WebView2's
content window belongs to `msedgewebview2.exe`. Blank with GPU and with
software rendering, though Chromium renders fine.

## The in-game browser

omp-client.dll asks the launcher (IPC on 127.0.0.1:45791, `show_overlay:<pid>`)
for an overlay window, `omp_overlay_window:<game pid>`. It copies that window
with `PrintWindow` every frame and posts the player's input to its WebView. On
Windows the window sits invisible at -15000,-15000. Under Wine three things
break:

- the window manager pulls the window back on screen;
- `show()` activates it, so the full-screen game loses focus and minimises;
- `PrintWindow` returns no pixels. Wine sends `WM_PRINT` with the game's DC
  to the launcher, which cannot draw into another process's DC.

The result is the game tabbing out and an empty square in game.

`src-tauri/src/wine_overlay.rs` (only when ntdll exports `wine_get_version`):

- makes the overlay an unmanaged, never-activated popup parked off screen:
  `WS_POPUP`, `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`, and a subclass that adds
  `SWP_NOACTIVATE` and answers `MA_NOACTIVATE`;
- captures the page back to back (`CapturePreview` PNG, decoded with WIC)
  into `Local\omp_overlay_frame_<game pid>`. That is a 32-byte header
  (magic `OMPF`, version 1, width, height, stride, seq odd while writing, pid,
  reserved), then BGRA rows, top-down, with room for 3840x2160;
- builds the overlay with `focused(false)` (`ipc.rs`). Tauri's default
  `focused(true)` made wry `MoveFocus` into the new WebView and tao force the
  window to the foreground (`force_window_active`: `SetForegroundWindow`,
  then a synthetic Alt press if that fails). Windows' foreground lock stops
  that; Wine has none, so every open took the foreground for ~180 ms. Now the
  first `show()` is `SW_SHOWNOACTIVATE` and nothing focuses the WebView;
- keeps the overlay disabled (`EnableWindow(FALSE)`). The game posts the
  player's clicks to Chromium's `Chrome_RenderWidgetHostHWND`, and on a
  button press Chromium calls `SetFocus` on it. That activates its top-level
  window, the overlay, and Wine hands it the foreground, so the game tabbed
  out on the first click into the browser until the player clicked the game
  again. Wine's `SetFocus` refuses a window under a disabled one. Posted mouse
  and key messages are not hardware input and still reach the page.

The game side is `ompoverlay.asi`
([omp-ingame-browser-wine](https://github.com/akacross/omp-ingame-browser-wine)).
It points omp-client's `PrintWindow` import at a copy from that block. It also
works around an omp-client bug: closing the overlay frees its DIB but keeps its
size, so a reopen at the same size never made a new one (the browser opened
only once). The game and the launcher must share a prefix, and so a
wineserver: start games from the launcher.

Testing without the game: a process that stays alive (the front end closes an
overlay whose `attached_id` process is gone, after about a second), plus a
script that speaks omp-client's side of the IPC: `init:<pid>`,
`show_overlay:<pid>`, `pos:x,y,w,h,<pid>`, `hide_overlay:<pid>`.

## WebView2 crashes: the launcher "locking up"

On 2026-10-13 WebView2's browser process crashed 9 s after an overlay opened
(Crashpad: `C0000005` reading `0x420` at `msedge.dll+0x2705364`, in the
browser's main thread). Every page lives in that process, so the main window's
page died with it and the window stayed blank and frozen. Overlays opened
afterwards still worked, because each Tauri window makes its own WebView2
environment and so a new browser process. The cause is not proven. The likeliest
trigger is the capture loop, which always has a `CapturePreview` in flight:
closing an overlay destroyed its WebView under that capture. Under Wine:

- **Overlays close only when no capture is in flight.**
  `wine_overlay::close()` marks the overlay closing; the capture loop closes
  the window when its current capture is back, or after 1 s.
  - Tauri 1's `Window::close()` cannot be held back: it destroys the window
    without a `CloseRequested` event. So both close paths go through
    `close()`: the IPC `hide_overlay` handler, and the page's own close when
    its game is gone (the new `close_this_overlay` command in `App.tsx`).
  - 30 open/close cycles at 1.2 s: every close waited for its capture, with no
    crash.
- **A dead WebView2 process is logged and recovered from** (`ProcessFailed`
  on every WebView):
  - for the main window, a dead or hung renderer is reloaded; a dead browser
    process restarts the launcher. A running game keeps running and omp-client
    reconnects. The new launcher waits for the old one to exit first, or the
    deep-link check would hand it straight back to it. There is no second
    restart within a minute;
  - a dead overlay is closed, and omp-client asks for a new one at the next
    open;
  - GPU and utility processes come back by themselves.
- **The previous run's log is kept** as `omp-launcher.prev.log` beside
  `omp-launcher.log` (next to the exe, or in local app data when that fails),
  because the log is truncated at every start.

Tested by killing `msedgewebview2.exe` with an overlay open:

```
ERROR  [wine_overlay] a WebView2 process of omp_overlay_window:1316 died: kind 0 (browser process exited), reason 0, exit code 0x00000001
INFO   [wine_overlay] omp_overlay_window:1316 closing once its frame capture is back
ERROR  [wine_overlay] a WebView2 process of main died: kind 0 (browser process exited), reason 0, exit code 0x00000001
WARN   [wine_overlay] restarting the launcher: its WebView2 browser process is gone
```

The new instance came up, took over the IPC port and served overlays again.

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
TARGET=i686-pc-windows-gnu ~/.cargo/bin/cargo build --release --target i686-pc-windows-gnu --features custom-protocol
```

`TARGET` matters: tauri-codegen picks Windows or Linux handling (the window
icon format, for one) from it, but Cargo only sets it for build scripts, so
without it Tauri's `generate_context!` macro sees the Linux host.

Output: `src-tauri/target/i686-pc-windows-gnu/release/omp-launcher.exe` and
`WebView2Loader.dll` (the GNU build loads WebView2 from that DLL, so ship it
next to the exe). Replace the installed `omp-launcher.exe` with both.
