//! The Win32 side of the splash: one layered popup window, its DIB, and its message loop, all
//! owned by one dedicated thread that exists only while the splash is on screen.
//!
//! # Shape
//! [`spawn`] starts the thread and returns immediately; everything below the thread boundary is
//! plain Win32 and every handle is released by a guard, so a failure at any step frees what it
//! acquired and reports it through the seam (which reveals the shell at once).
//!
//! The window is created **hidden**, presented once, and only then shown with
//! `SWP_NOACTIVATE`; `SetForegroundWindow` is never called, so the splash cannot steal focus from
//! whatever the user is doing while the app starts. It is `WS_POPUP` (no frame, no taskbar
//! entry beyond `WS_EX_TOOLWINDOW`), `WS_EX_TOPMOST` (it must not slide behind other windows
//! during startup) and `WS_EX_LAYERED` (per-pixel alpha through `UpdateLayeredWindow`).
//! Pixels where the art is transparent are not hit-tested by Windows for layered windows, so the
//! desktop around the artwork stays clickable without extra work.
//!
//! # Scaling
//! The art is authored for 960x600 and presented at an **integer** multiple of that, chosen by an
//! integer nearest-neighbour rule from the target monitor's effective DPI; the source buffer is
//! scaled here, into the DIB, with no interpolation anywhere, so pixel art stays crisp. Nothing
//! is drawn by GDI and nothing is stretched by the compositor.
//!
//! # DPI query
//! `GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, ..)` is the query, and it has to be a
//! *monitor* query: the integer scale decides the window's size, so the size must be known
//! before the window exists and `GetDpiForWindow` cannot be used. `MDT_EFFECTIVE_DPI` is the DPI
//! Windows uses to scale content on that monitor (it follows the user's scale percentage), which
//! is exactly the number the art has to match. Any failure falls back to 96 dpi, i.e. 1x.
//!
//! # Lifetime
//! The thread is **detached**: `spawn` drops the `JoinHandle` and nothing ever joins. Joining
//! would mean blocking the UI thread (or the close path) on an animation thread, for no benefit:
//! the window is hidden and destroyed by the thread that created it before it returns, no lock
//! outlives it, and a process exit terminates a detached thread instead of waiting for it, so
//! shutdown cannot hang on the splash. The splash thread also never blocks for long: it pumps its
//! message queue and sleeps in slices of a few milliseconds, and it ends as soon as the timeline
//! stops drawing or a stop is requested.
//!
//! # Close
//! `WM_CLOSE` destroys this window only, and `WM_DESTROY` is not turned into a quit message. The
//! frame loop notices the window is gone and reports it through the seam, so the shell appears
//! instead of the app being left invisible behind nothing, and the app's own close (a
//! `CloseRequested` on the main window) is handled by `main.rs`, never here.

use super::{
    art::{self, ArtOptions},
    timeline::Reveal,
    Sink, Shared,
};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
        SIZE, WPARAM,
    },
    Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetMonitorInfoW, MonitorFromPoint,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
        MONITORINFO, MONITOR_DEFAULTTOPRIMARY, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, IsWindow, MSG,
            PeekMessageW, RegisterClassW, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
            UnregisterClassW, UpdateLayeredWindow, HWND_TOPMOST, PM_REMOVE, SPI_GETCLIENTAREAANIMATION,
            SPI_GETHIGHCONTRAST, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WM_CLOSE, WM_DESTROY,
            WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    },
};

/// The window class. Registered on the splash thread and unregistered when it ends, so the class
/// exists exactly as long as a splash window can.
const CLASS_NAME: &str = "agent-workbench-splash";

/// The frame budget: about 60 frames per second, which is the rate the timeline's animation is
/// authored for.
const FRAME: Duration = Duration::from_millis(16);

/// How long a single sleep inside the frame budget may last. Short slices keep the loop
/// responsive to a stop request and to a destroyed window without spinning.
const SLICE: Duration = Duration::from_millis(4);

/// The DPI the art is authored against: 100% scaling.
const BASE_DPI: u32 = 96;

/// Upper bound on the integer scale. The art is 960x600, so 4x is a 3840x2400 surface; beyond
/// that a 4K-plus display with extreme scaling would allocate an enormous DIB for an animation
/// that is over in under three seconds. Clamping costs sharpness on a display nobody has.
const MAX_SCALE: i32 = 4;

/// `HCF_HIGHCONTRASTON` from `WINUSER.H`; the only flag of `HIGHCONTRASTW.dwFlags` that matters
/// here.
const HCF_HIGHCONTRASTON: u32 = 1;

