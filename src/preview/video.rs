//! In-memory Media Foundation playback for the MOV entry in an Apple LIVP.
//!
//! The preview handler never writes the clip to disk. The complete, bounded
//! MOV payload is wrapped in an `IStream`/`IMFByteStream`, resolved as an
//! MPEG-4 media source, and connected to the system audio renderer and EVR
//! video renderer. Media Foundation inserts the installed H.264/H.265 decoder
//! while resolving the partial topology.

use std::ffi::c_void;
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSession, IMFMediaSource, IMFPresentationDescriptor, IMFStreamDescriptor,
    IMFTopology, IMFTopologyNode, IMFVideoDisplayControl, MEEndOfPresentation, MESessionClosed,
    MESessionTopologyStatus, MF_E_NO_EVENTS_AVAILABLE, MF_EVENT_FLAG_NO_WAIT,
    MF_EVENT_TOPOLOGY_STATUS, MF_RESOLUTION_MEDIASOURCE, MF_RESOLUTION_READ,
    MF_TOPOLOGY_OUTPUT_NODE, MF_TOPOLOGY_SOURCESTREAM_NODE, MF_TOPONODE_PRESENTATION_DESCRIPTOR,
    MF_TOPONODE_SOURCE, MF_TOPONODE_STREAM_DESCRIPTOR, MF_TOPOSTATUS_READY, MF_VERSION,
    MFCreateAudioRendererActivate, MFCreateMFByteStreamOnStream, MFCreateMediaSession,
    MFCreateSourceResolver, MFCreateTopology, MFCreateTopologyNode, MFCreateVideoRendererActivate,
    MFGetService, MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup,
    MFVideoARMode_PreservePicture, MR_VIDEO_RENDER_SERVICE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{BOOL, Error, HRESULT, Interface, Result, w};

use crate::alog;

/// Private messages sent from the Media Foundation worker back to the preview
/// window. `WPARAM` carries one of the `NOTICE_*` values and `LPARAM` carries an
/// HRESULT for failure notifications.
pub(super) const WM_ARCTHUMB_VIDEO_STATE: u32 = WM_APP + 0x34A;
pub(super) const NOTICE_PLAYING: usize = 1;
pub(super) const NOTICE_PAUSED: usize = 2;
pub(super) const NOTICE_ENDED: usize = 3;
pub(super) const NOTICE_FAILED: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum VideoCodec {
    H264,
    Hevc,
    #[default]
    Unknown,
}

/// Lightweight ISO-BMFF sample-entry detection used only to improve the error
/// message when Media Foundation cannot resolve a decoder. Media Foundation is
/// still authoritative about the actual stream type.
pub(super) fn detect_mov_codec(bytes: &[u8]) -> VideoCodec {
    let mut detected = VideoCodec::Unknown;
    for tag in bytes.windows(4) {
        if tag == b"hvc1" || tag == b"hev1" {
            return VideoCodec::Hevc;
        }
        if tag == b"avc1" || tag == b"avc3" {
            detected = VideoCodec::H264;
        }
    }
    detected
}

enum PlayerCommand {
    Toggle,
    Replay,
    Resize(i32, i32),
    Repaint,
}

/// Media Foundation requires an explicit `Shutdown` before the final release.
/// These guards also cover early returns while topology resolution reports a
/// missing codec or malformed MOV.
struct MediaSourceGuard(IMFMediaSource);

impl Deref for MediaSourceGuard {
    type Target = IMFMediaSource;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for MediaSourceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Shutdown();
        }
    }
}

struct MediaSessionGuard(IMFMediaSession);

impl Deref for MediaSessionGuard {
    type Target = IMFMediaSession;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for MediaSessionGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Shutdown();
        }
    }
}

/// UI-thread handle for the playback worker. All Media Foundation interfaces
/// stay on the worker thread; the preview handler communicates through a
/// channel so COM apartment ownership remains straightforward.
pub(super) struct VideoPlayer {
    commands: SyncSender<PlayerCommand>,
    shutdown: Arc<AtomicBool>,
}

