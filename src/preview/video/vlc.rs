//! Optional, dynamically linked LibVLC 3 fallback. Only the private runtime next
//! to arcthumb.dll is loaded; PATH, the working directory and installed VLC are
//! never searched. Media and decoded frames stay in bounded memory.

use super::super::surface::Surface;
use super::*;
use std::cell::UnsafeCell;
use std::ffi::{CStr, OsString, c_char, c_int};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACKNESS, DIB_RGB_COLORS, HDC, PatBlt, SRCCOPY,
    STRETCH_HALFTONE, SetBrushOrgEx, SetStretchBltMode, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW, GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{PCSTR, PCWSTR};

type Handle = *mut c_void;
type Open = unsafe extern "C" fn(Handle, *mut Handle, *mut u64) -> c_int;
type Read = unsafe extern "C" fn(Handle, *mut u8, usize) -> isize;
type Seek = unsafe extern "C" fn(Handle, u64) -> c_int;
type Close = unsafe extern "C" fn(Handle);
type Lock = unsafe extern "C" fn(Handle, *mut Handle) -> Handle;
type Display = unsafe extern "C" fn(Handle, Handle);
type Format =
    unsafe extern "C" fn(*mut Handle, *mut c_char, *mut u32, *mut u32, *mut u32, *mut u32) -> u32;

// LibVLC 3's public C ABI. Function pointers remain valid for process lifetime;
// this COM server also deliberately returns S_FALSE from DllCanUnloadNow.
struct Api {
    new: unsafe extern "C" fn(c_int, *const *const c_char) -> Handle,
    media_new: unsafe extern "C" fn(Handle, Open, Read, Seek, Close, Handle) -> Handle,
    media_release: unsafe extern "C" fn(Handle),
    player_new: unsafe extern "C" fn(Handle) -> Handle,
    player_release: unsafe extern "C" fn(Handle),
    play: unsafe extern "C" fn(Handle) -> c_int,
    stop: unsafe extern "C" fn(Handle),
    pause: unsafe extern "C" fn(Handle, c_int),
    state: unsafe extern "C" fn(Handle) -> c_int,
    callbacks: unsafe extern "C" fn(
        Handle,
        Lock,
        Option<unsafe extern "C" fn(Handle, Handle, *const Handle)>,
        Display,
        Handle,
    ),
    format: unsafe extern "C" fn(Handle, Format, Option<unsafe extern "C" fn(Handle)>),
}

fn runtime_path() -> Result<PathBuf> {
    unsafe {
        let mut module = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(runtime_path as *const () as *const u16),
            &mut module,
        )?;
        let mut path = [0u16; 32768];
        let len = GetModuleFileNameW(Some(module), &mut path) as usize;
        if len == 0 || len >= path.len() {
            return Err(Error::from_hresult(E_FAIL));
        }
        let mut path = PathBuf::from(OsString::from_wide(&path[..len]));
        path.pop();
        Ok(path.join("libvlc").join("libvlc.dll"))
    }
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, HRESULT>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let path = runtime_path().map_err(|e| e.code())?;
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let module = LoadLibraryExW(
            PCWSTR(path.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .map_err(|e| e.code())?;
        macro_rules! load {
            ($symbol:literal, $ty:ty) => {{
                let address =
                    GetProcAddress(module, PCSTR(concat!($symbol, "\0").as_ptr())).ok_or(E_FAIL)?;
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address)
            }};
        }
        Ok(Api {
            new: load!(
                "libvlc_new",
                unsafe extern "C" fn(c_int, *const *const c_char) -> Handle
            ),
            media_new: load!(
                "libvlc_media_new_callbacks",
                unsafe extern "C" fn(Handle, Open, Read, Seek, Close, Handle) -> Handle
            ),
            media_release: load!("libvlc_media_release", unsafe extern "C" fn(Handle)),
            player_new: load!(
                "libvlc_media_player_new_from_media",
                unsafe extern "C" fn(Handle) -> Handle
            ),
            player_release: load!("libvlc_media_player_release", unsafe extern "C" fn(Handle)),
            play: load!(
                "libvlc_media_player_play",
                unsafe extern "C" fn(Handle) -> c_int
            ),
            stop: load!("libvlc_media_player_stop", unsafe extern "C" fn(Handle)),
            pause: load!(
                "libvlc_media_player_set_pause",
                unsafe extern "C" fn(Handle, c_int)
            ),
            state: load!(
                "libvlc_media_player_get_state",
                unsafe extern "C" fn(Handle) -> c_int
            ),
            callbacks: load!(
                "libvlc_video_set_callbacks",
                unsafe extern "C" fn(
                    Handle,
                    Lock,
                    Option<unsafe extern "C" fn(Handle, Handle, *const Handle)>,
                    Display,
                    Handle,
                )
            ),
            format: load!(
                "libvlc_video_set_format_callbacks",
                unsafe extern "C" fn(Handle, Format, Option<unsafe extern "C" fn(Handle)>)
            ),
        })
    })
    .as_ref()
    .map_err(|status| Error::from_hresult(*status))
}

