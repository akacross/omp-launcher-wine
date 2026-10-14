/* ompoverlay.dll - open.mp's in-game browser under Wine, the game's side.
 *
 * From ompoverlay.asi (https://github.com/akacross/omp-ingame-browser-wine,
 * MIT); build.rs compiles it into the launcher, and under Wine the launcher
 * injects it into a game once its omp-client connects (wine_overlay.rs), so
 * nothing needs installing in the game folder.
 *
 * omp-client.dll draws the in-game browser by copying the launcher's hidden
 * overlay window (title "omp_overlay_window:<game pid>") with PrintWindow
 * every frame. Under Wine that copy is always empty: Wine implements
 * PrintWindow by sending WM_PRINT with the caller's DC to the window's owner,
 * and the launcher (another process) cannot draw into it.
 *
 * The launcher publishes the overlay's pixels in shared memory instead
 * (wine_overlay.rs): Local\omp_overlay_frame_<pid>, a 32-byte header (u32
 * magic 'OMPF', version, width, height, stride, seq - odd while a frame is
 * written -, pid, reserved) and BGRA rows, top-down.
 *
 * This points omp-client.dll's PrintWindow import at a function that copies
 * that frame into omp-client's bitmap. Any other window, or no frames, goes to
 * the real PrintWindow.
 *
 * The ASI also worked around omp-client 0.2.2 (launcher v1.6) making no new
 * bitmap for an overlay reopened at the same size; the client of launcher
 * v1.7 does, so that part is gone.
 *
 * And it keeps the game from crashing as it quits: see release_exit_thread.
 */
#include <windows.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define FRAME_MAGIC   0x46504D4Fu /* "OMPF" */
#define FRAME_HEADER  32
#define OVERLAY_TITLE "omp_overlay_window:"
#define TARGET_DLL    "omp-client.dll"

typedef BOOL (WINAPI *PrintWindow_fn)(HWND, HDC, UINT);

static PrintWindow_fn real_print_window;
static HANDLE frame_mapping;
static const volatile unsigned *frame; /* the mapped block */
static unsigned frame_pid;
static unsigned last_seq = 0xFFFFFFFFu;
static HGDIOBJ last_bmp;               /* the DIB last filled; omp-client keeps it between frames */
static void *last_bits;
static HGDIOBJ probed_bmp;             /* the DIB whose row order was last probed */
static int probed_top_down;
static volatile DWORD *exit_thread_id; /* omp-client's global std::thread's id (release_exit_thread) */

static void log_line(const char *fmt, ...)
{
    char buf[256] = "[ompoverlay] ";
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf + 13, sizeof buf - 14, fmt, ap);
    va_end(ap);
    strcat(buf, "\n");
    OutputDebugStringA(buf);
}

static int open_frames(unsigned pid)
{
    if (frame && frame_pid == pid)
        return 1;
    if (frame) {
        UnmapViewOfFile((LPCVOID)frame);
        CloseHandle(frame_mapping);
        frame = NULL;
    }
    char name[64];
    snprintf(name, sizeof name, "Local\\omp_overlay_frame_%u", pid);
    frame_mapping = OpenFileMappingA(FILE_MAP_READ, FALSE, name);
    if (!frame_mapping)
        return 0;
    frame = MapViewOfFile(frame_mapping, FILE_MAP_READ, 0, 0, 0);
    if (!frame) {
        CloseHandle(frame_mapping);
        return 0;
    }
    frame_pid = pid;
    last_seq = 0xFFFFFFFFu;
    log_line("reading overlay frames from %s", name);
    return 1;
}

/* Whether the DIB's first row in memory is its top row. Wine reports a
 * positive dsBmih.biHeight even for top-down DIBs, so ask GDI instead: set the
 * top-left pixel and see whether memory row 0 changed. The frame copy that
 * follows overwrites the top-left pixel; memory row 0 of a bottom-up DIB is
 * put back. */
static int row_order(HDC hdc, HGDIOBJ bmp, const DIBSECTION *ds)
{
    if (bmp == probed_bmp)
        return probed_top_down;
    volatile unsigned *first = ds->dsBm.bmBits;
    unsigned saved = *first;
    *first = 0;
    SetPixelV(hdc, 0, 0, RGB(1, 2, 3));
    GdiFlush();
    probed_top_down = (*first & 0xFFFFFFu) == 0x010203u;
    if (!probed_top_down)
        *first = saved;
    probed_bmp = bmp;
    return probed_top_down;
}

/* Copy the newest frame into the 32-bit DIB selected into hdc. Returns 0 when
 * there is nothing to serve (no block, wrong bitmap), so the caller falls back. */
