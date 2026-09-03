//! UI-owned back buffer shared by cover and software-video painting.
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SRCCOPY, SelectObject,
};
/// Cached backing surface, created and destroyed only on the preview UI thread.
/// Scaling and clearing happen here before a single copy to the visible window.
#[derive(Default)]
pub(super) struct Surface(Option<BackBuffer>);

struct BackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: i32,
    height: i32,
}

impl BackBuffer {
    fn new(dc: HDC, width: i32, height: i32) -> Option<Self> {
        unsafe {
            let memory = CreateCompatibleDC(Some(dc));
            if memory.0.is_null() {
                return None;
            }
            let bitmap = CreateCompatibleBitmap(dc, width, height);
            if bitmap.0.is_null() {
                let _ = DeleteDC(memory);
                return None;
            }
            let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
            if previous.0.is_null() || previous.0 as isize == -1 {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(memory);
                return None;
            }
            Some(Self {
                dc: memory,
                bitmap,
                previous,
                width,
                height,
            })
        }
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

impl Surface {
    pub(super) fn prepare(&mut self, dc: HDC, width: i32, height: i32) -> Option<HDC> {
        if width <= 0 || height <= 0 {
            return None;
        }
        if self
            .0
            .as_ref()
            .is_none_or(|b| b.width != width || b.height != height)
        {
            // Keep the last surface if allocation fails; never clear the window.
            self.0 = Some(BackBuffer::new(dc, width, height)?);
        }
        self.0.as_ref().map(|b| b.dc)
    }

    pub(super) fn present(&self, dc: HDC, rect: RECT) {
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if let Some(buffer) = self
            .0
            .as_ref()
            .filter(|b| b.width == width && b.height == height)
        {
            unsafe {
                let _ = BitBlt(
                    dc,
                    rect.left,
                    rect.top,
                    width,
                    height,
                    Some(buffer.dc),
                    0,
                    0,
                    SRCCOPY,
                );
            }
        }
    }
}
