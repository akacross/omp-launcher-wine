//! open.mp's in-game browser under Wine (WINE-BUILD.md).
//!
//! omp-client.dll, inside the game, finds the overlay window this launcher
//! creates for it (`omp_overlay_window:<game pid>`), copies it with
//! PrintWindow every frame and posts the player's input to its WebView. On
//! Windows the window sits invisible at -15000,-15000. Under Wine two things
//! break:
//!   * the desktop's window manager pulls the window back on screen, and the
//!     front end's `show()` activates it - the game loses focus;
//!   * PrintWindow returns no pixels: Wine implements it by sending WM_PRINT
//!     with the caller's DC, which another process cannot draw into.
//!
//! So, under Wine only: the overlay becomes an unmanaged, non-activating popup
//! parked off screen (WebView2 keeps rendering there), and its page is captured
//! continuously (CapturePreview, PNG, decoded with WIC) into a named shared
//! memory block, `Local\omp_overlay_frame_<game pid>`. ompoverlay.asi, in the
//! game, points omp-client's PrintWindow at a function that copies that block.
//!
//! Block layout (little endian): u32 magic 'OMPF', u32 version 1, u32 width,
//! u32 height, u32 stride, u32 seq (odd while a frame is being written),
//! u32 pid, u32 reserved, then `height` rows of `stride` bytes, BGRA, top-down.
//!
//! Two more things, also under Wine only:
//!   * an overlay closes (close()) only once no capture is in flight, or after
//!     1 s: the capture loop always has one, and destroying the WebView under
//!     it is the likeliest trigger of a WebView2 browser-process crash seen
//!     2026-10-13. Tauri's own close() cannot be held back (it destroys the
//!     window without a CloseRequested event), so the IPC handler and the
//!     page both close overlays through here;
//!   * a WebView2 process that dies is logged. The main window's page goes
//!     with it and never comes back by itself (the launcher "locks up"):
//!     a dead renderer is reloaded, a dead browser process restarts the
//!     launcher (a game keeps running; omp-client reconnects). A dead overlay
//!     is closed; omp-client asks for a new one at the next open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::Manager;
use webview2_com::{
    take_pwstr, CapturePreviewCompletedHandler, ProcessFailedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2ProcessFailedEventArgs2, COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
        COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE, COREWEBVIEW2_PROCESS_FAILED_REASON,
    },
};
use windows::core::{Interface, PWSTR};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{CoCreateInstance, IStream, CLSCTX_INPROC_SERVER, STREAM_SEEK_SET};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READWRITE,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, IsWindow, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM,
    MA_NOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, WINDOWPOS,
    WM_MOUSEACTIVATE, WM_WINDOWPOSCHANGING, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

const MAGIC: u32 = 0x4650_4D4F; // "OMPF"
const VERSION: u32 = 1;
const HEADER: usize = 32;
const MAX_W: u32 = 3840;
const MAX_H: u32 = 2160;
const PARK_X: i32 = -15000; // where the front end keeps putting it, too
const PARK_Y: i32 = -15000;
const SUBCLASS_ID: usize = 0x6F6D_706F; // "ompo"
const CLOSE_GRACE: Duration = Duration::from_millis(1000);
const RESTARTED_AT: &str = "OMP_WINE_RESTARTED_AT"; // set on a launcher started by restart()
const WAIT_FOR: &str = "OMP_WINE_WAIT_FOR"; // the pid of the launcher it replaces
const SYNCHRONIZE: u32 = 0x0010_0000;

/// True when running under Wine (ntdll exports wine_get_version).
pub fn is_wine() -> bool {
    unsafe {
        let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
        ntdll != 0 && GetProcAddress(ntdll, b"wine_get_version\0".as_ptr()).is_some()
    }
}

/// What closing an overlay waits for (close()).
#[derive(Default)]
struct OverlayState {
    capturing: AtomicBool, // the capture loop runs (it always has a CapturePreview in flight)
    closing: AtomicBool,   // close() was asked for: the loop closes the window when its capture is back
    closed: AtomicBool,    // the window's close has been sent, once
}

static OVERLAYS: once_cell::sync::Lazy<Mutex<HashMap<String, Arc<OverlayState>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

fn overlay_state(label: &str) -> Option<Arc<OverlayState>> {
    OVERLAYS.lock().ok().and_then(|overlays| overlays.get(label).cloned())
}

