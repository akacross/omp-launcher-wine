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

use std::sync::atomic::{AtomicU32, Ordering};

use webview2_com::{
    CapturePreviewCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
    },
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{CoCreateInstance, IStream, CLSCTX_INPROC_SERVER, STREAM_SEEK_SET};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
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

/// True when running under Wine (ntdll exports wine_get_version).
pub fn is_wine() -> bool {
    unsafe {
        let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
        ntdll != 0 && GetProcAddress(ntdll, b"wine_get_version\0".as_ptr()).is_some()
    }
}

/// Hook up a freshly built overlay window. Does nothing outside Wine.
pub fn attach(window: &tauri::Window, game_pid: i32) {
    if !is_wine() {
        return;
    }
    let Ok(hwnd) = window.hwnd() else { return };
    let hwnd = hwnd.0;
    // WebView2 and window subclassing both want the window's own (UI) thread
    let _ = window.with_webview(move |webview| unsafe {
        park(hwnd);
        let Ok(core) = webview.controller().CoreWebView2() else {
            log::warn!("[wine_overlay] no CoreWebView2 for the overlay window");
            return;
        };
        match Capture::new(hwnd, game_pid as u32, core) {
            Some(capture) => {
                log::info!("[wine_overlay] serving overlay frames for game {}", game_pid);
                capture_next(Box::new(capture));
            }
            None => log::warn!("[wine_overlay] could not set up frame capture for game {}", game_pid),
        }
    });
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
}

impl Capture {
    unsafe fn new(hwnd: HWND, game_pid: u32, webview: ICoreWebView2) -> Option<Self> {
        let stream = CreateStreamOnHGlobal(0, true).ok()?;
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let frames = FrameBlock::open(game_pid)?;
        Some(Self { hwnd, webview, stream, factory, frames })
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
fn capture_next(mut capture: Box<Capture>) {
    unsafe {
        if IsWindow(capture.hwnd) == 0 {
            return; // overlay closed: dropping the capture unmaps the block
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
        if let Err(e) = webview.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler) {
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