/// The live splash window thread, as seen by the seam: just the flag that asks it to stop.
pub(crate) struct Win32Sink {
    stop: Arc<AtomicBool>,
}

impl Sink for Win32Sink {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Start the window thread and return at once, or `None` when the thread cannot be started (the
/// caller then fails open).
///
/// The handle is deliberately dropped: see the module docs on why the thread is detached.
pub(crate) fn spawn(shared: Arc<Shared>) -> Option<Win32Sink> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("splash-window".to_string())
        .spawn(move || run(shared, thread_stop))
        .ok()?;
    Some(Win32Sink { stop })
}

/// The thread body: open the window, run it, and report any failure through the seam so the
/// caller reveals the shell instead of waiting for a splash that never appears.
fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    if open(&shared, &stop).is_err() {
        shared.failed();
    }
}

/// Where the splash window sits, in physical pixels.
#[derive(Clone, Copy)]
struct Placement {
    at: POINT,
    size: Size,
}

/// The window size in physical pixels.
#[derive(Clone, Copy)]
struct Size {
    width: i32,
    height: i32,
}

/// The whole window lifetime. Every handle is owned by a guard, so any `?` below frees exactly
/// what was acquired and nothing else.
fn open(shared: &Arc<Shared>, stop: &AtomicBool) -> Result<(), ()> {
    let options = probe_options(shared);
    let monitor = target_monitor().ok_or(())?;
    let scale = scale_for(monitor.dpi);
    let size = Size {
        width: art::W.saturating_mul(scale),
        height: art::H.saturating_mul(scale),
    };
    let placement = Placement {
        at: centered(monitor.work, size),
        size,
    };
    let class = Class::register()?;
    let mut surface = Surface::new(size)?;
    let window = SplashWindow::create(&class, placement)?;
    // The first present happens while the window is still hidden: if the layered surface cannot
    // be pushed at all, the splash is broken and must not make the shell wait for it.
    window.present(&surface, placement)?;
    window.show(placement)?;
    // Dropping order on the way out is the reverse of declaration order: the window is hidden
    // and destroyed first, then the DIB, then the class.
    loop_frames(&window, &mut surface, shared, stop, placement, scale, options)
}

/// Draw, present and tick about sixty times a second until the splash should go.
fn loop_frames(
    window: &SplashWindow,
    surface: &mut Surface,
    shared: &Shared,
    stop: &AtomicBool,
    placement: Placement,
    scale: i32,
    options: ArtOptions,
) -> Result<(), ()> {
    let mut frame = vec![0u32; art::PIXEL_COUNT];
    loop {
        let frame_started = Instant::now();
        pump();
        if stop.load(Ordering::Acquire) {
            // The app asked for the splash to go (it is closing, or it revealed by another path).
            return Ok(());
        }
        if !window.is_alive() {
            // The window is gone: a user close, or the system destroying it. Report it so the
            // shell appears rather than the app staying invisible behind a splash that is not
            // there. The app's own close is unaffected: this is not the main window.
            shared.window_closed();
            return Ok(());
        }
        let now = shared.now();
        // The tick is what turns the 4000 ms deadline into a reveal; the reveal callback runs on
        // this thread when the deadline is what fires it, which is why the caller marshals.
        if shared.tick() != Reveal::Wait {
            return Ok(());
        }
        if !shared.is_showing() {
            return Ok(());
        }
        art::draw_frame(&mut frame, now, options);
        surface.blit(&frame, scale);
        window.present(surface, placement)?;
        wait_for_next_frame(frame_started, shared, stop);
    }
}

/// Sleep out the rest of the frame budget in small slices, staying responsive: the queue is
/// pumped and the stop flag checked between slices, so a stop is noticed within `SLICE`.
fn wait_for_next_frame(frame_started: Instant, shared: &Shared, stop: &AtomicBool) {
    loop {
        pump();
        if stop.load(Ordering::Acquire) || !shared.is_showing() {
            return;
        }
        let elapsed = frame_started.elapsed();
        if elapsed >= FRAME {
            return;
        }
        std::thread::sleep((FRAME - elapsed).min(SLICE));
    }
}

/// Drain this thread's message queue without blocking.
///
/// The splash takes no input, but messages are only delivered when they are dispatched:
/// without this, `WM_CLOSE` would never reach the window procedure and the loop would never
/// notice that the window had died.
fn pump() {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a live, correctly sized MSG; a null window with a zero filter
        // range asks for every queued message of this thread, and `PM_REMOVE` takes it out of
        // the queue, so this loop always terminates.
        let has_message = unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) };
        if has_message == 0 {
            return;
        }
        // SAFETY: the message was filled in by `PeekMessageW` above and is dispatched exactly
        // once; the window procedure never unwinds across the FFI boundary.
        unsafe { TranslateMessage(&message) };
        // SAFETY: as above, for the same message.
        unsafe { DispatchMessageW(&message) };
    }
}