fn forget_overlay(label: &str) {
    if let Ok(mut overlays) = OVERLAYS.lock() {
        overlays.remove(label);
    }
}

/// Close an overlay without destroying its WebView under a capture: from any
/// thread, the capture loop (UI thread) closes it when its capture is back, or
/// CLOSE_GRACE passes. Anything that is not a capturing overlay (not Wine, a
/// failed attach) closes at once.
pub fn close(window: &tauri::Window) {
    let label = window.label().to_string();
    let Some(state) = overlay_state(&label) else {
        let _ = window.close();
        return;
    };
    if state.closing.swap(true, Ordering::SeqCst) {
        return;
    }
    if !state.capturing.load(Ordering::SeqCst) {
        if !state.closed.swap(true, Ordering::SeqCst) {
            log::info!("[wine_overlay] {} closed", label);
            forget_overlay(&label);
            let _ = window.close();
        }
        return;
    }
    log::info!("[wine_overlay] {} closing once its frame capture is back", label);
    let late = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(CLOSE_GRACE);
        if !state.closed.swap(true, Ordering::SeqCst) {
            log::warn!("[wine_overlay] {}: its capture never came back; closing anyway", late.label());
            forget_overlay(late.label());
            let _ = late.close();
        }
    });
}

/// Hook up a freshly built overlay window. Does nothing outside Wine.
pub fn attach(window: &tauri::Window, game_pid: i32) {
    if !is_wine() {
        return;
    }
    let Ok(hwnd) = window.hwnd() else { return };
    let hwnd = hwnd.0;
    let state = Arc::new(OverlayState::default());
    if let Ok(mut overlays) = OVERLAYS.lock() {
        overlays.insert(window.label().to_string(), state.clone());
    }
    log::info!("[wine_overlay] overlay for game {} opened", game_pid);
    let owner = window.clone();
    // WebView2 and window subclassing both want the window's own (UI) thread
    let _ = window.with_webview(move |webview| unsafe {
        park(hwnd);
        let Ok(core) = webview.controller().CoreWebView2() else {
            log::warn!("[wine_overlay] no CoreWebView2 for the overlay window");
            return;
        };
        watch_processes(&core, owner.clone(), Role::Overlay);
        match Capture::new(hwnd, game_pid as u32, core, owner, state) {
            Some(capture) => {
                log::info!("[wine_overlay] serving overlay frames for game {}", game_pid);
                capture_next(Box::new(capture));
            }
            None => log::warn!("[wine_overlay] could not set up frame capture for game {}", game_pid),
        }
    });
}