#[repr(C, align(32))]
#[derive(Clone)]
struct Pixels([u8; 32]);

struct Frame {
    width: u32,
    height: u32,
    pitch: u32,
    pixels: Vec<Pixels>,
}

#[derive(Default)]
pub(super) struct Frames {
    active: AtomicBool,
    latest: Mutex<Option<Frame>>,
}

impl Frames {
    // Called only on the UI thread. A decoder can never hold up Explorer: if
    // publication is in progress, leave the existing surface for the next frame.
    pub(super) fn paint(&self, dc: HDC, rect: RECT, surface: &mut Surface) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return true;
        }
        if let Ok(frame) = self.latest.try_lock()
            && let Some(frame) = frame.as_ref()
        {
            let Some(buffer_dc) = surface.prepare(dc, width, height) else {
                return true;
            };
            let scale =
                (width as f64 / frame.width as f64).min(height as f64 / frame.height as f64);
            let w = (frame.width as f64 * scale).round() as i32;
            let h = (frame.height as f64 * scale).round() as i32;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: (frame.pitch / 4) as i32,
                    biHeight: -(frame.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            unsafe {
                // Default BLACKONWHITE stretching combines colour bits
                // when shrinking and visibly posterizes live photos.
                let _ = PatBlt(buffer_dc, 0, 0, width, height, BLACKNESS);
                SetStretchBltMode(buffer_dc, STRETCH_HALFTONE);
                let _ = SetBrushOrgEx(buffer_dc, 0, 0, None);
                StretchDIBits(
                    buffer_dc,
                    (width - w) / 2,
                    (height - h) / 2,
                    w,
                    h,
                    0,
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    Some(frame.pixels.as_ptr().cast()),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
        }
        surface.present(dc, rect);
        true
    }
}

struct Input {
    bytes: Arc<[u8]>,
    shutdown: Arc<AtomicBool>,
}
struct Cursor {
    input: Input,
    position: usize,
}

unsafe extern "C" fn open(opaque: Handle, data: *mut Handle, size: *mut u64) -> c_int {
    unsafe {
        let input = &*(opaque as *const Input);
        if input.shutdown.load(Ordering::Acquire) {
            return -1;
        }
        let cursor = Box::new(Cursor {
            input: Input {
                bytes: Arc::clone(&input.bytes),
                shutdown: Arc::clone(&input.shutdown),
            },
            position: 0,
        });
        *size = cursor.input.bytes.len() as u64;
        *data = Box::into_raw(cursor).cast();
        0
    }
}
unsafe extern "C" fn read(opaque: Handle, buffer: *mut u8, length: usize) -> isize {
    unsafe {
        let cursor = &mut *(opaque as *mut Cursor);
        if cursor.input.shutdown.load(Ordering::Acquire) {
            return -1;
        }
        let n = length
            .min(cursor.input.bytes.len() - cursor.position)
            .min(isize::MAX as usize);
        std::ptr::copy_nonoverlapping(cursor.input.bytes.as_ptr().add(cursor.position), buffer, n);
        cursor.position += n;
        n as isize
    }
}
unsafe extern "C" fn seek(opaque: Handle, offset: u64) -> c_int {
    unsafe {
        let cursor = &mut *(opaque as *mut Cursor);
        if cursor.input.shutdown.load(Ordering::Acquire) || offset > cursor.input.bytes.len() as u64
        {
            return -1;
        }
        cursor.position = offset as usize;
        0
    }
}
unsafe extern "C" fn close(opaque: Handle) {
    unsafe {
        drop(Box::from_raw(opaque as *mut Cursor));
    }
}

struct Pictures {
    // LibVLC 3 vmem serializes copy/display on its output thread and retires
    // that output before renegotiating its format. UI sees only latest.
    back: UnsafeCell<Frame>,
    frames: Arc<Frames>,
    shutdown: Arc<AtomicBool>,
    first: AtomicBool,
    target: PlaybackTarget,
}

unsafe extern "C" fn format(
    opaque: *mut Handle,
    chroma: *mut c_char,
    width: *mut u32,
    height: *mut u32,
    pitches: *mut u32,
    lines: *mut u32,
) -> u32 {
    unsafe {
        if *width == 0 || *height == 0 || *width > 16384 || *height > 16384 {
            return 0;
        }
        let picture = &*(*opaque as *const Pictures);
        let scale = (1280.0 / (*width).max(*height) as f64).min(1.0);
        *width = ((*width as f64 * scale) as u32).max(1);
        *height = ((*height as f64 * scale) as u32).max(1);
        // RV32 on little-endian Windows is BGRX, matching a top-down GDI DIB.
        std::ptr::copy_nonoverlapping(b"RV32".as_ptr(), chroma.cast(), 4);
        *pitches = (*width * 4).next_multiple_of(32);
        *lines = (*height).next_multiple_of(32);
        *picture.back.get() = Frame {
            width: *width,
            height: *height,
            pitch: *pitches,
            pixels: vec![Pixels([0; 32]); (*pitches / 32 * *lines) as usize],
        };
        1
    }
}
unsafe extern "C" fn lock(opaque: Handle, planes: *mut Handle) -> Handle {
    unsafe {
        let picture = &*(opaque as *const Pictures);
        *planes = (*picture.back.get()).pixels.as_mut_ptr().cast();
        std::ptr::null_mut()
    }
}
unsafe extern "C" fn display(opaque: Handle, _picture: Handle) {
    unsafe {
        let picture = &*(opaque as *const Pictures);
        if picture.shutdown.load(Ordering::Acquire) {
            return;
        }
        let back = &*picture.back.get();
        if let Ok(mut latest) = picture.frames.latest.lock() {
            let frame = latest.get_or_insert_with(|| Frame {
                width: back.width,
                height: back.height,
                pitch: back.pitch,
                pixels: Vec::new(),
            });
            frame.width = back.width;
            frame.height = back.height;
            frame.pitch = back.pitch;
            frame.pixels.clone_from(&back.pixels);
        }
        if !picture.first.swap(true, Ordering::AcqRel) {
            picture.target.notify(NOTICE_PLAYING, HRESULT(0));
        }
        picture.target.notify(NOTICE_FRAME, HRESULT(0));
    }
}

struct Player<'a> {
    api: &'a Api,
    media: Handle,
    player: Handle,
}
impl Drop for Player<'_> {
    fn drop(&mut self) {
        unsafe {
            if !self.player.is_null() {
                (self.api.stop)(self.player);
                (self.api.player_release)(self.player);
            }
            if !self.media.is_null() {
                (self.api.media_release)(self.media);
            }
        }
    }
}

