//! `IPreviewHandler` for ArcThumb.
//!
//! Mirrors the architecture of `com::ArcThumbProvider`, but instead
//! of returning a single HBITMAP it owns a child window inside
//! Explorer's preview pane (`Alt+P`) and paints the cover image into
//! it. For LIVP input it can also play the paired MOV through Media
//! Foundation without extracting it to disk.
//!
//! Lifecycle as Explorer / `prevhost.exe` calls it:
//!
//! 1. `IClassFactory::CreateInstance` → `ArcThumbPreviewHandler::default()`
//! 2. `IInitializeWithStream::Initialize(stream)` → stash the stream
//! 3. `IObjectWithSite::SetSite(site)` → stash (we never call back)
//! 4. `IPreviewHandler::SetWindow(parent, rect)` → remember parent + rect
//! 5. `IPreviewHandler::SetRect(rect)` → resize child window if any
//! 6. `IPreviewHandler::DoPreview()` → create the child window, marshal
//!    the stream to a cancellable loader thread, and return promptly
//! 7. The loader posts its decoded cover (and bounded LIVP MOV) back to
//!    the child window, which stores the result and schedules a paint
//! 8. (`SetRect` may fire many times during drag-resize. Each one
//!    moves the child window and invalidates it; the WM_PAINT handler
//!    re-resizes the cached image.)
//! 9. `IPreviewHandler::Unload()` → cancel without waiting, destroy the
//!    child window, and drop cached state
//! 10. `Release()` → eventually drops the impl struct, which destroys
//!    any window we still own (safety net for hosts that skip Unload)
//!
//! Every COM entry point is wrapped in `catch_unwind` so a panic in
//! the decoder, GDI, or our own code can never escape into
//! `prevhost.exe` and crash it.

mod load;
mod render;
mod video;

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use windows::Win32::Foundation::{
    CLASS_E_NOAGGREGATION, E_FAIL, E_NOINTERFACE, E_POINTER, HINSTANCE, HWND, LPARAM, RECT,
    S_FALSE, WPARAM,
};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl, IStream};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus, VK_RETURN, VK_SPACE};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandler_Impl};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GWLP_USERDATA, MSG, MoveWindow, SetParent, SetWindowLongPtrW,
    WINDOW_EX_STYLE, WM_KEYDOWN, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, Ref, Result, implement, w};

use crate::{alog, limits};

use render::CachedBitmap;
use video::{VideoCodec, VideoPlayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PreviewVideoState {
    #[default]
    None,
    Idle,
    Starting,
    Playing,
    Paused,
    Ended,
    LoadError,
    Error,
}

// =============================================================================
// CLSID + class factory
// =============================================================================

/// CLSID for the ArcThumb preview handler. **Never change** — baked
/// into users' registries on install. Distinct from
/// `CLSID_ARCTHUMB_PROVIDER` (the thumbnail provider) so the two
/// classes register as separate COM objects and can be toggled
/// independently.
pub const CLSID_ARCTHUMB_PREVIEW: GUID = GUID::from_u128(0x8C7C1E5F_3D4A_4E2B_9F1A_7B5D6E8F9A0C);

#[implement(IClassFactory)]
pub struct ArcThumbPreviewClassFactory;

impl IClassFactory_Impl for ArcThumbPreviewClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *ppvobject = std::ptr::null_mut();
            let handler = ArcThumbPreviewHandler::default();
            let unknown: IUnknown = handler.into();
            unknown.query(&*riid, ppvobject).ok()
        }
    }

    fn LockServer(&self, _flock: BOOL) -> Result<()> {
        Ok(())
    }
}

// =============================================================================
// ArcThumbPreviewHandler
// =============================================================================