static int copy_frame(unsigned pid, HDC hdc)
{
    static int told;   /* say once why frames are not served, not every frame */
    if (!open_frames(pid))
        return 0;
    if (frame[0] != FRAME_MAGIC) {
        if (!told++) log_line("frame block has no 'OMPF' magic (%08X); not serving", frame[0]);
        return 0;
    }

    DIBSECTION ds;
    HGDIOBJ bmp = GetCurrentObject(hdc, OBJ_BITMAP);
    int got = bmp ? GetObjectA(bmp, sizeof ds, &ds) : 0;
    if (got != (int)sizeof ds || !ds.dsBm.bmBits || ds.dsBm.bmBitsPixel != 32) {
        if (!told++) log_line("the capture DC holds no 32-bit DIB section (bitmap %p, GetObject %d); not serving", bmp, got);
        return 0;
    }

    unsigned seq = frame[5];
    if (seq & 1)
        return 1;   /* mid-write: keep the picture already in the DIB */
    if (seq == last_seq && bmp == last_bmp && ds.dsBm.bmBits == last_bits)
        return 1;   /* unchanged since the last copy into this very DIB */

    unsigned w = frame[2], h = frame[3], stride = frame[4];
    unsigned dw = (unsigned)ds.dsBm.bmWidth, dh = (unsigned)ds.dsBm.bmHeight;
    unsigned cw = w < dw ? w : dw, ch = h < dh ? h : dh;
    int top_down = row_order(hdc, bmp, &ds);
    unsigned char *dst = ds.dsBm.bmBits;
    const unsigned char *src = (const unsigned char *)frame + FRAME_HEADER;
    for (unsigned y = 0; y < ch; y++) {
        unsigned dy = top_down ? y : dh - 1 - y;
        memcpy(dst + (size_t)dy * ds.dsBm.bmWidthBytes, src + (size_t)y * stride, (size_t)cw * 4);
    }

    if (getenv("OMPOVERLAY_DEBUG"))
        log_line("copied %ux%u of a %ux%u frame into a %ux%u DIB (%s), src[0]=%08X dst[0]=%08X bits=%p",
                 cw, ch, w, h, dw, dh, top_down ? "top-down" : "bottom-up", ((const unsigned *)src)[0],
                 *(unsigned *)dst, dst);
    if (frame[5] != seq) {
        last_seq = 0xFFFFFFFFu;  /* the launcher wrote meanwhile: copy again next frame */
        return 1;
    }
    last_seq = seq;
    last_bmp = bmp;
    last_bits = ds.dsBm.bmBits;
    GdiFlush();
    return 1;
}

static BOOL WINAPI hooked_print_window(HWND hwnd, HDC hdc, UINT flags)
{
    char title[64];
    if (GetWindowTextA(hwnd, title, sizeof title) > 0 && strncmp(title, OVERLAY_TITLE, sizeof OVERLAY_TITLE - 1) == 0) {
        unsigned pid = (unsigned)strtoul(title + sizeof OVERLAY_TITLE - 1, NULL, 10);
        if (pid && copy_frame(pid, hdc))
            return TRUE;
    }
    return real_print_window(hwnd, hdc, flags);
}

/* Find USER32!PrintWindow in the module's import table and point it at ours. */
static int patch_import(HMODULE module)
{
    unsigned char *base = (unsigned char *)module;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
    IMAGE_DATA_DIRECTORY dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!dir.VirtualAddress)
        return 0;
    for (IMAGE_IMPORT_DESCRIPTOR *d = (IMAGE_IMPORT_DESCRIPTOR *)(base + dir.VirtualAddress); d->Name; d++) {
        if (_stricmp((const char *)base + d->Name, "USER32.dll") != 0)
            continue;
        IMAGE_THUNK_DATA *names = (IMAGE_THUNK_DATA *)(base + (d->OriginalFirstThunk ? d->OriginalFirstThunk : d->FirstThunk));
        IMAGE_THUNK_DATA *slots = (IMAGE_THUNK_DATA *)(base + d->FirstThunk);
        for (; names->u1.AddressOfData; names++, slots++) {
            if (IMAGE_SNAP_BY_ORDINAL(names->u1.Ordinal))
                continue;
            IMAGE_IMPORT_BY_NAME *by_name = (IMAGE_IMPORT_BY_NAME *)(base + names->u1.AddressOfData);
            if (strcmp((const char *)by_name->Name, "PrintWindow") != 0)
                continue;
            DWORD old;
            if (!VirtualProtect(&slots->u1.Function, sizeof slots->u1.Function, PAGE_READWRITE, &old))
                return 0;
            real_print_window = (PrintWindow_fn)slots->u1.Function;
            slots->u1.Function = (ULONG_PTR)hooked_print_window;
            VirtualProtect(&slots->u1.Function, sizeof slots->u1.Function, old, &old);
            return 1;
        }
    }
    return 0;
}

/* Find `pattern` ("83 3D ?? ..") in the module's code sections; the match, if
 * exactly one, else NULL. */