pub(super) fn run(
    bytes: Arc<[u8]>,
    target: PlaybackTarget,
    receiver: &Receiver<PlayerCommand>,
    shutdown: Arc<AtomicBool>,
    request: &AtomicU32,
    frames: Arc<Frames>,
) -> Result<()> {
    let api = api()?;
    let mut input = Box::new(Input {
        bytes,
        shutdown: Arc::clone(&shutdown),
    });
    let mut pictures = Box::new(Pictures {
        back: UnsafeCell::new(Frame {
            width: 0,
            height: 0,
            pitch: 0,
            pixels: Vec::new(),
        }),
        frames: Arc::clone(&frames),
        shutdown: Arc::clone(&shutdown),
        first: AtomicBool::new(false),
        target,
    });
    // The player guard must drop BEFORE the callback contexts, including on
    // every error path. Stop/release wait only on this detached worker.
    let mut player = Player {
        api,
        media: std::ptr::null_mut(),
        player: std::ptr::null_mut(),
    };
    let instance = instance(api)?;
    unsafe {
        player.media = (api.media_new)(
            instance,
            open,
            read,
            seek,
            close,
            (&mut *input as *mut Input).cast(),
        );
        if player.media.is_null() {
            return Err(Error::from_hresult(E_FAIL));
        }
        player.player = (api.player_new)(player.media);
        if player.player.is_null() {
            return Err(Error::from_hresult(E_FAIL));
        }
        (api.callbacks)(
            player.player,
            lock,
            None,
            display,
            (&mut *pictures as *mut Pictures).cast(),
        );
        (api.format)(player.player, format, None);
    }
    run_player(
        &player, &pictures, target, receiver, &shutdown, request, &frames,
    )
}

