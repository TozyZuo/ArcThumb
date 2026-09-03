//! End-to-end COM integration test for arcthumb.dll.
//!
//! This test exercises the same code path Explorer uses:
//!
//! 1. `LoadLibraryW("arcthumb.dll")`
//! 2. `GetProcAddress("DllGetClassObject")`
//! 3. Call it to obtain `IClassFactory` for `CLSID_ARCTHUMB_PROVIDER`
//! 4. `IClassFactory::CreateInstance` → `IUnknown`
//! 5. `QueryInterface` for `IInitializeWithStream` and `IThumbnailProvider`
//! 6. Wrap an in-memory ZIP (containing a real PNG) in an `IStream`
//!    via `SHCreateMemStream`
//! 7. `IInitializeWithStream::Initialize(stream)`
//! 8. `IThumbnailProvider::GetThumbnail(64)` → `HBITMAP`
//! 9. Verify the bitmap exists and has sane dimensions
//! 10. Free everything
//!
//! Together this covers `lib.rs` (Dll exports), `com.rs` (factory +
//! provider), `stream.rs` (ComStreamReader bridging IStream), most
//! of `bitmap.rs` (from_rgba GDI path), plus the archive + decode
//! pipeline that the unit tests already cover in isolation.
//!
//! The test is conditionally compiled for Windows only — the rest
//! of the crate is too, but cargo would still try to build the test
//! file on other targets, so we gate it explicitly.

#![cfg(windows)]

use std::ffi::{OsString, c_void};
use std::io::Cursor;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, RECT, S_OK};
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, IClassFactory, IStream,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Ole::{IObjectWithSite, IOleWindow};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IPreviewHandler, IThumbnailProvider, SHCreateMemStream, WTS_ALPHATYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DestroyWindow, IsWindow, WINDOW_EX_STYLE, WS_POPUP,
};
use windows::core::{GUID, HRESULT, Interface, PCWSTR, w};

use arcthumb::{CLSID_ARCTHUMB_PREVIEW, CLSID_ARCTHUMB_PROVIDER};

/// Signature of `DllGetClassObject` as it is exported from `arcthumb.dll`.
type DllGetClassObjectFn = unsafe extern "system" fn(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT;

// =============================================================================
// Helpers
// =============================================================================

/// Locate `arcthumb.dll` relative to the running test executable.
///
/// Cargo lays out test binaries as `target/<profile>/deps/<name>-<hash>.exe`
/// and the cdylib alongside `target/<profile>/arcthumb.dll`. Walking
/// up two parents from the test exe lands us in the profile dir.
///
/// **Important**: some test runners (notably `cargo llvm-cov`) invoke
/// `cargo test --tests`, which only builds test targets and skips the
/// cdylib. We detect that and run `cargo build --lib` explicitly into
/// the *same* target dir, inheriting `RUSTFLAGS` so the cdylib carries
/// the same instrumentation as the test binary. That keeps the COM
/// integration test useful for coverage measurement, not just for
/// black-box pass/fail.
fn locate_dll() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // <target>/<profile>/deps/com_integration-HASH.exe
    let profile_dir = exe
        .parent() // deps/
        .and_then(|p| p.parent()) // <profile>/
        .expect("test exe should be inside target/<profile>/deps/");
    let target_dir = profile_dir
        .parent() // target/  (or target/llvm-cov-target/)
        .expect("profile dir should have a parent")
        .to_path_buf();
    let profile_name = profile_dir
        .file_name()
        .expect("profile dir name")
        .to_string_lossy()
        .into_owned();

    let candidate = profile_dir.join("arcthumb.dll");
    if candidate.exists() {
        return candidate;
    }

    // Cdylib wasn't built. Force `cargo build --lib` into the same
    // target dir we're already running from, so any instrumentation
    // RUSTFLAGS the parent process set are honoured.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut cmd = std::process::Command::new(env!("CARGO"));
    cmd.args(["build", "--lib", "--quiet"])
        .current_dir(manifest)
        .env("CARGO_TARGET_DIR", &target_dir);
    if profile_name == "release" {
        cmd.arg("--release");
    }
    let status = cmd.status().expect("failed to spawn cargo build --lib");
    assert!(status.success(), "cargo build --lib failed");

    assert!(
        candidate.exists(),
        "arcthumb.dll still missing after cargo build at {}",
        candidate.display()
    );
    candidate
}