/// The COM object Explorer / prevhost.exe instantiates per file.
///
/// All mutable state lives behind interior-mutability primitives so
/// the COM trait methods can mutate it through `&self`.
#[implement(IPreviewHandler, IInitializeWithStream, IObjectWithSite, IOleWindow)]
#[derive(Default)]
pub struct ArcThumbPreviewHandler {
    /// IStream stashed by `Initialize`. Consumed by `DoPreview`.
    stream: RefCell<Option<IStream>>,
    /// Site interface set by `IObjectWithSite::SetSite`. We never
    /// call back into it but `GetSite` must round-trip it.
    site: RefCell<Option<IUnknown>>,
    /// Parent HWND set by `IPreviewHandler::SetWindow`.
    parent_hwnd: Cell<HWND>,
    /// Last rect set by `SetWindow` / `SetRect`, in parent coords.
    rect: Cell<RECT>,
    /// Our owned child window, created in `DoPreview`. Destroyed in
    /// `Unload` (or in `Drop` as a safety net).
    child_hwnd: Cell<HWND>,
    /// Decoded source image, retained across `SetRect` events so we
    /// don't re-parse the archive on every drag-resize tick.
    pub(crate) source: RefCell<Option<image::DynamicImage>>,
    /// Cached HBITMAP at the last drawn (width, height). Replaced on
    /// resize. Freed via `CachedBitmap::Drop`.
    pub(crate) cache: RefCell<Option<CachedBitmap>>,
    /// Bounded MOV payload extracted from a LIVP. Shared with a playback
    /// worker without another full-size copy.
    video_bytes: RefCell<Option<Arc<[u8]>>>,
    video_codec: Cell<VideoCodec>,
    video_state: Cell<PreviewVideoState>,
    video_error: Cell<HRESULT>,
    video_player: RefCell<Option<VideoPlayer>>,
    /// Identifies the current detached playback worker. Late messages from a
    /// worker belonging to an already-unloaded/reused HWND are ignored.
    video_token: Cell<u32>,
    /// Result slot owned jointly with the detached loader. Unload cancels and
    /// drops our reference without ever waiting for a codec or archive parser.
    load_slot: RefCell<Option<Arc<load::LoadSlot>>>,
}

impl Drop for ArcThumbPreviewHandler {
    /// Safety net: if a host releases us without calling `Unload`,
    /// the child window would leak. We tear it down here too.
    fn drop(&mut self) {
        if let Some(player) = self.video_player.get_mut().take() {
            self.video_token.set(0);
            player.shutdown();
        }
        if let Some(slot) = self.load_slot.get_mut().take() {
            slot.cancel();
        }
        let hwnd = self.child_hwnd.get();
        if !hwnd.is_invalid() {
            unsafe {
                // Clear our pointer first so a stray WM_PAINT during
                // teardown can't dereference us.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

// =============================================================================
// Panic-guard helper
// =============================================================================

/// Run `f`, returning its `Result<()>` on success or `E_FAIL` on panic.
/// Used by every COM entry point — a panic crossing the C ABI is UB
/// and would take down `prevhost.exe`.
fn guard<F: FnOnce() -> Result<()>>(label: &str, f: F) -> Result<()> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            alog!("PANIC caught in {label}");
            Err(windows::core::Error::from_hresult(E_FAIL))
        }
    }
}

// =============================================================================
// IInitializeWithStream
// =============================================================================

impl IInitializeWithStream_Impl for ArcThumbPreviewHandler_Impl {
    fn Initialize(&self, pstream: Ref<'_, IStream>, _grfmode: u32) -> Result<()> {
        guard("Preview::Initialize", || {
            *self.this.stream.borrow_mut() = pstream.cloned();
            Ok(())
        })
    }
}

// =============================================================================
// IObjectWithSite
// =============================================================================

impl IObjectWithSite_Impl for ArcThumbPreviewHandler_Impl {
    fn SetSite(&self, punksite: Ref<'_, IUnknown>) -> Result<()> {
        guard("Preview::SetSite", || {
            *self.this.site.borrow_mut() = punksite.cloned();
            Ok(())
        })
    }

