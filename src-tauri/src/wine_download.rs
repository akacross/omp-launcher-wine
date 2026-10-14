// Downloads under Wine, through WinHTTP.
//
// tauri-plugin-upload's download (reqwest on tokio) stalls under Wine just short
// of the end: the read for the last few kilobytes never wakes (tokio's sockets
// sit on Wine's AFD poll, as query.rs describes), so the loading screen waited
// on "Downloading OMP plugin..." with 1982464 of omp-client-7.dll's 2008816
// bytes on disk. WinHTTP is Wine's own HTTP stack, with plain blocking sockets.
// src/utils/download.ts uses this only under Wine; elsewhere the plugin's.

use serde::Serialize;
use tauri::Window;

#[derive(Clone, Serialize)]
struct ProgressPayload {
    id: u32,
    progress: u64,
    total: u64,
}

#[tauri::command]
pub fn is_wine() -> bool {
    #[cfg(windows)]
    return crate::wine_overlay::is_wine();
    #[cfg(not(windows))]
    return false;
}

/// The plugin's download, in WinHTTP: same arguments, and the same progress
/// events under "wine_download://progress".
#[tauri::command]
pub async fn wine_download(
    window: Window,
    id: u32,
    url: String,
    file_path: String,
) -> std::result::Result<u32, String> {
    tokio::task::spawn_blocking(move || {
        download(&url, &file_path, |progress, total| {
            let _ = window.emit(
                "wine_download://progress",
                ProgressPayload {
                    id,
                    progress,
                    total,
                },
            );
        })
        .map_err(|e| {
            log::error!("[wine_download] {}: {}", url, e);
            e
        })
    })
    .await
    .map_err(|e| format!("download task failed: {}", e))??;
    Ok(id)
}

#[cfg(not(windows))]
fn download(_url: &str, _file_path: &str, _progress: impl FnMut(u64, u64)) -> Result<(), String> {
    Err("WinHTTP downloads exist only on Windows".to_string())
}

#[cfg(windows)]
fn download(url: &str, file_path: &str, mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
    use std::io::Write;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Networking::WinHttp::*;

    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
    fn check(handle: *mut core::ffi::c_void, what: &str) -> Result<Handle, String> {
        if handle.is_null() {
            Err(format!("{} failed: error {}", what, unsafe { GetLastError() }))
        } else {
            Ok(Handle(handle))
        }
    }
    fn ok(result: i32, what: &str) -> Result<(), String> {
        if result == 0 {
            Err(format!("{} failed: error {}", what, unsafe { GetLastError() }))
        } else {
            Ok(())
        }
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!("not an http(s) URL: {}", url));
    };
    let (authority, object) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (host, port.parse().unwrap()),
        _ => (authority, if secure { 443 } else { 80 }),
    };

    unsafe {
        let session = check(
            WinHttpOpen(
                wide("omp-launcher").as_ptr(),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            ),
            "WinHttpOpen",
        )?;
        // a stalled transfer ends in an error instead of a wait without end
        ok(
            WinHttpSetTimeouts(session.0, 0, 15_000, 15_000, 30_000),
            "WinHttpSetTimeouts",
        )?;
        let connection = check(
            WinHttpConnect(session.0, wide(host).as_ptr(), port, 0),
            "WinHttpConnect",
        )?;
        let request = check(
            WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                wide(object).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                if secure { WINHTTP_FLAG_SECURE } else { 0 },
            ),
            "WinHttpOpenRequest",
        )?;
        ok(
            WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0),
            "WinHttpSendRequest",
        )?;
        ok(
            WinHttpReceiveResponse(request.0, std::ptr::null_mut()),
            "WinHttpReceiveResponse",
        )?;

        let query_number = |info: u32| -> Option<u32> {
            let mut value: u32 = 0;
            let mut size = std::mem::size_of::<u32>() as u32;
            let found = WinHttpQueryHeaders(
                request.0,
                info | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut value as *mut u32 as *mut core::ffi::c_void,
                &mut size,
                std::ptr::null_mut(),
            );
            (found != 0).then_some(value)
        };
        let status = query_number(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if status != 200 {
            return Err(format!("HTTP status {}", status));
        }
        // 0 when the server sends no Content-Length, as with the plugin
        let total = query_number(WINHTTP_QUERY_CONTENT_LENGTH).unwrap_or(0) as u64;

        let file = std::fs::File::create(file_path)
            .map_err(|e| format!("cannot create {}: {}", file_path, e))?;
        let mut file = std::io::BufWriter::new(file);
        let mut buffer = vec![0u8; 64 * 1024];
        let mut received: u64 = 0;
        loop {
            let mut read: u32 = 0;
            ok(
                WinHttpReadData(
                    request.0,
                    buffer.as_mut_ptr() as *mut core::ffi::c_void,
                    buffer.len() as u32,
                    &mut read,
                ),
                "WinHttpReadData",
            )?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read as usize])
                .map_err(|e| format!("cannot write {}: {}", file_path, e))?;
            received += read as u64;
            progress(read as u64, total);
        }
        file.flush()
            .map_err(|e| format!("cannot write {}: {}", file_path, e))?;
        if total > 0 && received != total {
            return Err(format!("received {} of {} bytes", received, total));
        }
        log::info!("[wine_download] {}: {} bytes", url, received);
        Ok(())
    }
}
