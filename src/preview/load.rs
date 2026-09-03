//! Asynchronous preview loading.
//!
//! `IPreviewHandler::DoPreview` is called on the preview host's UI thread.  A
//! slow ZIP entry, WIC codec, or image decoder must therefore never run inside
//! that call: doing so freezes the preview host and can make Explorer appear
//! hung.  This module marshals Explorer's `IStream` to a worker apartment,
//! performs all archive/decode work there, and posts a completion message back
//! to the preview window.

use std::io::{self, Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use image::DynamicImage;
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::Marshal::{
    CoMarshalInterThreadInterfaceInStream, CoReleaseMarshalData,
};
use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IStream};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{Error, Interface, Result};

use crate::{alog, archive, decode, settings, stream::ComStreamReader};

use super::video::{self, VideoCodec};

pub(super) const WM_ARCTHUMB_LOAD_COMPLETE: u32 = WM_APP + 0x349;

pub(super) struct LoadedPreview {
    pub image: DynamicImage,
    pub video_bytes: Option<Arc<[u8]>>,
    pub video_codec: VideoCodec,
}

pub(super) enum LoadOutcome {
    Ready(LoadedPreview),
    Failed,
}

/// State shared only between a loader and the UI thread.  It deliberately
/// contains no pointer/reference to the COM handler, so a late worker cannot
/// access a handler that Explorer has already released.
pub(super) struct LoadSlot {
    token: u32,
    cancelled: AtomicBool,
    outcome: Mutex<Option<LoadOutcome>>,
}

impl LoadSlot {
    fn new(token: u32) -> Self {
        Self {
            token,
            cancelled: AtomicBool::new(false),
            outcome: Mutex::new(None),
        }
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(super) fn token(&self) -> u32 {
        self.token
    }

    pub(super) fn take_outcome(&self) -> Option<LoadOutcome> {
        self.outcome.lock().ok()?.take()
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// The stream returned by `CoMarshalInterThreadInterfaceInStream` is expressly
/// intended to be transferred to another thread.  `windows-rs` conservatively
/// leaves all raw COM interfaces as `!Send`, so this narrow wrapper records the
/// guarantee supplied by that API; the wrapped pointer is never otherwise used
/// on the originating thread.
struct MarshalledStream(Option<IStream>);
unsafe impl Send for MarshalledStream {}

impl MarshalledStream {
    fn take(mut self) -> IStream {
        self.0.take().expect("marshalled stream already consumed")
    }
}

impl Drop for MarshalledStream {
    fn drop(&mut self) {
        if let Some(stream) = self.0.as_ref() {
            // If spawning/panicking prevents the destination thread from
            // unmarshalling, release COM's marshal packet explicitly.
            unsafe {
                let _ = CoReleaseMarshalData(stream);
            }
        }
    }
}

/// Start loading and return immediately.  The only synchronous COM work is
/// creating the standard inter-thread marshal packet.
pub(super) fn start(stream: IStream, hwnd: HWND, target_px: u32) -> Result<Arc<LoadSlot>> {
    let permit = LoaderPermit::acquire().inspect_err(|_| {
        alog!("Preview: refusing to start more than {MAX_ACTIVE_LOADERS} loader threads");
    })?;
    let marshalled = unsafe { CoMarshalInterThreadInterfaceInStream(&IStream::IID, &stream) }?;
    let marshalled = MarshalledStream(Some(marshalled));
    let token = next_load_token();
    let slot = Arc::new(LoadSlot::new(token));
    let worker_slot = Arc::clone(&slot);
    let hwnd_value = hwnd.0 as isize;

    thread::Builder::new()
        .name("arcthumb-preview-load".to_string())
        .spawn(move || {
            let _permit = permit;
            let hwnd = HWND(hwnd_value as *mut core::ffi::c_void);
            let outcome = match catch_unwind(AssertUnwindSafe(|| {
                load_preview(marshalled, &worker_slot, target_px)
            })) {
                Ok(Ok(loaded)) => LoadOutcome::Ready(loaded),
                Ok(Err(error)) => {
                    alog!("Preview load failed: {error}");
                    LoadOutcome::Failed
                }
                Err(_) => {
                    alog!("PANIC caught in preview loader");
                    LoadOutcome::Failed
                }
            };

            if worker_slot.is_cancelled() {
                alog!("Preview load cancelled");
                return;
            }
            if let Ok(mut result) = worker_slot.outcome.lock() {
                *result = Some(outcome);
            } else {
                return;
            }
            unsafe {
                // A failed post simply means Explorer unloaded/destroyed the
                // window while decoding. The Arc-owned result is then dropped
                // safely on this worker.
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_ARCTHUMB_LOAD_COMPLETE,
                    WPARAM(token as usize),
                    LPARAM(0),
                );
            }
        })
        .map_err(|_| Error::from_hresult(E_FAIL))?;

    Ok(slot)
}

static NEXT_LOAD_TOKEN: AtomicU32 = AtomicU32::new(1);
static ACTIVE_LOADERS: AtomicU32 = AtomicU32::new(0);
const MAX_ACTIVE_LOADERS: u32 = 4;

struct LoaderPermit;

impl LoaderPermit {
    fn acquire() -> Result<Self> {
        ACTIVE_LOADERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_LOADERS).then_some(active + 1)
            })
            .map_err(|_| Error::from_hresult(E_FAIL))?;
        Ok(Self)
    }
}