/// The window procedure. The splash takes no input, so the only two messages that matter are the
/// close pair.
unsafe extern "system" fn wndproc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_CLOSE => {
            // Destroyed here rather than through `DefWindowProcW`, which would only reach the
            // same place by way of WM_DESTROY. This destroys the splash window alone: the app's
            // own close is a `CloseRequested` on the main window and is handled in `main.rs`.
            // The frame loop then sees the window is gone and reports it.
            // SAFETY: `window` is the live window this procedure belongs to.
            unsafe { DestroyWindow(window) };
            0
        }
        WM_DESTROY => 0,
        _ => {
            // SAFETY: the arguments are exactly the ones this procedure received, and
            // `DefWindowProcW` is the correct fallback for every other message.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
    }
}

/// The window class registration, unregistered by the guard as soon as the window is gone.
struct Class {
    name: Vec<u16>,
    instance: HINSTANCE,
    /// Whether this object registered the class, i.e. whether it must unregister it.
    owner: bool,
}

impl Class {
    fn register() -> Result<Self, ()> {
        let name = wide(CLASS_NAME);
        // SAFETY: a null module name asks for the module of this executable and reads nothing.
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err(());
        }
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: name.as_ptr(),
            ..WNDCLASSW::default()
        };
        // SAFETY: `class` is fully initialised, `name` is NUL-terminated UTF-16 that outlives the
        // call, and `wndproc` has the `extern "system"` signature the class requires.
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            // SAFETY: `GetLastError` has no preconditions, and it is read before any other call
            // could overwrite the status the registration just reported.
            let status = unsafe { GetLastError() };
            if status != ERROR_CLASS_ALREADY_EXISTS {
                return Err(());
            }
            // The class of this name already exists in this process (a replay raced the teardown
            // of the previous generation). It is this module's class, so the window can still be
            // created, but this generation must not unregister a class it did not register.
            return Ok(Self { name, instance, owner: false });
        }
        Ok(Self { name, instance, owner: true })
    }
}

impl Drop for Class {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        // SAFETY: the class was registered by this object with exactly this name and instance,
        // and the window that used it has already been destroyed.
        unsafe { UnregisterClassW(self.name.as_ptr(), self.instance) };
    }
}

/// The splash window itself: hidden and destroyed by the guard.
struct SplashWindow {
    handle: HWND,
}

