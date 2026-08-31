//! Windows Imaging Component decoding for formats supplied by system
//! codecs. LIVP support uses this path for its HEIC/HEIF still image.

use std::error::Error;
use std::ptr;

use image::{DynamicImage, ImageBuffer, Rgba};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapSource, IWICImagingFactory,
    IWICPalette, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::Interface;

use crate::limits;

/// Decode an in-memory image with WIC. When `target_px` is non-zero,
/// ask WIC to deliver at most twice that size on the longest side.
/// The caller performs the final high-quality resize; the 2× headroom
/// avoids throwing away useful sampling detail too early.
pub(super) fn decode(bytes: &[u8], target_px: u32) -> Result<DynamicImage, Box<dyn Error>> {
    if bytes.is_empty() {
        return Err("WIC input is empty".into());
    }

    // SAFETY: the shell invokes thumbnail/preview handlers on a
    // COM-initialised thread. SHCreateMemStream owns a copy of the
    // supplied bytes, and every output buffer passed below is sized
    // from checked WIC dimensions before the call.
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = SHCreateMemStream(Some(bytes)).ok_or("SHCreateMemStream failed")?;
        let decoder = factory.CreateDecoderFromStream(
            &stream,
            ptr::null(),
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;

        let mut source_width = 0u32;
        let mut source_height = 0u32;
        frame.GetSize(&mut source_width, &mut source_height)?;
        validate_dimensions(source_width, source_height)?;

        let (output_width, output_height) =
            thumbnail_dimensions(source_width, source_height, target_px);
        let frame_source: IWICBitmapSource = frame.cast()?;
        let source: IWICBitmapSource =
            if (output_width, output_height) != (source_width, source_height) {
                let scaler = factory.CreateBitmapScaler()?;
                scaler.Initialize(
                    &frame_source,
                    output_width,
                    output_height,
                    WICBitmapInterpolationModeFant,
                )?;
                scaler.into()
            } else {
                frame_source
            };

        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &source,
            &GUID_WICPixelFormat32bppRGBA,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;

        let stride = output_width
            .checked_mul(4)
            .ok_or("WIC output stride overflow")?;
        let byte_count = (stride as u64).saturating_mul(output_height as u64);
        if byte_count > limits::MAX_IMAGE_ALLOC {
            return Err(
                format!("WIC decoded buffer would exceed allocation limit: {byte_count}").into(),
            );
        }
        let byte_count = usize::try_from(byte_count).map_err(|_| "WIC buffer size overflow")?;
        let mut pixels = vec![0u8; byte_count];
        converter.CopyPixels(ptr::null(), stride, &mut pixels)?;

        let image: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_raw(output_width, output_height, pixels)
                .ok_or("WIC RGBA buffer size mismatch")?;
        Ok(DynamicImage::ImageRgba8(image))
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), Box<dyn Error>> {
    if width == 0 || height == 0 {
        return Err(format!("WIC returned invalid dimensions: {width}x{height}").into());
    }
    if width > limits::MAX_IMAGE_DIMENSION || height > limits::MAX_IMAGE_DIMENSION {
        return Err(format!("WIC dimensions too large: {width}x{height}").into());
    }
    Ok(())
}

fn thumbnail_dimensions(width: u32, height: u32, target_px: u32) -> (u32, u32) {
    let requested = target_px.saturating_mul(2);
    let longest = width.max(height);
    if requested == 0 || longest <= requested {
        return (width, height);
    }

    if width >= height {
        let scaled_height = ((height as u64 * requested as u64) / width as u64).max(1) as u32;
        (requested, scaled_height)
    } else {
        let scaled_width = ((width as u64 * requested as u64) / height as u64).max(1) as u32;
        (scaled_width, requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    #[test]
    fn thumbnail_dimensions_preserve_aspect_ratio() {
        assert_eq!(thumbnail_dimensions(4000, 3000, 256), (512, 384));
        assert_eq!(thumbnail_dimensions(3000, 4000, 256), (384, 512));
    }

    #[test]
    fn thumbnail_dimensions_do_not_enlarge_or_scale_for_zero_target() {
        assert_eq!(thumbnail_dimensions(200, 100, 256), (200, 100));
        assert_eq!(thumbnail_dimensions(4000, 3000, 0), (4000, 3000));
    }

    #[test]
    fn thumbnail_dimensions_keep_thin_side_nonzero() {
        assert_eq!(thumbnail_dimensions(32768, 1, 1), (2, 1));
    }

    #[test]
    fn wic_decodes_an_in_memory_image() {
        // PNG is built into WIC on every supported Windows version,
        // so this exercises the same stream/factory/scaler/converter
        // chain as HEIC without requiring a HEIF codec on CI.
        let source = ImageBuffer::from_fn(4, 3, |_, _| Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(source)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode PNG fixture");

        // A unit-test worker is not otherwise a COM apartment. If it
        // was already initialised by another test, keep using that
        // apartment and do not balance it with CoUninitialize here.
        let initialized_here = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let decoded = decode(&png, 0);
        if initialized_here {
            unsafe { CoUninitialize() };
        }

        let decoded = decoded.expect("WIC should decode its built-in PNG format");
        assert_eq!((decoded.width(), decoded.height()), (4, 3));
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0).0, [10, 20, 30, 255]);
    }
}