    fn GetSite(&self, riid: *const GUID, ppvsite: *mut *mut c_void) -> Result<()> {
        guard("Preview::GetSite", || {
            if riid.is_null() || ppvsite.is_null() {
                return Err(E_POINTER.into());
            }
            unsafe {
                *ppvsite = std::ptr::null_mut();
                let site = self.this.site.borrow();
                match site.as_ref() {
                    Some(unk) => unk.query(&*riid, ppvsite).ok(),
                    None => Err(E_NOINTERFACE.into()),
                }
            }
        })
    }
}

// =============================================================================
// IOleWindow
// =============================================================================

impl IOleWindow_Impl for ArcThumbPreviewHandler_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        // No need for catch_unwind here — pure field load.
        Ok(self.this.child_hwnd.get())
    }

    fn ContextSensitiveHelp(&self, _fentermode: BOOL) -> Result<()> {
        // Explorer never calls this with TRUE; we have no help to show.
        Ok(())
    }
}

// =============================================================================
// IPreviewHandler
// =============================================================================

impl IPreviewHandler_Impl for ArcThumbPreviewHandler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> Result<()> {
        guard("Preview::SetWindow", || {
            self.this.parent_hwnd.set(hwnd);
            if !prc.is_null() {
                self.this.rect.set(unsafe { *prc });
            }
            // If the child window already exists (re-parenting case),
            // move it under the new parent and resize.
            let child = self.this.child_hwnd.get();
            if !child.is_invalid() && !hwnd.is_invalid() {
                let r = self.this.rect.get();
                unsafe {
                    let _ = SetParent(child, Some(hwnd));
                    let _ = MoveWindow(
                        child,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        true,
                    );
                }
            }
            Ok(())
        })
    }

    fn SetRect(&self, prc: *const RECT) -> Result<()> {
        guard("Preview::SetRect", || {
            if prc.is_null() {
                return Err(E_POINTER.into());
            }
            let r = unsafe { *prc };
            self.this.rect.set(r);
            if let Some(player) = self.this.video_player.borrow().as_ref() {
                player.resize(r.right - r.left, r.bottom - r.top);
            }
            let child = self.this.child_hwnd.get();
            if !child.is_invalid() {
                unsafe {
                    let _ = MoveWindow(
                        child,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        true,
                    );
                    let _ = InvalidateRect(Some(child), None, true);
                }
            }
            Ok(())
        })
    }

    fn DoPreview(&self) -> Result<()> {
        guard("Preview::DoPreview", || {
            // 1. Take the stream out so we can consume it.
            let stream = self
                .this
                .stream
                .borrow_mut()
                .take()
                .ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;

            // 2. Create the child window before starting potentially slow
            // archive/WIC work. DoPreview runs on prevhost's UI thread and
            // must return promptly even if a third-party codec stalls.
            if self.this.child_hwnd.get().is_invalid() {
                self.create_child_window()?;
            }

            // 3. Marshal Explorer's stream to a detached worker apartment.
            // No Unload/Drop path joins that worker, so Explorer can always
            // switch files or close the preview pane immediately.
            if let Some(old) = self.this.load_slot.borrow_mut().take() {
                old.cancel();
            }
            let r = self.this.rect.get();
            let target_px = (r.right - r.left)
                .max(r.bottom - r.top)
                .max(1)
                .min(limits::MAX_THUMBNAIL_SIZE as i32) as u32;
            let slot = load::start(stream, self.this.child_hwnd.get(), target_px).map_err(|e| {
                alog!("Preview: could not start asynchronous load: {e}");
                windows::core::Error::from_hresult(E_FAIL)
            })?;
            *self.this.load_slot.borrow_mut() = Some(slot);
            unsafe {
                let _ = InvalidateRect(Some(self.this.child_hwnd.get()), None, true);
            }
            Ok(())
        })
    }

    fn Unload(&self) -> Result<()> {
        // Unload must always succeed; swallow any internal failure.
        let _ = guard("Preview::Unload", || {
            if let Some(player) = self.this.video_player.borrow_mut().take() {
                self.this.video_token.set(0);
                player.shutdown();
            }
            if let Some(slot) = self.this.load_slot.borrow_mut().take() {
                slot.cancel();
            }
            let hwnd = self.this.child_hwnd.replace(HWND::default());
            if !hwnd.is_invalid() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let _ = DestroyWindow(hwnd);
                }
            }
            *self.this.cache.borrow_mut() = None;
            *self.this.source.borrow_mut() = None;
            *self.this.stream.borrow_mut() = None;
            *self.this.video_bytes.borrow_mut() = None;
            self.this.video_state.set(PreviewVideoState::None);
            self.this.video_codec.set(VideoCodec::Unknown);
            self.this.video_error.set(HRESULT(0));
            Ok(())
        });
        Ok(())
    }

    fn SetFocus(&self) -> Result<()> {
        let child = self.this.child_hwnd.get();
        if child.is_invalid() {
            return Err(windows::core::Error::from_hresult(S_FALSE));
        }
        unsafe {
            let _ = SetFocus(Some(child));
        }
        Ok(())
    }

    fn QueryFocus(&self) -> Result<HWND> {
        let focus = unsafe { GetFocus() };
        if focus.is_invalid() {
            Err(windows::core::Error::from_hresult(S_FALSE))
        } else {
            Ok(focus)
        }
    }

    fn TranslateAccelerator(&self, pmsg: *const MSG) -> Result<()> {
        if !pmsg.is_null() {
            let msg = unsafe { &*pmsg };
            if msg.message == WM_KEYDOWN
                && (msg.wParam.0 == VK_SPACE.0 as usize || msg.wParam.0 == VK_RETURN.0 as usize)
            {
                self.this.toggle_video();
                return Ok(());
            }
        }
        Err(windows::core::Error::from_hresult(S_FALSE))
    }
}