impl SplashWindow {
    fn create(class: &Class, placement: Placement) -> Result<Self, ()> {
        // SAFETY: `class.name` is the NUL-terminated class name registered just above. No
        // `WS_VISIBLE`, so the window cannot appear until `show`; no parent, so it is a
        // top-level popup; `lpparam` is null and the procedure reads nothing from it.
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class.name.as_ptr(),
                class.name.as_ptr(),
                WS_POPUP,
                placement.at.x,
                placement.at.y,
                placement.size.width,
                placement.size.height,
                null_mut(),
                null_mut(),
                class.instance,
                null(),
            )
        };
        if handle.is_null() {
            return Err(());
        }
        Ok(Self { handle })
    }

    /// Push the DIB to the screen. `UpdateLayeredWindow` takes the whole surface and the blend
    /// mode; `AC_SRC_ALPHA` with `SourceConstantAlpha` 255 means "use the per-pixel alpha the art
    /// already carries".
    fn present(&self, surface: &Surface, placement: Placement) -> Result<(), ()> {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let source = POINT::default();
        let size = SIZE {
            cx: placement.size.width,
            cy: placement.size.height,
        };
        let key: COLORREF = 0;
        // SAFETY: a null destination DC asks for the screen; `surface.dc` holds a live 32-bit
        // top-down DIB of exactly `size`, which is what `ULW_ALPHA` requires; the blend function
        // and the source point are live locals for the whole call.
        let presented = unsafe {
            UpdateLayeredWindow(
                self.handle,
                null_mut(),
                &placement.at,
                &size,
                surface.dc,
                &source,
                key,
                &blend,
                ULW_ALPHA,
            )
        };
        if presented == 0 {
            return Err(());
        }
        Ok(())
    }

    /// Put the window on screen without activating it. `SWP_NOACTIVATE` keeps the keyboard focus
    /// wherever it was; `SetForegroundWindow` is never called, by design. A refusal is treated as
    /// a failure: a splash that never appears must not hide the shell behind it either.
    fn show(&self, placement: Placement) -> Result<(), ()> {
        // SAFETY: `self.handle` is the live window created above; the window is already
        // `WS_EX_TOPMOST`, and asking for `HWND_TOPMOST` again with `SWP_NOACTIVATE` does not
        // activate it either.
        let shown = unsafe {
            SetWindowPos(
                self.handle,
                HWND_TOPMOST,
                placement.at.x,
                placement.at.y,
                placement.size.width,
                placement.size.height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        if shown == 0 {
            return Err(());
        }
        Ok(())
    }

    fn is_alive(&self) -> bool {
        // SAFETY: `IsWindow` only reads a window handle and reports zero for a destroyed one.
        unsafe { IsWindow(self.handle) != 0 }
    }

    /// Hide the window, then destroy it. Returns whether `DestroyWindow` succeeded, and is
    /// idempotent: the handle is cleared in every case, so a second call (or a `Drop` after an
    /// explicit call) finds nothing to take down and returns `false`.
    ///
    /// This is the `Drop` body as a method so the opt-in window test can assert the teardown's
    /// own result instead of inferring it from a copy of this code; `Drop` still ignores that
    /// result, exactly as before.
    fn close(&mut self) -> bool {
        if self.handle.is_null() {
            return false;
        }
        // SAFETY: `IsWindow` only reads a window handle. A zero means the window was already
        // destroyed (a close, or the system), so there is nothing left to take down and the
        // handle must not be passed to `DestroyWindow` again.
        if unsafe { IsWindow(self.handle) } == 0 {
            self.handle = null_mut();
            return false;
        }
        // Hide first, then destroy: the ruled teardown order, so the topmost window is off the
        // screen before anything else happens.
        // SAFETY: the handle is the live window this object created; `ShowWindow` and
        // `DestroyWindow` only touch it, and it is never used again afterwards.
        unsafe { ShowWindow(self.handle, SW_HIDE) };
        // SAFETY: as above, and this is the only `DestroyWindow` for this handle.
        let destroyed = unsafe { DestroyWindow(self.handle) } != 0;
        self.handle = null_mut();
        destroyed
    }
}

impl Drop for SplashWindow {
    fn drop(&mut self) {
        // The result is deliberately ignored: the window is being taken down either way.
        self.close();
    }
}

/// The memory DC and its 32-bit top-down DIB, released by the guard.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    /// The object `SelectObject` displaced; it goes back before the DC and the bitmap are freed.
    previous: HGDIOBJ,
    /// `CreateDIBSection`'s pixel storage, `width * height` values of 32 bits.
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl Surface {
    fn new(size: Size) -> Result<Self, ()> {
        if size.width <= 0 || size.height <= 0 {
            return Err(());
        }
        // SAFETY: a null DC asks for a memory DC compatible with the current screen; nothing is
        // read through the argument.
        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        if dc.is_null() {
            return Err(());
        }
        let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size.width,
            // Negative height: a top-down DIB, so row 0 is the top row and the buffer can be
            // filled in the same order the artwork is rasterised in.
            biHeight: -size.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        }, ..Default::default() };
        let mut bits: *mut c_void = null_mut();
        // SAFETY: `info` is a fully initialised BITMAPINFO for a 32-bit `BI_RGB` surface, `bits`
        // is a live out-parameter, and a null section handle asks the system for the storage.
        let bitmap = unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        if bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                // SAFETY: the bitmap was created here and is not selected into any DC.
                unsafe { DeleteObject(bitmap as HGDIOBJ) };
            }
            // SAFETY: the DC was created here and nothing else refers to it.
            unsafe { DeleteDC(dc) };
            return Err(());
        }
        // SAFETY: both handles are live and belong to this surface; the displaced object is kept
        // so it can be selected back before either is deleted.
        let previous = unsafe { SelectObject(dc, bitmap as HGDIOBJ) };
        if previous.is_null() {
            // SAFETY: a failure path with no live reference to either handle.
            unsafe { DeleteObject(bitmap as HGDIOBJ) };
            // SAFETY: as above.
            unsafe { DeleteDC(dc) };
            return Err(());
        }
        Ok(Self {
            dc,
            bitmap,
            previous,
            bits: bits as *mut u32,
            width: size.width,
            height: size.height,
        })
    }

    /// Scale `source` (the art's `PIXEL_COUNT` buffer) into the DIB with nearest-neighbour
    /// sampling: destination pixel `x` is source pixel `x / scale`, so every destination pixel is
    /// a copy of exactly one source pixel and nothing is interpolated.
    fn blit(&mut self, source: &[u32], scale: i32) {
        if source.len() != art::PIXEL_COUNT {
            return;
        }
        let Ok(scale) = usize::try_from(scale) else {
            return;
        };
        let (Ok(source_w), Ok(source_h)) = (usize::try_from(art::W), usize::try_from(art::H)) else {
            return;
        };
        let (Ok(width), Ok(height)) = (usize::try_from(self.width), usize::try_from(self.height)) else {
            return;
        };
        let Some(pixels) = width.checked_mul(height) else {
            return;
        };
        if scale == 0 || pixels == 0 || width != source_w.saturating_mul(scale) {
            return;
        }
        // SAFETY: `CreateDIBSection` allocated `width * height` 32-bit values and `self.bits`
        // points at them; the surface owns that storage for as long as this `Surface` lives, and
        // no other reference to it exists here.
        let destination = unsafe { std::slice::from_raw_parts_mut(self.bits, pixels) };
        // One scaled row is built once and copied `scale` times, so the inner loop is a copy
        // rather than a division per pixel.
        let mut row = vec![0u32; width];
        for source_y in 0..source_h {
            let Some(source_row) = source.get(source_y * source_w..(source_y + 1) * source_w) else {
                return;
            };
            for (x, pixel) in row.iter_mut().enumerate() {
                *pixel = source_row.get(x / scale).copied().unwrap_or(0);
            }
            for repeat in 0..scale {
                let y = source_y * scale + repeat;
                let Some(line) = destination.get_mut(y * width..(y + 1) * width) else {
                    continue;
                };
                line.copy_from_slice(&row);
            }
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if !self.dc.is_null() {
            if !self.previous.is_null() {
                // SAFETY: the object `SelectObject` displaced from this DC, going back so the
                // bitmap is no longer selected into anything; the DC is still live.
                unsafe { SelectObject(self.dc, self.previous) };
            }
            if !self.bitmap.is_null() {
                // SAFETY: the bitmap was created here and is not selected into any DC now.
                unsafe { DeleteObject(self.bitmap as HGDIOBJ) };
            }
            // SAFETY: the last reference to this DC, with nothing selected into it.
            unsafe { DeleteDC(self.dc) };
            self.dc = null_mut();
            self.bitmap = null_mut();
        }
        self.previous = null_mut();
        self.bits = null_mut();
    }
}

