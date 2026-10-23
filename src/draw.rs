use crate::backend::settings_dir;
use std::collections::VecDeque;
use windows::{
    core::{w, Interface, Result, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            DirectWrite::*,
            Dxgi::Common::*,
            Imaging::*,
        },
        System::Com::*,
    },
};

pub const WHITE: u32 = 0xeeeeee;
pub const GREY: u32 = 0x8e8e93;
pub const GREEN: u32 = 0x26cf78;

pub fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}
fn color(rgb: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 255) as f32 / 255.,
        g: ((rgb >> 8) & 255) as f32 / 255.,
        b: (rgb & 255) as f32 / 255.,
        a: 1.,
    }
}

pub struct Painter {
    pub target: ID2D1HwndRenderTarget,
    factory: IDWriteFactory,
    fonts: IDWriteFontCollection,
    brush: ID2D1SolidColorBrush,
    wic: IWICImagingFactory,
    layouts: VecDeque<(String, u32, u32, bool, IDWriteTextLayout, f32)>,
    bitmaps: VecDeque<(String, u32, u32, bool, ID2D1Bitmap)>,
    bitmap_bytes: usize,
}
impl Painter {
    pub unsafe fn new(hwnd: HWND, width: u32, height: u32, dpi: f32) -> Result<Self> {
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let target = d2d.CreateHwndRenderTarget(
            &D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            },
            &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width, height },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            },
        )?;
        let brush = target.CreateSolidColorBrush(&color(WHITE), None)?;
        let factory: IDWriteFactory3 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let builder = factory.CreateFontSetBuilder()?;
        let dir = settings_dir().join("fonts");
        let _ = std::fs::create_dir_all(&dir);
        let license = dir.join("IBM-Plex-LICENSE.txt");
        if !license.exists() {
            std::fs::write(&license, include_bytes!("../assets/IBM-Plex-LICENSE.txt"))
                .map_err(|_| windows::core::Error::from_win32())?;
        }
        for (name, bytes) in [
            (
                "IBMPlexSans-Regular.ttf",
                include_bytes!("../assets/IBMPlexSans-Regular.ttf").as_slice(),
            ),
            (
                "IBMPlexSans-Bold.ttf",
                include_bytes!("../assets/IBMPlexSans-Bold.ttf").as_slice(),
            ),
        ] {
            let path = dir.join(name);
            if !path.exists() {
                std::fs::write(&path, bytes).map_err(|_| windows::core::Error::from_win32())?;
            }
            let wide: Vec<u16> = path
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let face = factory.CreateFontFaceReference2(
                PCWSTR(wide.as_ptr()),
                None,
                0,
                DWRITE_FONT_SIMULATIONS_NONE,
            )?;
            builder.AddFontFaceReference2(&face)?;
        }
        let fonts = factory
            .CreateFontCollectionFromFontSet(&builder.CreateFontSet()?)?
            .cast()?;
        let wic = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        Ok(Self {
            target,
            brush,
            factory: factory.cast()?,
            fonts,
            wic,
            layouts: VecDeque::new(),
            bitmaps: VecDeque::new(),
            bitmap_bytes: 0,
        })
    }
    pub unsafe fn fill(&self, r: D2D_RECT_F, radius: f32, rgb: u32) {
        self.brush.SetColor(&color(rgb));
        self.target.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: radius,
                radiusY: radius,
            },
            &self.brush,
        );
    }
    pub unsafe fn circle(&self, x: f32, y: f32, r: f32, rgb: u32) {
        self.brush.SetColor(&color(rgb));
        self.target.FillEllipse(
            &D2D1_ELLIPSE {
                point: D2D_POINT_2F { x, y },
                radiusX: r,
                radiusY: r,
            },
            &self.brush,
        );
    }
    unsafe fn layout(
        &mut self,
        text: &str,
        width: f32,
        size: f32,
        bold: bool,
    ) -> Result<(IDWriteTextLayout, f32)> {
        let width = width.max(1.);
        if let Some(i) = self.layouts.iter().position(|(t, w, s, b, _, _)| {
            t == text && *w == width.to_bits() && *s == size.to_bits() && *b == bold
        }) {
            let entry = self.layouts.remove(i).unwrap();
            let result = (entry.4.clone(), entry.5);
            self.layouts.push_back(entry);
            return Ok(result);
        }
        let format = self.factory.CreateTextFormat(
            w!("IBM Plex Sans"),
            &self.fonts,
            if bold {
                DWRITE_FONT_WEIGHT_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            },
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )?;
        let layout = self.factory.CreateTextLayout(
            &text.encode_utf16().collect::<Vec<_>>(),
            &format,
            width,
            10000.,
        )?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        if self.layouts.len() >= 256 {
            self.layouts.pop_front();
        }
        self.layouts.push_back((
            text.into(),
            width.to_bits(),
            size.to_bits(),
            bold,
            layout.clone(),
            metrics.height,
        ));
        Ok((layout, metrics.height))
    }
    pub unsafe fn height(&mut self, text: &str, width: f32, size: f32, bold: bool) -> f32 {
        self.layout(text, width, size, bold)
            .map(|(_, h)| h)
            .unwrap_or(size * 1.4)
    }
    pub unsafe fn text(&mut self, text: &str, r: D2D_RECT_F, size: f32, bold: bool, rgb: u32) {
        if let Ok((layout, _)) = self.layout(text, r.right - r.left, size, bold) {
            self.brush.SetColor(&color(rgb));
            self.target
                .PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            self.target.DrawTextLayout(
                D2D_POINT_2F {
                    x: r.left,
                    y: r.top,
                },
                &layout,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
            self.target.PopAxisAlignedClip();
        }
    }
    pub unsafe fn right_text(
        &mut self,
        text: &str,
        r: D2D_RECT_F,
        size: f32,
        bold: bool,
        rgb: u32,
    ) {
        if let Ok((layout, _)) = self.layout(text, r.right - r.left, size, bold) {
            let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
            self.text(text, r, size, bold, rgb);
            let _ = layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
        }
    }
    pub fn images_cached(&self) -> usize {
        self.bitmaps.len()
    }
    pub fn bitmap_bytes(&self) -> usize {
        self.bitmap_bytes
    }
    unsafe fn bitmap(&mut self, path: &str, w: u32, h: u32, round: bool) -> Result<ID2D1Bitmap> {
        if let Some(i) = self
            .bitmaps
            .iter()
            .position(|(p, cw, ch, cr, _)| p == path && *cw == w && *ch == h && *cr == round)
        {
            let entry = self.bitmaps.remove(i).unwrap();
            let bitmap = entry.4.clone();
            self.bitmaps.push_back(entry);
            return Ok(bitmap);
        }
        let path_wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let decoder = self.wic.CreateDecoderFromFilename(
            PCWSTR(path_wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let source = decoder.GetFrame(0)?;
        let (mut width, mut height) = (0, 0);
        source.GetSize(&mut width, &mut height)?;
        let source: IWICBitmapSource = if round {
            let side = width.min(height);
            let clipper = self.wic.CreateBitmapClipper()?;
            clipper.Initialize(
                &source,
                &WICRect {
                    X: ((width - side) / 2) as i32,
                    Y: ((height - side) / 2) as i32,
                    Width: side as i32,
                    Height: side as i32,
                },
            )?;
            width = side;
            height = side;
            clipper.cast()?
        } else {
            source.cast()?
        };
        let scale = (w as f32 / width.max(1) as f32)
            .min(h as f32 / height.max(1) as f32)
            .min(1.);
        let (width, height) = (
            (width as f32 * scale).max(1.) as u32,
            (height as f32 * scale).max(1.) as u32,
        );
        let scaler = self.wic.CreateBitmapScaler()?;
        scaler.Initialize(&source, width, height, WICBitmapInterpolationModeFant)?;
        let converter = self.wic.CreateFormatConverter()?;
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.,
            WICBitmapPaletteTypeCustom,
        )?;
        let bitmap = if round {
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            converter.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
            mask_circle(&mut pixels, width as usize);
            self.target.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(pixels.as_ptr().cast()),
                width * 4,
                &D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.,
                    dpiY: 96.,
                },
            )?
        } else {
            self.target.CreateBitmapFromWicBitmap(&converter, None)?
        };
        let bytes = (width * height * 4) as usize;
        // LRU is bounded by decoded bytes as well as count, not original file size.
        while !self.bitmaps.is_empty()
            && (self.bitmap_bytes + bytes > 4 * 1024 * 1024 || self.bitmaps.len() >= 32)
        {
            let old = self.bitmaps.pop_front().unwrap().4.GetPixelSize();
            self.bitmap_bytes -= (old.width * old.height * 4) as usize;
        }
        self.bitmap_bytes += bytes;
        self.bitmaps
            .push_back((path.into(), w, h, round, bitmap.clone()));
        Ok(bitmap)
    }
    pub unsafe fn image(&mut self, path: &str, r: D2D_RECT_F) -> bool {
        self.draw_image(path, r, false)
    }
    pub unsafe fn avatar(&mut self, path: &str, r: D2D_RECT_F) -> bool {
        self.draw_image(path, r, true)
    }
    unsafe fn draw_image(&mut self, path: &str, r: D2D_RECT_F, round: bool) -> bool {
        let (mut dx, mut dy) = (96., 96.);
        self.target.GetDpi(&mut dx, &mut dy);
        if let Ok(bitmap) = self.bitmap(
            path,
            ((r.right - r.left) * dx / 96.).ceil() as u32,
            ((r.bottom - r.top) * dy / 96.).ceil() as u32,
            round,
        ) {
            let size = bitmap.GetSize();
            let scale = ((r.right - r.left) / size.width).min((r.bottom - r.top) / size.height);
            let dest = rect(r.left, r.top, size.width * scale, size.height * scale);
            self.target.DrawBitmap(
                &bitmap,
                Some(&dest),
                1.,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
            true
        } else {
            false
        }
    }
}

fn mask_circle(pixels: &mut [u8], size: usize) {
    let radius = size as f32 / 2.;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - radius;
            let dy = y as f32 + 0.5 - radius;
            let alpha = ((0.5 - ((dx * dx + dy * dy).sqrt() - radius)).clamp(0., 1.) * 255.) as u16;
            for channel in &mut pixels[(y * size + x) * 4..(y * size + x + 1) * 4] {
                *channel = (*channel as u16 * alpha / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zig_avatar_mask_premultiplies_and_clears_corners() {
        for size in [42, 63] {
            let mut pixels = vec![128; size * size * 4];
            mask_circle(&mut pixels, size);
            assert_eq!(&pixels[..4], &[0; 4]);
            assert_eq!(pixels[(size / 2 * size + size / 2) * 4 + 3], 128);
            let edge = pixels[(size / 2 * size + size - 1) * 4 + 3];
            assert!(edge > 0 && edge < 128);
            for pixel in pixels.chunks_exact(4) {
                assert_eq!(pixel[0], pixel[3]);
            }
        }
    }
}