// =============================================================================
// Window creation
// =============================================================================

impl ArcThumbPreviewHandler_Impl {
    fn create_child_window(&self) -> Result<()> {
        let parent = self.this.parent_hwnd.get();
        if parent.is_invalid() {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        let atom = render::register_window_class();
        if atom == 0 {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        let r = self.this.rect.get();
        let width = (r.right - r.left).max(1);
        let height = (r.bottom - r.top).max(1);

        // Pass a pointer to the user struct (`self.this`) so the
        // window proc can recover us via GWLP_USERDATA in WM_NCCREATE.
        let user_ptr: *const ArcThumbPreviewHandler = &self.this as *const ArcThumbPreviewHandler;

        let hinstance: HINSTANCE = unsafe {
            GetModuleHandleW(None)
                .map(|h| HINSTANCE(h.0))
                .unwrap_or_default()
        };

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(atom as usize as *const u16),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                r.left,
                r.top,
                width,
                height,
                Some(parent),
                None,
                Some(hinstance),
                Some(user_ptr as *const c_void),
            )
        }
        .map_err(|e| {
            alog!("Preview: CreateWindowExW failed: {e}");
            windows::core::Error::from_hresult(E_FAIL)
        })?;

        self.this.child_hwnd.set(hwnd);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
        Ok(())
    }
}

impl ArcThumbPreviewHandler {
    pub(crate) fn video_state(&self) -> PreviewVideoState {
        self.video_state.get()
    }