/// The two system settings that change what is drawn, probed once at creation.
fn probe_options(shared: &Shared) -> ArtOptions {
    let reduced_motion = reduced_motion();
    // The timeline needs the same input: with reduced motion the artwork counts as complete.
    shared.set_reduced_motion(reduced_motion);
    ArtOptions {
        reduced_motion,
        high_contrast: high_contrast(),
    }
}

/// Reduced motion, from `SPI_GETCLIENTAREAANIMATION`: the "Show animations in Windows"
/// accessibility switch. It is read as "animations off means the user asked for less motion",
/// which is the setting Windows exposes for exactly that request; the animation then collapses to
/// its final frame while the splash still waits for the shell or the deadline.
fn reduced_motion() -> bool {
    let mut animations: i32 = 1;
    // SAFETY: `SPI_GETCLIENTAREAANIMATION` writes one `BOOL` through the pointer, which is a live
    // local of exactly that type, so the size and alignment are right.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            &mut animations as *mut i32 as *mut c_void,
            0,
        )
    };
    // A failed query leaves animations on: the animation is the default, and inventing
    // "reduced" from an unreadable setting would deny the art to everybody.
    read != 0 && animations == 0
}

/// `HIGHCONTRASTW` from `WINUSER.H`, mirrored locally.
///
/// The vendored definition lives in `windows_sys::Win32::UI::Accessibility::HIGHCONTRASTW`
/// (`cbSize: u32`, `dwFlags: u32`, `lpszDefaultScheme: PWSTR`), whose feature is not enabled;
/// adding that feature for one 16-byte struct would pull in the whole accessibility surface.
/// Hence the layout is copied verbatim from windows-sys 0.61.2 instead of being guessed.
#[repr(C)]
struct HighContrast {
    cb_size: u32,
    flags: u32,
    /// Never read here: the system fills it in (the high-contrast scheme's name).
    _default_scheme: *mut u16,
}

/// High contrast, from `SPI_GETHIGHCONTRAST`. When it is on, the art switches to an opaque
/// backdrop with brighter inks (see `art::Palette::HIGH_CONTRAST`).
fn high_contrast() -> bool {
    let mut info = HighContrast {
        cb_size: size_of::<HighContrast>() as u32,
        flags: 0,
        _default_scheme: null_mut(),
    };
    // SAFETY: `uiparam` is the size of the struct the pointer refers to, as the API requires, and
    // `info` is a live, correctly sized and aligned `HIGHCONTRASTW`-shaped buffer that this call
    // only writes to.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            info.cb_size,
            &mut info as *mut HighContrast as *mut c_void,
            0,
        )
    };
    read != 0 && info.flags & HCF_HIGHCONTRASTON != 0
}