impl Drop for LoaderPermit {
    fn drop(&mut self) {
        ACTIVE_LOADERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn next_load_token() -> u32 {
    loop {
        let token = NEXT_LOAD_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

fn load_preview(
    marshalled: MarshalledStream,
    slot: &LoadSlot,
    target_px: u32,
) -> std::result::Result<LoadedPreview, String> {
    // A newly-created Rust worker has no COM apartment yet. Every successful
    // CoInitializeEx, including S_FALSE, must be balanced by CoUninitialize.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = ComApartment;

    alog!("Preview load: unmarshalling Explorer stream");
    // CoGetInterfaceAndReleaseStream consumes/releases the marshal stream even
    // though windows-rs exposes it as a borrowed Param. Suppress the wrapper's
    // normal Drop to avoid releasing that COM pointer a second time.
    let marshalled = ManuallyDrop::new(marshalled.take());
    let stream: IStream = unsafe { CoGetInterfaceAndReleaseStream(&*marshalled) }
        .map_err(|e| format!("stream unmarshal failed: {e}"))?;
    let reader = ComStreamReader::new(stream);
    let mut reader = CancellableReader::new(reader, &slot.cancelled);

    alog!("Preview load: inspecting archive");
    let (name, bytes, video_bytes, video_codec) =
        match archive::try_read_livp(&mut reader, settings::current()) {
            Ok(Some(parts)) => {
                alog!(
                    "Preview: LIVP pair {} + {} ({} video bytes)",
                    parts.image_name,
                    parts.video_name,
                    parts.video_bytes.len()
                );
                let codec = video::detect_mov_codec(&parts.video_bytes);
                (
                    parts.image_name,
                    parts.image_bytes,
                    Some(Arc::from(parts.video_bytes.into_boxed_slice())),
                    codec,
                )
            }
            Ok(None) => {
                let (name, bytes) = archive::read_first_image(reader, settings::current())
                    .map_err(|e| format!("archive read failed: {e}"))?;
                (name, bytes, None, VideoCodec::Unknown)
            }
            Err(e) => return Err(format!("LIVP inspection failed: {e}")),
        };

    if slot.is_cancelled() {
        return Err("cancelled before image decode".to_string());
    }
    alog!("Preview load: decoding {name} for {target_px}px target");
    let image = decode::decode_for_thumbnail(&name, &bytes, target_px)
        .map_err(|e| format!("decode failed for {name}: {e}"))?;
    alog!(
        "Preview: decoded {}x{} from {}",
        image.width(),
        image.height(),
        name
    );

    Ok(LoadedPreview {
        image,
        video_bytes,
        video_codec,
    })
}

/// Lets archive parsing notice `Unload` between reads. Third-party decoders
/// may still take time inside one call, but cancellation never makes Explorer
/// wait because loader threads are detached.
struct CancellableReader<'a, R> {
    inner: R,
    cancelled: &'a AtomicBool,
}

impl<'a, R> CancellableReader<'a, R> {
    fn new(inner: R, cancelled: &'a AtomicBool) -> Self {
        Self { inner, cancelled }
    }

    fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(io::Error::new(
                // Read::read_exact/read_to_end retry Interrupted indefinitely.
                // Cancellation is permanent, so report a non-retryable error.
                io::ErrorKind::ConnectionAborted,
                "preview load cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for CancellableReader<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.check()?;
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use zip::write::SimpleFileOptions;

    fn build_livp() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            3,
            2,
            image::Rgb([10, 20, 30]),
        ));
        let mut jpeg = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();

        let mut archive = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut archive));
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            writer.start_file("photo.jpg", options).unwrap();
            writer.write_all(&jpeg).unwrap();
            writer.start_file("photo.mov", options).unwrap();
            writer.write_all(b"....hvc1....motion....").unwrap();
            writer.finish().unwrap();
        }
        archive
    }

    #[test]
    fn cancellable_reader_reads_and_seeks_before_cancellation() {
        let cancelled = AtomicBool::new(false);
        let mut reader = CancellableReader::new(Cursor::new(b"abcdef"), &cancelled);
        let mut bytes = [0; 2];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ab");
        assert_eq!(reader.seek(SeekFrom::Start(4)).unwrap(), 4);
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ef");
    }

    #[test]
    fn cancellable_reader_interrupts_reads_and_seeks() {
        let cancelled = AtomicBool::new(true);
        let mut reader = CancellableReader::new(Cursor::new(b"abcdef"), &cancelled);
        let mut byte = [0; 1];
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            reader.seek(SeekFrom::Start(0)).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        // Check the error kind above first: on the old implementation these
        // standard Read helpers would retry forever rather than returning.
        assert_eq!(
            reader.read_exact(&mut byte).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            reader.read_to_end(&mut Vec::new()).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn marshalled_stream_loads_livp_off_thread_pipeline() {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .unwrap();
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }
        let _apartment = Apartment;

        let bytes = build_livp();
        let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.unwrap();
        let marshalled =
            unsafe { CoMarshalInterThreadInterfaceInStream(&IStream::IID, &stream) }.unwrap();
        let marshalled = MarshalledStream(Some(marshalled));
        let loaded = thread::spawn(move || {
            let slot = LoadSlot::new(1);
            load_preview(marshalled, &slot, 256).unwrap()
        })
        .join()
        .unwrap();

        assert_eq!((loaded.image.width(), loaded.image.height()), (3, 2));
        assert_eq!(loaded.video_codec, VideoCodec::Hevc);
        assert_eq!(
            loaded.video_bytes.unwrap().as_ref(),
            b"....hvc1....motion...."
        );
    }
}