/// UTF-16 NUL-terminated form of a `Path`, suitable for `LoadLibraryW`.
fn to_wide(path: &std::path::Path) -> Vec<u16> {
    OsString::from(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Build a tiny in-memory PNG so we have realistic image bytes for
/// the archive entry. Using the `image` crate keeps the fixture
/// self-contained and reproducible across CI machines.
fn make_tiny_png() -> Vec<u8> {
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_fn(4, 4, |_, _| Rgba([255, 0, 0, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// Wrap a single PNG in an in-memory ZIP and return the bytes.
fn make_test_zip() -> Vec<u8> {
    use zip::write::SimpleFileOptions;
    let png = make_tiny_png();
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("01.png", opts).unwrap();
        std::io::Write::write_all(&mut w, &png).unwrap();
        w.finish().unwrap();
    }
    buf
}

/// Keeps the DLL loaded for the lifetime of the test process.
///
/// This intentionally does not call `FreeLibrary`: ArcThumb's
/// `DllCanUnloadNow` returns `S_FALSE`, so a real COM host retains the module.
/// More importantly, preview `Unload` is deliberately non-blocking and its
/// cancelled loader can still be winding down. Unloading executable code from
/// underneath that worker would make this test manufacture an access violation
/// that Explorer cannot produce while honouring `DllCanUnloadNow`.
struct LoadedDll(#[allow(dead_code)] HMODULE);

/// RAII wrapper that calls `CoUninitialize` on drop.
struct ComApartment;
impl ComApartment {
    fn enter() -> Self {
        unsafe {
            // Shell extensions live in STA. Test runs in its own
            // thread so this is hermetic.
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            assert!(hr.is_ok(), "CoInitializeEx failed: {hr:?}");
        }
        Self
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Free an HBITMAP on drop.
struct OwnedHBitmap(HBITMAP);
impl Drop for OwnedHBitmap {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0.0));
        }
    }
}

// =============================================================================
// The actual integration test
// =============================================================================

#[test]
fn end_to_end_thumbnail_via_dll() {
    let _com = ComApartment::enter();

    let dll_path = locate_dll();
    assert!(
        dll_path.exists(),
        "arcthumb.dll not found at {}; run `cargo build` first",
        dll_path.display()
    );

    // ---- Step 1: load the DLL ----------------------------------
    let wide = to_wide(&dll_path);
    let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("LoadLibraryW failed");
    let _dll_guard = LoadedDll(module);

    // ---- Step 2: resolve DllGetClassObject ---------------------
    let proc = unsafe { GetProcAddress(module, windows::core::s!("DllGetClassObject")) }
        .expect("DllGetClassObject not exported");
    let dll_get_class_object: DllGetClassObjectFn = unsafe { std::mem::transmute(proc) };

    // ---- Step 3: ask for IClassFactory -------------------------
    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        dll_get_class_object(
            &CLSID_ARCTHUMB_PROVIDER,
            &IClassFactory::IID,
            &mut factory_ptr,
        )
    };
    assert_eq!(hr, S_OK, "DllGetClassObject failed: {hr:?}");
    assert!(!factory_ptr.is_null(), "factory pointer is null");

    let factory: IClassFactory = unsafe { IClassFactory::from_raw(factory_ptr) };

    // ---- Step 4: factory creates a thumbnail provider ----------
    let provider_unknown: windows::core::IUnknown =
        unsafe { factory.CreateInstance(None).expect("CreateInstance failed") };

    // ---- Step 5: QueryInterface for the two interfaces we need
    let init_with_stream: IInitializeWithStream = provider_unknown
        .cast()
        .expect("cast to IInitializeWithStream");
    let thumb_provider: IThumbnailProvider =
        provider_unknown.cast().expect("cast to IThumbnailProvider");

    // ---- Step 6: build a fake archive stream -------------------
    let zip_bytes = make_test_zip();
    let stream: IStream =
        unsafe { SHCreateMemStream(Some(&zip_bytes)) }.expect("SHCreateMemStream returned None");

    // ---- Step 7: Initialize(stream) ----------------------------
    unsafe {
        init_with_stream
            .Initialize(&stream, 0)
            .expect("Initialize failed");
    }

    // ---- Step 8: GetThumbnail(64) ------------------------------
    let mut hbmp = HBITMAP::default();
    let mut alpha: WTS_ALPHATYPE = WTS_ALPHATYPE(0);
    unsafe {
        thumb_provider
            .GetThumbnail(64, &mut hbmp, &mut alpha)
            .expect("GetThumbnail failed");
    }
    let _bmp_guard = OwnedHBitmap(hbmp);

    // ---- Step 9: validate the returned bitmap ------------------
    assert!(!hbmp.is_invalid(), "HBITMAP is invalid");

    // Inspect the DIB header. The provider resizes to fit inside
    // 64×64 while preserving aspect ratio; for our 4×4 source the
    // result is exactly 64×64.
    let mut bm = BITMAP::default();
    let written = unsafe {
        GetObjectW(
            HGDIOBJ(hbmp.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        )
    };
    assert!(written > 0, "GetObjectW returned 0");
    assert!(bm.bmWidth > 0 && bm.bmWidth <= 64, "width = {}", bm.bmWidth);
    assert!(
        bm.bmHeight > 0 && bm.bmHeight <= 64,
        "height = {}",
        bm.bmHeight
    );
    assert_eq!(bm.bmBitsPixel, 32, "expected 32bpp DIB");
    // The provider always returns ARGB so Explorer can composite.
    assert_eq!(alpha.0, 2 /* WTSAT_ARGB */);
}

/// Negative-path test: passing the wrong CLSID must yield
/// `CLASS_E_CLASSNOTAVAILABLE` (0x80040111) and **not** crash the
/// loader thread.
#[test]
fn dll_get_class_object_rejects_unknown_clsid() {
    let _com = ComApartment::enter();

    let dll_path = locate_dll();
    let wide = to_wide(&dll_path);
    let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("LoadLibraryW failed");
    let _dll_guard = LoadedDll(module);

    let proc = unsafe { GetProcAddress(module, windows::core::s!("DllGetClassObject")) }
        .expect("DllGetClassObject not exported");
    let dll_get_class_object: DllGetClassObjectFn = unsafe { std::mem::transmute(proc) };

    // Random GUID we definitely don't host.
    let bogus = GUID::from_u128(0xDEAD_BEEF_CAFE_BABE_0102_0304_0506_0708);
    let mut out: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { dll_get_class_object(&bogus, &IClassFactory::IID, &mut out) };
    // CLASS_E_CLASSNOTAVAILABLE
    assert_eq!(
        hr.0, 0x80040111u32 as i32,
        "expected CLASS_E_CLASSNOTAVAILABLE, got {hr:?}"
    );
    assert!(out.is_null());
}

/// Negative-path test: `DllCanUnloadNow` should return `S_FALSE` so
/// COM keeps us loaded for the lifetime of Explorer.
#[test]
fn dll_can_unload_now_returns_s_false() {
    let dll_path = locate_dll();
    let wide = to_wide(&dll_path);
    let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("LoadLibraryW failed");
    let _dll_guard = LoadedDll(module);

    let proc = unsafe { GetProcAddress(module, windows::core::s!("DllCanUnloadNow")) }
        .expect("DllCanUnloadNow not exported");
    type Fn0 = unsafe extern "system" fn() -> HRESULT;
    let f: Fn0 = unsafe { std::mem::transmute(proc) };
    let hr = unsafe { f() };
    // S_FALSE = 0x00000001
    assert_eq!(hr.0, 1, "expected S_FALSE, got {hr:?}");
}

// =============================================================================
// IPreviewHandler integration tests
// =============================================================================

/// Helper that loads the DLL, resolves DllGetClassObject, and asks
/// for the preview-handler class factory. Returns the loaded DLL
/// guard and the IClassFactory so the caller can keep both alive.
fn load_preview_factory() -> (LoadedDll, IClassFactory) {
    let dll_path = locate_dll();
    assert!(
        dll_path.exists(),
        "arcthumb.dll not found at {}",
        dll_path.display()
    );

    let wide = to_wide(&dll_path);
    let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.expect("LoadLibraryW failed");
    let dll_guard = LoadedDll(module);

    let proc = unsafe { GetProcAddress(module, windows::core::s!("DllGetClassObject")) }
        .expect("DllGetClassObject not exported");
    let dll_get_class_object: DllGetClassObjectFn = unsafe { std::mem::transmute(proc) };

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        dll_get_class_object(
            &CLSID_ARCTHUMB_PREVIEW,
            &IClassFactory::IID,
            &mut factory_ptr,
        )
    };
    assert_eq!(hr, S_OK, "DllGetClassObject(preview) failed: {hr:?}");
    assert!(!factory_ptr.is_null(), "preview factory pointer is null");
    let factory: IClassFactory = unsafe { IClassFactory::from_raw(factory_ptr) };
    (dll_guard, factory)
}

/// Build a hidden top-level window we can use as the preview pane's
/// host. WS_POPUP without WS_VISIBLE means it's never shown — we
/// just need an HWND that lives long enough for `DoPreview` to
/// parent its child against.
fn create_hidden_parent() -> HWND {
    let hinstance: HINSTANCE = unsafe {
        GetModuleHandleW(None)
            .map(|h| HINSTANCE(h.0))
            .unwrap_or_default()
    };
    // Reuse the standard "STATIC" class so we don't have to register
    // our own — STATIC is always present in user32.dll.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("ArcThumb test parent"),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            400,
            400,
            None,
            None,
            Some(hinstance),
            None,
        )
    }
    .expect("CreateWindowExW(STATIC) failed");
    assert!(!hwnd.is_invalid());
    hwnd
}

#[test]
fn preview_class_factory_creates_handler() {
    let _com = ComApartment::enter();
    let (_dll, factory) = load_preview_factory();

    // CreateInstance should return an IUnknown that can be cast to
    // each of the four interfaces our preview handler implements.
    let unknown: windows::core::IUnknown =
        unsafe { factory.CreateInstance(None).expect("CreateInstance failed") };
    let _preview: IPreviewHandler = unknown.cast().expect("cast IPreviewHandler");
    let _init: IInitializeWithStream = unknown.cast().expect("cast IInitializeWithStream");
    let _site: IObjectWithSite = unknown.cast().expect("cast IObjectWithSite");
    let _ole: IOleWindow = unknown.cast().expect("cast IOleWindow");
}

/// Registry ThreadingModel=Apartment is insufficient if QueryInterface exposes
/// windows-rs's default IAgileObject/IMarshal implementation. Both the factory
/// and the HWND-owning handler must use standard apartment marshaling.
#[test]
fn preview_factory_and_handler_are_apartment_bound() {
    let _com = ComApartment::enter();
    let (_dll, factory) = load_preview_factory();
    let unknown: windows::core::IUnknown = unsafe { factory.CreateInstance(None).unwrap() };
    let factory_unknown: windows::core::IUnknown = factory.cast().unwrap();
    for object in [&factory_unknown, &unknown] {
        for iid in [
            GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90), // IAgileObject
            GUID::from_u128(0x00000003_0000_0000_c000_000000000046), // IMarshal
        ] {
            let mut ptr = std::ptr::null_mut();
            let hr = unsafe { object.query(&iid, &mut ptr) };
            if !ptr.is_null() {
                drop(unsafe { windows::core::IUnknown::from_raw(ptr) });
            }
            assert_eq!(hr, windows::Win32::Foundation::E_NOINTERFACE);
        }
    }
}