static unsigned char *find_code(HMODULE module, const char *pattern)
{
    unsigned char want[64], mask[64];
    int n = 0;
    for (const char *p = pattern; *p && n < 64; ) {
        if (*p == ' ') { p++; continue; }
        if (p[0] == '?') { want[n] = 0; mask[n++] = 0; p += 2; continue; }
        char hex[3] = {p[0], p[1], 0};
        want[n] = (unsigned char)strtoul(hex, NULL, 16); mask[n++] = 1; p += 2;
    }
    unsigned char *base = (unsigned char *)module, *found = NULL;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
    IMAGE_SECTION_HEADER *sec = IMAGE_FIRST_SECTION(nt);
    for (int i = 0; i < nt->FileHeader.NumberOfSections; i++, sec++) {
        if (!(sec->Characteristics & IMAGE_SCN_CNT_CODE))
            continue;
        unsigned char *start = base + sec->VirtualAddress;
        DWORD size = sec->Misc.VirtualSize;
        for (DWORD at = 0; at + n <= size; at++) {
            int k = 0;
            while (k < n && (!mask[k] || start[at + k] == want[k]))
                k++;
            if (k == n) {
                if (found)
                    return NULL;   /* not unique */
                found = start + at;
            }
        }
    }
    return found;
}

/* Whether [p, p+size) lies in one of the module's writable sections. */
static int in_writable_section(HMODULE module, const void *p, size_t size)
{
    unsigned char *base = (unsigned char *)module;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
    IMAGE_SECTION_HEADER *sec = IMAGE_FIRST_SECTION(nt);
    for (int i = 0; i < nt->FileHeader.NumberOfSections; i++, sec++) {
        const unsigned char *start = base + sec->VirtualAddress;
        if ((sec->Characteristics & IMAGE_SCN_MEM_WRITE) && (const unsigned char *)p >= start &&
            (const unsigned char *)p + size <= start + sec->Misc.VirtualSize)
            return 1;
    }
    return 0;
}

/* omp-client of launcher v1.7 keeps a std::thread in a global: its socket
 * thread to the launcher. The cleanup that stops and joins it is registered at
 * load and does not run on every way out of the game, and then, as the DLL
 * unloads, that global's destructor finds the thread still joinable and calls
 * std::terminate: abort(), __fastfail(7), 0xC0000409 on quitting. The
 * destructor (0x10148930 in that build) is
 *   cmp dword [thread id], 0 / jne std::terminate / ret
 * By the time any DLL hears that the process is ending, ExitProcess has ended
 * every other thread, that one included; and this DLL, loaded after
 * omp-client, hears it first. So it marks the global as holding no thread
 * (handle and id 0, as a detach leaves it) and the destructor returns. */
#define EXIT_THREAD_DTOR "83 3D ?? ?? ?? ?? 00 0F 85 ?? ?? ?? ?? C3"

static void find_exit_thread(HMODULE omp)
{
    unsigned char *dtor = find_code(omp, EXIT_THREAD_DTOR);
    volatile DWORD *id = dtor ? *(volatile DWORD **)(dtor + 2) : NULL;
    if (!id || !in_writable_section(omp, (const void *)(id - 1), 2 * sizeof(DWORD))) {
        log_line("%s's thread destructor is not the one this plugin knows; quitting may still end in abort()", TARGET_DLL);
        return;
    }
    exit_thread_id = id;
    log_line("%s keeps its launcher thread at %p; it is released when the game quits", TARGET_DLL, (void *)(id - 1));
}

static void release_exit_thread(void)
{
    volatile DWORD *id = exit_thread_id;
    if (!id || !*id)
        return;
    log_line("the game is quitting with %s's launcher thread (id %lu) still held; released, so its destructor does not abort()",
             TARGET_DLL, (unsigned long)*id);
    id[-1] = 0;   /* the handle: the process is ending, nothing to close */
    id[0] = 0;
}

/* The launcher injects this once omp-client has connected to it, so
 * omp-client.dll is loaded already; wait for it all the same. */
static DWORD WINAPI watch(void *unused)
{
    (void)unused;
    for (int i = 0; i < 1200; i++) {   /* two minutes */
        HMODULE omp = GetModuleHandleA(TARGET_DLL);
        if (omp) {
            find_exit_thread(omp);
            /* an ompoverlay.asi from before already serves the frames */
            if (GetModuleHandleA("ompoverlay.asi")) {
                log_line("ompoverlay.asi is loaded and serves the frames; it is no longer needed");
                return 0;
            }
            if (patch_import(omp))
                log_line("PrintWindow of %s now serves overlay frames from shared memory", TARGET_DLL);
            else
                log_line("%s imports no PrintWindow; nothing to do", TARGET_DLL);
            return 0;
        }
        Sleep(100);
    }
    return 0;
}

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, void *reserved)
{
    if (reason == DLL_PROCESS_ATTACH) {
        DisableThreadLibraryCalls(inst);
        HANDLE t = CreateThread(NULL, 0, watch, NULL, 0, NULL);
        if (t)
            CloseHandle(t);
    } else if (reason == DLL_PROCESS_DETACH && reserved) {
        /* the process is ending (not a FreeLibrary) */
        release_exit_thread();
    }
    return TRUE;
}