    pub(crate) fn video_overlay_text(&self) -> Option<&'static str> {
        if self.load_slot.borrow().is_some() && self.source.borrow().is_none() {
            return Some("Loading preview...");
        }
        match self.video_state.get() {
            PreviewVideoState::Idle | PreviewVideoState::Ended => {
                Some("Click or press Space to play Live Photo")
            }
            PreviewVideoState::Starting => Some("Opening Live Photo video..."),
            PreviewVideoState::LoadError => Some("Preview is unavailable"),
            PreviewVideoState::Error if self.video_codec.get() == VideoCodec::Hevc => {
                Some("H.265 unavailable - install Microsoft HEVC Video Extensions")
            }
            PreviewVideoState::Error => Some("Live Photo video playback is unavailable"),
            PreviewVideoState::None | PreviewVideoState::Playing | PreviewVideoState::Paused => {
                None
            }
        }
    }

    pub(crate) fn handle_load_complete(&self, token: WPARAM) {
        let token = token.0 as u32;
        let is_current = self
            .load_slot
            .borrow()
            .as_ref()
            .is_some_and(|slot| slot.token() == token);
        if token == 0 || !is_current {
            return;
        }
        let Some(slot) = self.load_slot.borrow_mut().take() else {
            return;
        };
        match slot.take_outcome() {
            Some(load::LoadOutcome::Ready(loaded)) => {
                *self.source.borrow_mut() = Some(loaded.image);
                *self.video_bytes.borrow_mut() = loaded.video_bytes;
                self.video_codec.set(loaded.video_codec);
                self.video_state
                    .set(if self.video_bytes.borrow().is_some() {
                        PreviewVideoState::Idle
                    } else {
                        PreviewVideoState::None
                    });
            }
            Some(load::LoadOutcome::Failed) => {
                self.video_state.set(PreviewVideoState::LoadError);
            }
            None => return,
        }
        unsafe {
            let _ = InvalidateRect(Some(self.child_hwnd.get()), None, true);
        }
    }

    pub(crate) fn request_video_repaint(&self) {
        if let Some(player) = self.video_player.borrow().as_ref() {
            player.repaint();
        }
    }

    pub(crate) fn toggle_video(&self) {
        match self.video_state.get() {
            PreviewVideoState::None
            | PreviewVideoState::Starting
            | PreviewVideoState::LoadError => {}
            PreviewVideoState::Playing | PreviewVideoState::Paused => {
                if let Some(player) = self.video_player.borrow().as_ref() {
                    player.toggle();
                }
            }
            PreviewVideoState::Ended => {
                self.video_state.set(PreviewVideoState::Starting);
                unsafe {
                    let _ = InvalidateRect(Some(self.child_hwnd.get()), None, true);
                }
                if let Some(player) = self.video_player.borrow().as_ref() {
                    player.replay();
                }
            }
            PreviewVideoState::Idle | PreviewVideoState::Error => {
                if let Some(old) = self.video_player.borrow_mut().take() {
                    self.video_token.set(0);
                    old.shutdown();
                }
                let Some(bytes) = self.video_bytes.borrow().as_ref().cloned() else {
                    return;
                };
                let hwnd = self.child_hwnd.get();
                if hwnd.is_invalid() {
                    return;
                }
                let r = self.rect.get();
                self.video_state.set(PreviewVideoState::Starting);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                let token = video::next_player_token();
                self.video_token.set(token);
                match VideoPlayer::start(bytes, hwnd, r.right - r.left, r.bottom - r.top, token) {
                    Ok(player) => *self.video_player.borrow_mut() = Some(player),
                    Err(error) => {
                        self.video_token.set(0);
                        self.video_error.set(error.code());
                        self.video_state.set(PreviewVideoState::Error);
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, true);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn handle_video_notice(&self, notice: WPARAM, status: LPARAM) {
        let (token, status) = video::unpack_notice_status(status);
        if token == 0 || token != self.video_token.get() {
            return;
        }
        match notice.0 {
            video::NOTICE_PLAYING => self.video_state.set(PreviewVideoState::Playing),
            video::NOTICE_PAUSED => self.video_state.set(PreviewVideoState::Paused),
            video::NOTICE_ENDED => self.video_state.set(PreviewVideoState::Ended),
            video::NOTICE_FAILED => {
                self.video_error.set(status);
                self.video_state.set(PreviewVideoState::Error);
            }
            _ => return,
        }
        if matches!(
            self.video_state.get(),
            PreviewVideoState::Ended | PreviewVideoState::Error
        ) {
            unsafe {
                let _ = InvalidateRect(Some(self.child_hwnd.get()), None, true);
            }
        }
    }
}