/// Watch the main window's WebView2 processes (see the top). Does nothing
/// outside Wine.
pub fn watch_main(window: &tauri::Window) {
    if !is_wine() {
        return;
    }
    let owner = window.clone();
    let _ = window.with_webview(move |webview| unsafe {
        match webview.controller().CoreWebView2() {
            Ok(core) => watch_processes(&core, owner, Role::Main),
            Err(e) => log::warn!("[wine_overlay] cannot watch the main window's WebView2: {}", e),
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Main,
    Overlay,
}

/// Log a WebView2 process that dies, and recover what it took with it.
unsafe fn watch_processes(core: &ICoreWebView2, window: tauri::Window, role: Role) {
    let label = window.label().to_string();
    let handler = ProcessFailedEventHandler::create(Box::new(move |sender, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(-1);
        args.ProcessFailedKind(&mut kind)?;
        let mut detail = String::new();
        if let Ok(more) = args.cast::<ICoreWebView2ProcessFailedEventArgs2>() {
            let mut reason = COREWEBVIEW2_PROCESS_FAILED_REASON(-1);
            let mut code = 0i32;
            let mut description = PWSTR::null();
            let _ = more.Reason(&mut reason);
            let _ = more.ExitCode(&mut code);
            let _ = more.ProcessDescription(&mut description);
            let description = if description.is_null() { String::new() } else { take_pwstr(description) };
            detail = format!(", reason {}, exit code {:#010x} {}", reason.0, code as u32, description);
        }
        log::error!(
            "[wine_overlay] a WebView2 process of {} died: kind {} ({}){}",
            window.label(),
            kind.0,
            process_kind_name(kind),
            detail
        );
        let lost_page = kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
            || kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
            || kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE;
        if !lost_page {
            return Ok(()); // GPU, utility and helper processes come back by themselves
        }
        match role {
            Role::Overlay => close(&window),
            Role::Main if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => restart(&window),
            Role::Main => {
                log::warn!("[wine_overlay] reloading {}", window.label());
                if let Some(webview) = sender {
                    let _ = webview.Reload();
                }
            }
        }
        Ok(())
    }));
    let mut token = Default::default();
    if let Err(e) = core.add_ProcessFailed(&handler, &mut token) {
        log::warn!("[wine_overlay] cannot watch the WebView2 processes of {}: {}", label, e);
    }
}

fn process_kind_name(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> &'static str {
    match kind.0 {
        0 => "browser process exited",
        1 => "render process exited",
        2 => "render process unresponsive",
        3 => "frame render process exited",
        4 => "utility process exited",
        5 => "sandbox helper exited",
        6 => "GPU process exited",
        7 => "plugin process exited",
        _ => "unknown",
    }
}

/// The main window's browser process is gone and the page with it, which
/// WebView2 cannot bring back: start the launcher again. Not twice within a
/// minute, in case the browser dies again at start.
fn restart(window: &tauri::Window) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Some(at) = std::env::var(RESTARTED_AT).ok().and_then(|t| t.parse::<u64>().ok()) {
        if now.saturating_sub(at) < 60 {
            log::error!("[wine_overlay] the launcher was restarted less than a minute ago; not again");
            return;
        }
    }
    let Ok(exe) = std::env::current_exe() else { return };
    log::warn!("[wine_overlay] restarting the launcher: its WebView2 browser process is gone");
    match std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(RESTARTED_AT, now.to_string())
        .env(WAIT_FOR, std::process::id().to_string())
        .spawn()
    {
        Ok(_) => window.app_handle().exit(0),
        Err(e) => log::error!("[wine_overlay] could not restart the launcher: {}", e),
    }
}

/// Before the log is opened (it is truncated at every start): keep the last
/// run's as `<name>.prev.log`, so what happened before a restart, by restart()
/// or by hand after a lock-up, is still there. Wine only.
pub fn keep_previous_log(name: &str) {
    if !is_wine() {
        return;
    }
    let path = std::path::Path::new(name);
    if path.exists() {
        let _ = std::fs::rename(path, path.with_extension("prev.log"));
    }
}

/// First thing in main(): a launcher started by restart() waits for the one it
/// replaces, whose deep-link pipe would otherwise take its arguments and send
/// it away (deeplink::prepare forwards to a running instance and exits).
pub fn wait_for_previous_instance() {
    let Some(pid) = std::env::var(WAIT_FOR).ok().and_then(|p| p.parse::<u32>().ok()) else { return };
    std::env::remove_var(WAIT_FOR);
    unsafe {
        let process = OpenProcess(SYNCHRONIZE, 0, pid);
        if process != 0 {
            WaitForSingleObject(process, 10_000);
            CloseHandle(process);
        }
    }
}

/// Unmanaged (no window-manager decorations, placement or focus), never
/// activated, shown off screen so WebView2 keeps painting.
unsafe fn park(hwnd: HWND) {
    SetWindowLongW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS) as i32);
    SetWindowLongW(hwnd, GWL_EXSTYLE, (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as i32);
    SetWindowSubclass(hwnd, Some(no_activate), SUBCLASS_ID, 0);
    SetWindowPos(
        hwnd,
        HWND_BOTTOM,
        PARK_X,
        PARK_Y,
        0,
        0,
        SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
    );
}

/// tao re-applies its own styles on some changes; put ours back when it does.
unsafe fn keep_parked(hwnd: HWND) {
    if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_NOACTIVATE == 0 {
        park(hwnd);
    }
}