/// Unlike the original same-thread smoke test, calls arrive from an MTA like
/// prevhost's RPC dispatch. COM must deliver activation/window operations to
/// the owning STA, which is the only thread with a UI message pump.
#[test]
fn preview_cross_apartment_calls_keep_window_on_owner_thread() {
    use std::mem::ManuallyDrop;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows::Win32::System::Com::COINIT_MULTITHREADED;
    use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
    use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, FindWindowExW, GetWindowThreadProcessId, MSG, PM_REMOVE, PeekMessageW,
        TranslateMessage,
    };

    let _com = ComApartment::enter();
    let (_dll, factory) = load_preview_factory();
    let owner_thread = unsafe { GetCurrentThreadId() };
    let parent = create_hidden_parent();
    let marshalled =
        unsafe { CoMarshalInterThreadInterfaceInStream(&IClassFactory::IID, &factory).unwrap() };
    // This marshal packet (not a raw factory interface) is designed to cross
    // apartments. The destination consumes it with CoGetInterfaceAndReleaseStream.
    let packet = marshalled.into_raw() as usize;
    let parent_value = parent.0 as usize;
    let (send, recv) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(|| {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .unwrap();
            let _com = ComApartment;
            let packet = ManuallyDrop::new(unsafe { IStream::from_raw(packet as *mut c_void) });
            let factory: IClassFactory =
                unsafe { CoGetInterfaceAndReleaseStream(&*packet).unwrap() };
            let unknown: windows::core::IUnknown = unsafe { factory.CreateInstance(None).unwrap() };
            let preview: IPreviewHandler = unknown.cast().unwrap();
            let init: IInitializeWithStream = unknown.cast().unwrap();
            let bytes = make_test_zip();
            let stream = unsafe { SHCreateMemStream(Some(&bytes)).unwrap() };
            let rect = RECT {
                left: 0,
                top: 0,
                right: 200,
                bottom: 200,
            };
            unsafe {
                init.Initialize(&stream, 0).unwrap();
                preview
                    .SetWindow(HWND(parent_value as *mut c_void), &rect)
                    .unwrap();
                preview.DoPreview().unwrap();
                // IOleWindow::GetWindow is input_sync; COM rejects calling
                // that method from an MTA across apartments. Inspect the real
                // child HWND through Win32 without bypassing preview marshaling.
                let child = FindWindowExW(
                    Some(HWND(parent_value as *mut c_void)),
                    None,
                    w!("ArcThumbPreviewWindow"),
                    PCWSTR::null(),
                )
                .unwrap();
                let window_thread = GetWindowThreadProcessId(child, None);
                preview.SetRect(&rect).unwrap();
                preview.Unload().unwrap();
                assert!(
                    !IsWindow(Some(child)).as_bool(),
                    "Unload must destroy on the owner thread"
                );
                assert_eq!(
                    window_thread, owner_thread,
                    "preview HWND was created on an RPC worker"
                );
            }
        });
        let _ = send.send(outcome);
    });

    let started = Instant::now();
    let outcome = loop {
        if let Ok(outcome) = recv.try_recv() {
            break outcome;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cross-apartment preview call hung"
        );
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    worker.join().unwrap();
    unsafe { DestroyWindow(parent).unwrap() };
    outcome.unwrap();
}