/// The target monitor and its effective DPI.
struct Monitor {
    /// The work area: the desktop minus the taskbar and any other appbar, which is what the
    /// splash must be centred in.
    work: RECT,
    dpi: u32,
}

/// The monitor the splash appears on: the one under the cursor, i.e. where the user is looking
/// and clicking, falling back to the primary monitor. Then its **work** area, so the splash is
/// not centred in a region the taskbar covers, and its effective DPI.
fn target_monitor() -> Option<Monitor> {
    let mut cursor = POINT::default();
    // SAFETY: `GetCursorPos` writes one POINT into a live local.
    let read = unsafe { GetCursorPos(&mut cursor) };
    let point = if read != 0 { cursor } else { POINT::default() };
    // SAFETY: the point is passed by value and the call only returns a monitor handle, which is
    // never null here because of `MONITOR_DEFAULTTOPRIMARY`.
    let handle = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY) };
    if handle.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    // SAFETY: `cbSize` is set as the API requires and `info` is a live MONITORINFO.
    if unsafe { GetMonitorInfoW(handle, &mut info) } == 0 {
        return None;
    }
    let mut dpi_x: u32 = 0;
    let mut dpi_y: u32 = 0;
    // SAFETY: the handle came from `MonitorFromPoint` and both out-parameters are live `u32`s;
    // `MDT_EFFECTIVE_DPI` only reads the monitor's scaling.
    let status = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let dpi = if status == 0 && dpi_x > 0 { dpi_x } else { BASE_DPI };
    Some(Monitor { work: info.rcWork, dpi })
}

/// The integer nearest-neighbour factor for `dpi`, as a whole multiple of the 100% size.
///
/// Integer division of `(dpi + 48) / 96` rounds the scale to the nearest whole number: 96 dpi
/// gives 1x, 144 dpi (150%) gives 2x, 192 dpi (200%) gives 2x, 240 dpi (250%) gives 3x. Rounding
/// rather than truncating keeps the art at the size the user asked for; the factor stays an
/// integer so every destination pixel is a copy of one source pixel. A DPI too large to express
/// as a scale falls back to the cap, and the result is always inside `1..=MAX_SCALE`.
fn scale_for(dpi: u32) -> i32 {
    let rounded = i64::from(dpi).saturating_add(i64::from(BASE_DPI / 2)) / i64::from(BASE_DPI);
    i32::try_from(rounded).unwrap_or(MAX_SCALE).clamp(1, MAX_SCALE)
}

/// Centre `size` in the monitor's work area, in physical pixels.
///
/// The two axes are independent: each contributes half of its own slack, and an axis with no
/// slack contributes none, so a work area too small to fit the splash on one axis puts the
/// window on that axis' origin instead of off the top-left corner.
fn centered(work: RECT, size: Size) -> POINT {
    let width = (work.right - work.left).max(0);
    let height = (work.bottom - work.top).max(0);
    POINT {
        x: work.left.saturating_add(centered_offset(width, size.width)),
        y: work.top.saturating_add(centered_offset(height, size.height)),
    }
}

/// Half the room one axis has left once `needed` is taken out of `span`, floored at zero.
///
/// `saturating_sub` cannot express this: it only clamps at `i32::MIN`, so a work area narrower
/// than the splash produced a negative slack and centred the window off its top-left corner. An
/// axis too small to fit the splash has no room to share, so it offsets by nothing at all.
fn centered_offset(span: i32, needed: i32) -> i32 {
    let slack = (i64::from(span) - i64::from(needed)).max(0);
    i32::try_from(slack / 2).unwrap_or(i32::MAX)
}