unsafe extern "system" fn no_activate(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        // `show()` and friends must never take focus from the game
        WM_WINDOWPOSCHANGING => (*(lparam as *mut WINDOWPOS)).flags |= SWP_NOACTIVATE,
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

struct Capture {
    hwnd: HWND,
    webview: ICoreWebView2,
    stream: IStream,
    factory: IWICImagingFactory,
    frames: FrameBlock,
    window: tauri::Window,
    state: Arc<OverlayState>,
}

impl Capture {
    unsafe fn new(
        hwnd: HWND,
        game_pid: u32,
        webview: ICoreWebView2,
        window: tauri::Window,
        state: Arc<OverlayState>,
    ) -> Option<Self> {
        let stream = CreateStreamOnHGlobal(0, true).ok()?;
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let frames = FrameBlock::open(game_pid)?;
        Some(Self { hwnd, webview, stream, factory, frames, window, state })
    }

    /// Decode the PNG CapturePreview wrote into the stream into the block.
    unsafe fn publish(&mut self) -> windows::core::Result<()> {
        self.stream.Seek(0, STREAM_SEEK_SET)?;
        let decoder =
            self.factory.CreateDecoderFromStream(&self.stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let converter = self.factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut width, mut height) = (0u32, 0u32);
        converter.GetSize(&mut width, &mut height)?;
        if width == 0 || height == 0 || width > MAX_W || height > MAX_H {
            return Ok(());
        }
        let stride = width * 4;
        self.frames.write(width, height, stride, |pixels| converter.CopyPixels(std::ptr::null(), stride, pixels))
    }
}

/// One capture at a time, back to back, for as long as the window lives.
/// Runs on the UI thread, as do the completions and the close requests.
fn capture_next(mut capture: Box<Capture>) {
    unsafe {
        let state = capture.state.clone();
        if state.closing.load(Ordering::SeqCst) {
            // close() is waiting for this capture
            state.capturing.store(false, Ordering::SeqCst);
            if !state.closed.swap(true, Ordering::SeqCst) {
                log::info!("[wine_overlay] {} closed after its last capture", capture.window.label());
                forget_overlay(capture.window.label());
                let _ = capture.window.close();
            }
            return; // dropping the capture unmaps the block
        }
        if IsWindow(capture.hwnd) == 0 {
            state.capturing.store(false, Ordering::SeqCst);
            forget_overlay(capture.window.label());
            return;
        }
        keep_parked(capture.hwnd);
        let _ = capture.stream.SetSize(0);
        let _ = capture.stream.Seek(0, STREAM_SEEK_SET);
        let webview = capture.webview.clone();
        let stream = capture.stream.clone();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            if result.is_ok() {
                if let Err(e) = capture.publish() {
                    log::warn!("[wine_overlay] frame decode failed: {}", e);
                }
            }
            capture_next(capture);
            Ok(())
        }));
        state.capturing.store(true, Ordering::SeqCst);
        if let Err(e) = webview.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler) {
            state.capturing.store(false, Ordering::SeqCst);
            log::warn!("[wine_overlay] CapturePreview failed, overlay frames stop: {}", e);
        }
    }
}

/// The named shared-memory block the game reads.
struct FrameBlock {
    mapping: isize,
    view: *mut u8,
}

impl FrameBlock {
    unsafe fn open(game_pid: u32) -> Option<Self> {
        let size = HEADER as u64 + (MAX_W as u64 * 4) * MAX_H as u64;
        let name = wide(&format!("Local\\omp_overlay_frame_{}", game_pid));
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            (size >> 32) as u32,
            size as u32,
            name.as_ptr(),
        );
        if mapping == 0 {
            return None;
        }
        let view: MEMORY_MAPPED_VIEW_ADDRESS = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0);
        if view.Value.is_null() {
            CloseHandle(mapping);
            return None;
        }
        let block = Self { mapping, view: view.Value as *mut u8 };
        let header = block.view as *mut u32;
        header.add(0).write_volatile(MAGIC);
        header.add(1).write_volatile(VERSION);
        header.add(6).write_volatile(game_pid);
        Some(block)
    }

    fn seq(&self) -> &AtomicU32 {
        unsafe { &*(self.view.add(20) as *const AtomicU32) }
    }

    /// Seqlock: odd while writing, so a reader never takes a torn frame.
    unsafe fn write(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        fill: impl FnOnce(&mut [u8]) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        let seq = self.seq().load(Ordering::Relaxed);
        self.seq().store(seq | 1, Ordering::Release);
        let pixels = std::slice::from_raw_parts_mut(self.view.add(HEADER), (stride * height) as usize);
        let result = fill(pixels);
        let header = self.view as *mut u32;
        header.add(2).write_volatile(width);
        header.add(3).write_volatile(height);
        header.add(4).write_volatile(stride);
        self.seq().store((seq | 1).wrapping_add(1), Ordering::Release);
        result
    }
}

impl Drop for FrameBlock {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut _ });
            CloseHandle(self.mapping);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