// LibVLC is thread-safe. Share one engine/plugin registry across selections;
// do not rescan the runtime or construct a global decoder pool for every file.
fn instance(api: &Api) -> Result<Handle> {
    static INSTANCE: OnceLock<std::result::Result<usize, HRESULT>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            let args: &[&CStr] = &[
                c"--ignore-config",
                c"--no-plugins-cache",
                c"--no-video-title-show",
                c"--no-osd",
                c"--no-snapshot-preview",
                c"--no-stats",
                c"--no-media-library",
                c"--no-lua",
                c"--no-spu",
                c"--avcodec-hw=none",
                c"--avcodec-threads=0",
                c"--file-caching=100",
                c"--quiet",
            ];
            let args: Vec<_> = args.iter().map(|arg| arg.as_ptr()).collect();
            let instance = unsafe { (api.new)(args.len() as c_int, args.as_ptr()) };
            if instance.is_null() {
                Err(E_FAIL)
            } else {
                Ok(instance as usize)
            }
        })
        .map(|instance| instance as Handle)
        .map_err(Error::from_hresult)
}

fn run_player(
    player: &Player<'_>,
    pictures: &Pictures,
    target: PlaybackTarget,
    receiver: &Receiver<PlayerCommand>,
    shutdown: &AtomicBool,
    request: &AtomicU32,
    frames: &Frames,
) -> Result<()> {
    let api = player.api;
    frames.active.store(true, Ordering::Release);
    alog!("LIVP: LibVLC software fallback prepared (memory input, automatic decoder threading)");
    let mut last_state = 0;
    let mut started = false;
    let mut start_time = Instant::now();
    while !shutdown.load(Ordering::Acquire) {
        if let Err(mpsc::RecvTimeoutError::Disconnected) =
            receiver.recv_timeout(Duration::from_millis(12))
        {
            break;
        }
        match request.swap(REQUEST_NONE, Ordering::AcqRel) {
            REQUEST_PLAY => unsafe {
                // LibVLC requires stop before replaying an ended input. Retain
                // the engine, media bytes, callback storage and player object.
                if started {
                    (api.stop)(player.player);
                }
                pictures.first.store(false, Ordering::Release);
                if (api.play)(player.player) != 0 {
                    return Err(Error::from_hresult(E_FAIL));
                }
                started = true;
                last_state = 0;
                start_time = Instant::now();
            },
            REQUEST_PAUSE if started => unsafe {
                (api.pause)(player.player, 1);
            },
            REQUEST_RESUME if started => unsafe {
                (api.pause)(player.player, 0);
            },
            _ => {}
        }
        let state = unsafe { (api.state)(player.player) };
        if state != last_state {
            match state {
                3 if last_state == 4 => target.notify(NOTICE_PLAYING, HRESULT(0)),
                4 => target.notify(NOTICE_PAUSED, HRESULT(0)),
                6 if pictures.first.load(Ordering::Acquire) => {
                    target.notify(NOTICE_ENDED, HRESULT(0))
                }
                6 => return Err(Error::from_hresult(E_FAIL)),
                7 => return Err(Error::from_hresult(E_FAIL)),
                _ => {}
            }
            last_state = state;
        }
        if started
            && !pictures.first.load(Ordering::Acquire)
            && start_time.elapsed() > Duration::from_secs(15)
        {
            return Err(Error::from_hresult(E_FAIL));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_buffer_is_bounded_and_aligned_for_native_conversion() {
        let mut pictures = Pictures {
            back: UnsafeCell::new(Frame {
                width: 0,
                height: 0,
                pitch: 0,
                pixels: Vec::new(),
            }),
            frames: Arc::new(Frames::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            first: AtomicBool::new(false),
            target: PlaybackTarget {
                notify_window: 0,
                render_window: 0,
                token: 1,
            },
        };
        let mut opaque = (&mut pictures as *mut Pictures).cast();
        let (mut w, mut h, mut pitch, mut lines) = (2160, 3840, 0, 0);
        let mut chroma = [0; 4];
        unsafe {
            assert_eq!(
                format(
                    &mut opaque,
                    chroma.as_mut_ptr(),
                    &mut w,
                    &mut h,
                    &mut pitch,
                    &mut lines
                ),
                1
            );
            assert_eq!((w, h), (720, 1280));
            assert_eq!(pitch % 32, 0);
            assert_eq!(lines % 32, 0);
            let frame = &*pictures.back.get();
            assert_eq!(frame.pixels.as_ptr() as usize % 32, 0);
            assert!(frame.pixels.len() * 32 >= (pitch * lines) as usize);
            assert!(frame.pixels.len() * 32 <= 1280 * 1280 * 4);
            w = 0;
            assert_eq!(
                format(
                    &mut opaque,
                    chroma.as_mut_ptr(),
                    &mut w,
                    &mut h,
                    &mut pitch,
                    &mut lines
                ),
                0
            );
            w = 32768;
            assert_eq!(
                format(
                    &mut opaque,
                    chroma.as_mut_ptr(),
                    &mut w,
                    &mut h,
                    &mut pitch,
                    &mut lines
                ),
                0
            );
        }
    }

    #[test]
    fn memory_callbacks_reopen_seek_eof_and_cancel() {
        let mut input = Input {
            bytes: Arc::from(&b"abcdef"[..]),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        unsafe {
            let mut data = std::ptr::null_mut();
            let mut size = 0;
            let ptr = (&mut input as *mut Input).cast();
            assert_eq!(open(ptr, &mut data, &mut size), 0);
            assert_eq!(size, 6);
            let mut buffer = [0; 8];
            assert_eq!(read(data, buffer.as_mut_ptr(), 3), 3);
            assert_eq!(&buffer[..3], b"abc");
            assert_eq!(seek(data, 5), 0);
            assert_eq!(read(data, buffer.as_mut_ptr(), 8), 1);
            assert_eq!(buffer[0], b'f');
            assert_eq!(read(data, buffer.as_mut_ptr(), 8), 0);
            assert_eq!(seek(data, u64::MAX), -1);
            close(data);
            assert_eq!(open(ptr, &mut data, &mut size), 0);
            assert_eq!(read(data, buffer.as_mut_ptr(), 6), 6);
            assert_eq!(&buffer[..6], b"abcdef");
            input.shutdown.store(true, Ordering::Release);
            assert_eq!(read(data, buffer.as_mut_ptr(), 8), -1);
            assert_eq!(seek(data, 0), -1);
            close(data);
            assert_eq!(open(ptr, &mut data, &mut size), -1);
        }
    }
}