#[test]
fn preview_handler_end_to_end() {
    let _com = ComApartment::enter();
    let (_dll, factory) = load_preview_factory();

    // Get the four interfaces.
    let unknown: windows::core::IUnknown =
        unsafe { factory.CreateInstance(None).expect("CreateInstance failed") };
    let preview: IPreviewHandler = unknown.cast().expect("IPreviewHandler");
    let init: IInitializeWithStream = unknown.cast().expect("IInitializeWithStream");
    let ole: IOleWindow = unknown.cast().expect("IOleWindow");

    // Build the same in-memory ZIP fixture the thumbnail end-to-end
    // test uses.
    let zip_bytes = make_test_zip();
    let stream: IStream =
        unsafe { SHCreateMemStream(Some(&zip_bytes)) }.expect("SHCreateMemStream returned None");
    unsafe { init.Initialize(&stream, 0).expect("Initialize failed") };

    // Make a hidden parent and parent the preview to it.
    let parent = create_hidden_parent();
    let rect = RECT {
        left: 0,
        top: 0,
        right: 400,
        bottom: 400,
    };
    unsafe {
        preview.SetWindow(parent, &rect).expect("SetWindow failed");
        preview.SetRect(&rect).expect("SetRect failed");
        preview.DoPreview().expect("DoPreview failed");
    }

    // GetWindow should now return our child window.
    let child = unsafe { ole.GetWindow().expect("GetWindow failed") };
    assert!(!child.is_invalid(), "child HWND is invalid");
    let alive = unsafe { IsWindow(Some(child)) };
    assert!(alive.as_bool(), "child window not alive after DoPreview");

    // Unload should tear it down.
    unsafe { preview.Unload().expect("Unload failed") };
    let alive_after = unsafe { IsWindow(Some(child)) };
    assert!(
        !alive_after.as_bool(),
        "child window should be destroyed by Unload"
    );

    // Cleanup the parent we created.
    unsafe {
        let _ = DestroyWindow(parent);
    }
}

#[test]
fn preview_handler_unload_is_safe_without_dopreview() {
    let _com = ComApartment::enter();
    let (_dll, factory) = load_preview_factory();

    let unknown: windows::core::IUnknown =
        unsafe { factory.CreateInstance(None).expect("CreateInstance failed") };
    let preview: IPreviewHandler = unknown.cast().expect("IPreviewHandler");

    // Calling Unload before DoPreview must succeed without panicking.
    unsafe {
        preview
            .Unload()
            .expect("Unload before DoPreview should succeed")
    };
}

// Suppress dead-code warnings for the unused OsStringExt import on
// some configurations.
#[allow(dead_code)]
fn _suppress_unused() {
    let _ = OsString::from_wide(&[]);
}