/// NUL-terminated UTF-16, for the Win32 entry points that take `PCWSTR`.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the opt-in window test below needs these: the production path reads the ex-style back
    // through none of them.
    use windows_sys::Win32::{
        System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS},
        UI::WindowsAndMessaging::{
            GetClientRect, GetForegroundWindow, GetWindowLongPtrW, IsWindowVisible, GWL_EXSTYLE,
        },
    };

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    fn size(width: i32, height: i32) -> Size {
        Size { width, height }
    }

    #[test]
    fn the_scale_is_an_integer_multiple_of_the_authored_size() {
        assert_eq!(scale_for(96), 1, "100%");
        assert_eq!(scale_for(120), 1, "125% keeps the art 1:1 rather than resampling it");
        assert_eq!(scale_for(144), 2, "150% rounds up to the next whole multiple");
        assert_eq!(scale_for(168), 2, "175%");
        assert_eq!(scale_for(192), 2, "200%");
        assert_eq!(scale_for(240), 3, "250%");
        assert_eq!(scale_for(0), 1, "an unreadable DPI is 100%");
        assert_eq!(scale_for(u32::MAX), MAX_SCALE, "the surface size is capped");
        for dpi in [96, 120, 144, 168, 192, 216, 240, 288, 336, 384, 480] {
            let scale = scale_for(dpi);
            assert!((1..=MAX_SCALE).contains(&scale), "{dpi} dpi gives a bounded scale");
        }
    }

    #[test]
    fn the_splash_is_centred_in_the_work_area_not_the_monitor() {
        // A 1920x1080 monitor whose taskbar takes the bottom 48 pixels: the work area is what
        // counts, so the splash is 24 pixels higher than it would be on the full monitor.
        let work = rect(0, 0, 1920, 1032);
        let centre = centered(work, size(960, 600));
        assert_eq!((centre.x, centre.y), (480, 216));
        assert_eq!(centre.y + 600 / 2, 1032 / 2, "centred in the work area");
    }

    #[test]
    fn the_splash_is_centred_on_a_secondary_monitor_with_a_negative_origin() {
        let work = rect(-1920, -200, 0, 880);
        let centre = centered(work, size(960, 600));
        assert_eq!((centre.x, centre.y), (-1440, 40));
    }

    #[test]
    fn a_work_area_smaller_than_the_splash_puts_it_at_the_top_left() {
        let work = rect(100, 50, 500, 300);
        let centre = centered(work, size(960, 600));
        assert_eq!((centre.x, centre.y), (100, 50), "never a negative offset");
    }

    #[test]
    fn a_work_area_too_short_on_one_axis_still_centres_the_other() {
        // Only the vertical axis is short, so the horizontal offset survives: the axes are
        // computed independently and one cannot take the other down with it.
        let work = rect(100, 50, 1400, 300);
        let centre = centered(work, size(960, 600));
        assert_eq!((centre.x, centre.y), (270, 50), "centred across, flush on the short axis");
    }

    #[test]
    fn the_high_contrast_struct_mirrors_the_vendored_layout() {
        // `cbSize: u32, dwFlags: u32, lpszDefaultScheme: PWSTR` and the natural padding that
        // follows: on a 64-bit build 4 + 4 + 8.
        assert_eq!(size_of::<HighContrast>(), size_of::<(u32, u32, *mut u16)>());
        assert_eq!(HCF_HIGHCONTRASTON, 1);
    }

    /// One process-wide GUI object count, the number Task Manager shows in its "GDI objects" and
    /// "USER objects" columns. Zero means the query could not be made.
    fn gui_objects(flag: u32) -> u32 {
        // SAFETY: `GetCurrentProcess` returns a pseudo handle to this process, which
        // `GetGuiResources` only queries for its counters, and the flag is one of the `GR_*`
        // values that API accepts.
        unsafe { GetGuiResources(GetCurrentProcess(), flag) }
    }

    /// The opt-in proof that the real Win32 path works on this machine. It is skipped unless
    /// `AW_SPLASH_WINDOW_TEST=1`, so the default suite stays headless.
    ///
    /// It walks the same steps `open` walks — class, surface, window, first present — on one
    /// thread (windows are thread-affine: only the creating thread may destroy one), and it
    /// stops before `show`: the window is created hidden, presented while still hidden, and
    /// destroyed. Nothing here can appear on screen or take the foreground.
    ///
    /// The GDI and USER counts are process-wide, so this test is meant to be run filtered; the
    /// window code is the only thing in this crate that creates a DC, a bitmap or a window, so
    /// no other test moves those counters.
    #[test]
    fn a_layered_splash_window_is_created_presented_hidden_and_released() {
        if std::env::var_os("AW_SPLASH_WINDOW_TEST").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return;
        }

        let gdi_before = gui_objects(GR_GDIOBJECTS);
        let user_before = gui_objects(GR_USEROBJECTS);

        // The real DPI-scaled size for this machine's target monitor, computed the way
        // production computes it.
        let monitor = target_monitor().expect("a desktop monitor with a work area");
        let scale = scale_for(monitor.dpi);
        assert!((1..=MAX_SCALE).contains(&scale), "{scale} is a bounded integer scale for {} dpi", monitor.dpi);
        let expected = Size {
            width: art::W * scale,
            height: art::H * scale,
        };
        let placement = Placement {
            at: centered(monitor.work, expected),
            size: expected,
        };

        // 1. The class registers, and a second registration of the same name is accepted without
        // claiming ownership: that is the replay path, where a new generation reuses a class
        // that is still live.
        let class = Class::register().expect("the splash class registers");
        let replay = Class::register().expect("a second registration of a live class is accepted");
        assert!(!replay.owner, "a class this generation did not register must not be unregistered by it");
        drop(replay);

        // 2. The DIB.
        let mut surface = Surface::new(expected).expect("the memory DC and its 32-bit DIB exist");
        assert!(
            !surface.dc.is_null() && !surface.bitmap.is_null() && !surface.bits.is_null(),
            "the surface owns a DC, a bitmap and its pixel storage"
        );
        assert!(
            gui_objects(GR_GDIOBJECTS) > gdi_before,
            "the DC and the DIB are real GDI objects while the surface is alive"
        );

        // 3. The window: created hidden, so it can neither appear nor take focus.
        let mut window = SplashWindow::create(&class, placement).expect("CreateWindowExW returns a window");
        let handle = window.handle;
        assert!(!handle.is_null(), "CreateWindowExW returned a real handle");
        // SAFETY: `handle` is the window created on the line above, on this thread.
        let alive = unsafe { IsWindow(handle) };
        assert_ne!(alive, 0, "the handle refers to a live window");
        // SAFETY: `handle` is that live window.
        let exstyle = unsafe { GetWindowLongPtrW(handle, GWL_EXSTYLE) };
        // The three bits production asks for are all in the low half of the word.
        let exstyle = exstyle as u32;
        for (bit, name) in [
            (WS_EX_LAYERED, "WS_EX_LAYERED"),
            (WS_EX_TOOLWINDOW, "WS_EX_TOOLWINDOW"),
            (WS_EX_TOPMOST, "WS_EX_TOPMOST"),
        ] {
            assert_eq!(
                exstyle & bit,
                bit,
                "{name} must be set: the splash is a topmost per-pixel-alpha popup"
            );
        }

        // 4. The client area is the DPI-scaled 960x600 exactly: `WS_POPUP` has no frame, so
        // nothing is inset from the size that was asked for.
        let mut client = RECT::default();
        // SAFETY: `handle` is the live window above and `client` is a mutable RECT, the type the
        // call writes exactly one of.
        let read = unsafe { GetClientRect(handle, &mut client) };
        assert_ne!(read, 0, "GetClientRect succeeds on a live window");
        assert_eq!((client.left, client.top), (0, 0), "the client area starts at its own origin");
        assert_eq!(client.right - client.left, art::W * scale, "client width is the scaled art width");
        assert_eq!(client.bottom - client.top, art::H * scale, "client height is the scaled art height");

        // 5. One frame, presented while the window is still hidden — the order `open` relies on,
        // and the reason the splash can fail open before anything is on screen. `present` treats
        // a zero return from `UpdateLayeredWindow` as a failure, so an `Ok` here is that call
        // having succeeded.
        let mut frame = vec![0u32; art::PIXEL_COUNT];
        art::draw_frame(&mut frame, 0, ArtOptions::REDUCED_MOTION);
        surface.blit(&frame, scale);
        window
            .present(&surface, placement)
            .expect("UpdateLayeredWindow succeeds on the hidden layered window");

        // 6. Still invisible, and never the foreground window.
        // SAFETY: `handle` is that live window.
        let visible = unsafe { IsWindowVisible(handle) };
        assert_eq!(visible, 0, "the splash window is never shown by this test");
        // SAFETY: `GetForegroundWindow` takes no arguments and only reads the current foreground.
        let foreground = unsafe { GetForegroundWindow() };
        assert_ne!(foreground, handle, "the splash never takes the foreground");

        // 7. Teardown: the destroy succeeds, the handle dies, and both guards return everything
        // they took.
        assert!(window.close(), "DestroyWindow succeeds on the live window");
        assert!(window.handle.is_null(), "the handle is cleared once the window is gone");
        assert!(!window.close(), "a second close finds nothing left to destroy");
        // SAFETY: the handle is only read, and it is stale by design.
        let still_there = unsafe { IsWindow(handle) };
        assert_eq!(still_there, 0, "the window is gone");

        // The guards close it all down: the `Drop` after an explicit `close` is a no-op, the
        // surface gives back its DC and bitmap, and the class unregisters.
        drop(window);
        drop(surface);
        drop(class);

        assert_eq!(
            gui_objects(GR_GDIOBJECTS),
            gdi_before,
            "no GDI object outlives the surface guard"
        );
        assert_eq!(
            gui_objects(GR_USEROBJECTS),
            user_before,
            "no USER object outlives the window and class guards"
        );
    }
}