impl VideoPlayer {
    pub(super) fn start(
        bytes: Arc<[u8]>,
        hwnd: HWND,
        width: i32,
        height: i32,
        token: u32,
    ) -> Result<Self> {
        // Resize/repaint are advisory and can arrive much faster than a codec
        // consumes them. A bounded queue plus try_send prevents either memory
        // growth or UI-thread blocking while Explorer is being resized.
        let (commands, receiver) = mpsc::sync_channel(16);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let hwnd_value = hwnd.0 as isize;
        let worker = thread::Builder::new()
            .name("arcthumb-livp-video".to_string())
            .spawn(move || {
                // Recreate the handle value on the destination thread; HWND is
                // process-global but its typed wrapper contains a raw pointer
                // and is intentionally not moved between Rust threads.
                let hwnd = HWND(hwnd_value as *mut c_void);
                match catch_unwind(AssertUnwindSafe(|| {
                    run_worker(bytes, hwnd, width, height, receiver, token, worker_shutdown)
                })) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        alog!("LIVP video playback failed: {error}");
                        post_notice(hwnd, NOTICE_FAILED, error.code(), token);
                    }
                    Err(_) => {
                        alog!("PANIC caught in LIVP video worker");
                        post_notice(hwnd, NOTICE_FAILED, E_FAIL, token);
                    }
                }
            })
            .map_err(|_| Error::from_hresult(E_FAIL))?;
        // Dropping a Rust JoinHandle detaches the thread. Media Foundation
        // shutdown remains the worker's responsibility; Explorer's UI thread
        // must never join a codec/resolver thread because those components are
        // third-party and may block indefinitely.
        drop(worker);
        Ok(Self { commands, shutdown })
    }

    pub(super) fn toggle(&self) {
        let _ = self.commands.try_send(PlayerCommand::Toggle);
    }

    pub(super) fn replay(&self) {
        let _ = self.commands.try_send(PlayerCommand::Replay);
    }

    pub(super) fn resize(&self, width: i32, height: i32) {
        let _ = self.commands.try_send(PlayerCommand::Resize(width, height));
    }

    pub(super) fn repaint(&self) {
        let _ = self.commands.try_send(PlayerCommand::Repaint);
    }

    pub(super) fn shutdown(self) {
        drop(self);
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

static NEXT_PLAYER_TOKEN: AtomicU32 = AtomicU32::new(1);

pub(super) fn next_player_token() -> u32 {
    loop {
        let token = NEXT_PLAYER_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

fn pack_notice_status(token: u32, status: HRESULT) -> LPARAM {
    let packed = ((token as u64) << 32) | status.0 as u32 as u64;
    LPARAM(packed as isize)
}

pub(super) fn unpack_notice_status(value: LPARAM) -> (u32, HRESULT) {
    let packed = value.0 as u64;
    ((packed >> 32) as u32, HRESULT(packed as u32 as i32))
}

fn post_notice(hwnd: HWND, notice: usize, status: HRESULT, token: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_ARCTHUMB_VIDEO_STATE,
            WPARAM(notice),
            pack_notice_status(token, status),
        );
    }
}

fn run_worker(
    bytes: Arc<[u8]>,
    hwnd: HWND,
    width: i32,
    height: i32,
    receiver: Receiver<PlayerCommand>,
    token: u32,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = ComApartment;

    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }?;
    struct MediaFoundationRuntime;
    impl Drop for MediaFoundationRuntime {
        fn drop(&mut self) {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
    let _media_foundation = MediaFoundationRuntime;

    // Guard declaration order ensures Media Foundation shuts down before the
    // COM apartment on success, error, and panic alike.
    run_worker_initialized(bytes, hwnd, width, height, receiver, token, &shutdown)
}

fn run_worker_initialized(
    bytes: Arc<[u8]>,
    hwnd: HWND,
    width: i32,
    height: i32,
    receiver: Receiver<PlayerCommand>,
    token: u32,
    shutdown: &AtomicBool,
) -> Result<()> {
    let stream = unsafe { SHCreateMemStream(Some(bytes.as_ref())) }
        .ok_or_else(|| Error::from_hresult(E_FAIL))?;
    let byte_stream = unsafe { MFCreateMFByteStreamOnStream(&stream) }?;
    let resolver = unsafe { MFCreateSourceResolver() }?;
    let flags = (MF_RESOLUTION_MEDIASOURCE.0 | MF_RESOLUTION_READ.0) as u32;
    let mut object_type = Default::default();
    let mut object = None;
    unsafe {
        resolver.CreateObjectFromByteStream(
            &byte_stream,
            w!("memory.mov"),
            flags,
            None::<&IPropertyStore>,
            &mut object_type,
            &mut object,
        )?;
    }
    if shutdown.load(Ordering::Acquire) {
        return Ok(());
    }
    let source = MediaSourceGuard(object.ok_or_else(|| Error::from_hresult(E_FAIL))?.cast()?);
    let topology = create_playback_topology(&source, hwnd)?;
    if shutdown.load(Ordering::Acquire) {
        return Ok(());
    }
    let session = MediaSessionGuard(unsafe { MFCreateMediaSession(None) }?);
    unsafe { session.SetTopology(0, &topology)? };

    let mut display: Option<IMFVideoDisplayControl> = None;
    let mut topology_ready = false;
    let mut playing = false;
    let mut ended = false;
    let mut current_size = (width.max(1), height.max(1));
    while !shutdown.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(12)) {
            Ok(PlayerCommand::Toggle) if topology_ready && playing => {
                unsafe { session.Pause()? };
                playing = false;
                post_notice(hwnd, NOTICE_PAUSED, HRESULT(0), token);
            }
            Ok(PlayerCommand::Toggle) if topology_ready => {
                let start = if ended {
                    PROPVARIANT::from(0i64)
                } else {
                    PROPVARIANT::default()
                };
                unsafe { session.Start(std::ptr::null(), &start)? };
                playing = true;
                ended = false;
                post_notice(hwnd, NOTICE_PLAYING, HRESULT(0), token);
            }
            Ok(PlayerCommand::Replay) if topology_ready => {
                let start = PROPVARIANT::from(0i64);
                unsafe { session.Start(std::ptr::null(), &start)? };
                playing = true;
                ended = false;
                post_notice(hwnd, NOTICE_PLAYING, HRESULT(0), token);
            }
            Ok(PlayerCommand::Resize(width, height)) => {
                current_size = (width.max(1), height.max(1));
                if let Some(control) = display.as_ref() {
                    resize_video(control, current_size)?;
                }
            }
            Ok(PlayerCommand::Repaint) => {
                if let Some(control) = display.as_ref() {
                    unsafe { control.RepaintVideo()? };
                }
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        loop {
            let event = match unsafe { session.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(error),
            };
            let status = unsafe { event.GetStatus()? };
            if status.is_err() {
                return Err(Error::from_hresult(status));
            }
            let event_type = unsafe { event.GetType()? };
            if event_type == MESessionTopologyStatus.0 as u32 {
                let topology_status = unsafe { event.GetUINT32(&MF_EVENT_TOPOLOGY_STATUS)? };
                if topology_status == MF_TOPOSTATUS_READY.0 as u32 && !topology_ready {
                    topology_ready = true;
                    display = get_video_display(&session).ok();
                    if let Some(control) = display.as_ref() {
                        unsafe {
                            control.SetAspectRatioMode(MFVideoARMode_PreservePicture.0 as u32)?;
                        }
                        resize_video(control, current_size)?;
                    }
                    let start = PROPVARIANT::default();
                    unsafe { session.Start(std::ptr::null(), &start)? };
                    playing = true;
                    post_notice(hwnd, NOTICE_PLAYING, HRESULT(0), token);
                }
            } else if event_type == MEEndOfPresentation.0 as u32 {
                playing = false;
                ended = true;
                post_notice(hwnd, NOTICE_ENDED, HRESULT(0), token);
            }
        }
    }

    // Close is asynchronous. Give the session a short opportunity to report
    // MESessionClosed before shutting down the source and session objects.
    unsafe {
        let _ = session.Close();
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match unsafe { session.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                if unsafe { event.GetType() }.ok() == Some(MESessionClosed.0 as u32) {
                    break;
                }
            }
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    Ok(())
}

fn create_playback_topology(source: &IMFMediaSource, hwnd: HWND) -> Result<IMFTopology> {
    let topology = unsafe { MFCreateTopology() }?;
    let presentation = unsafe { source.CreatePresentationDescriptor() }?;
    let stream_count = unsafe { presentation.GetStreamDescriptorCount() }?;
    let mut video_streams = 0u32;

    for index in 0..stream_count {
        let mut selected = BOOL(0);
        let mut descriptor = None;
        unsafe {
            presentation.GetStreamDescriptorByIndex(index, &mut selected, &mut descriptor)?;
        }
        let Some(descriptor) = descriptor else {
            continue;
        };
        if !selected.as_bool() {
            continue;
        }

        let handler = unsafe { descriptor.GetMediaTypeHandler() }?;
        let major = unsafe { handler.GetMajorType() }?;
        let renderer: IMFActivate = if major == MFMediaType_Video {
            video_streams += 1;
            unsafe { MFCreateVideoRendererActivate(hwnd) }?
        } else if major == MFMediaType_Audio {
            match unsafe { MFCreateAudioRendererActivate() } {
                Ok(renderer) => renderer,
                Err(error) => {
                    alog!("LIVP audio renderer unavailable; playing silently: {error}");
                    unsafe { presentation.DeselectStream(index)? };
                    continue;
                }
            }
        } else {
            // Apple MOV files may carry timed-metadata tracks. They are not
            // needed for Live Photo playback and have no standard renderer.
            unsafe { presentation.DeselectStream(index)? };
            continue;
        };

        add_topology_branch(&topology, source, &presentation, &descriptor, &renderer)?;
    }

    if video_streams == 0 {
        return Err(Error::from_hresult(E_FAIL));
    }
    Ok(topology)
}

fn add_topology_branch(
    topology: &IMFTopology,
    source: &IMFMediaSource,
    presentation: &IMFPresentationDescriptor,
    descriptor: &IMFStreamDescriptor,
    renderer: &IMFActivate,
) -> Result<()> {
    let source_node = unsafe { MFCreateTopologyNode(MF_TOPOLOGY_SOURCESTREAM_NODE) }?;
    unsafe {
        source_node.SetUnknown(&MF_TOPONODE_SOURCE, source)?;
        source_node.SetUnknown(&MF_TOPONODE_PRESENTATION_DESCRIPTOR, presentation)?;
        source_node.SetUnknown(&MF_TOPONODE_STREAM_DESCRIPTOR, descriptor)?;
    }

    let output_node: IMFTopologyNode = unsafe { MFCreateTopologyNode(MF_TOPOLOGY_OUTPUT_NODE) }?;
    unsafe {
        output_node.SetObject(renderer)?;
        topology.AddNode(&source_node)?;
        topology.AddNode(&output_node)?;
        source_node.ConnectOutput(0, &output_node, 0)?;
    }
    Ok(())
}

fn get_video_display(session: &IMFMediaSession) -> Result<IMFVideoDisplayControl> {
    let mut raw = std::ptr::null_mut();
    unsafe {
        MFGetService(
            session,
            &MR_VIDEO_RENDER_SERVICE,
            &IMFVideoDisplayControl::IID,
            &mut raw,
        )?;
        Ok(IMFVideoDisplayControl::from_raw(raw))
    }
}

fn resize_video(control: &IMFVideoDisplayControl, size: (i32, i32)) -> Result<()> {
    let destination = RECT {
        left: 0,
        top: 0,
        right: size.0.max(1),
        bottom: size.1.max(1),
    };
    unsafe { control.SetVideoPosition(std::ptr::null(), &destination) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_h264_and_hevc_sample_entries() {
        assert_eq!(detect_mov_codec(b"....avc1...."), VideoCodec::H264);
        assert_eq!(detect_mov_codec(b"....avc3...."), VideoCodec::H264);
        assert_eq!(detect_mov_codec(b"....hvc1...."), VideoCodec::Hevc);
        assert_eq!(detect_mov_codec(b"....hev1...."), VideoCodec::Hevc);
        assert_eq!(detect_mov_codec(b"not an mp4"), VideoCodec::Unknown);
    }

    #[test]
    fn hevc_hint_wins_if_multiple_tags_exist() {
        assert_eq!(detect_mov_codec(b"avc1 metadata hvc1"), VideoCodec::Hevc);
    }

    #[test]
    fn notice_status_roundtrips_token_and_hresult() {
        let status = HRESULT(0x8000_4005u32 as i32);
        let packed = pack_notice_status(0x1234_5678, status);
        assert_eq!(unpack_notice_status(packed), (0x1234_5678, status));
    }
}
