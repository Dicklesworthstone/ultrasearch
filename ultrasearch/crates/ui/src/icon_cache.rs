use gpui::prelude::*;
use gpui::{AsyncApp, ImageSource, WeakEntity};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub struct IconCache {
    cache: HashMap<String, ImageSource>,
    pending: HashSet<String>,
    temp_dir: PathBuf,
}

impl IconCache {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let temp_dir = std::env::temp_dir().join("ultrasearch_icons");
        std::fs::create_dir_all(&temp_dir).ok();

        Self {
            cache: HashMap::new(),
            pending: HashSet::new(),
            temp_dir,
        }
    }

    pub fn get(&mut self, ext: &str, cx: &mut Context<Self>) -> Option<ImageSource> {
        if let Some(src) = self.cache.get(ext) {
            return Some(src.clone());
        }

        // Ensure we have a normalized extension (with dot)
        let ext_normalized = if ext.starts_with('.') {
            ext.to_string()
        } else {
            format!(".{}", ext)
        };

        if !self.pending.contains(&ext_normalized) {
            self.pending.insert(ext_normalized.clone());
            let ext_clone = ext_normalized.clone();
            let temp_dir = self.temp_dir.clone();

            cx.spawn(|this: WeakEntity<IconCache>, cx: &mut AsyncApp| {
                let mut cx = cx.clone(); // Clone the async context to move into future
                let ext_bg = ext_clone.clone(); // Clone for background task
                async move {
                    // Run blocking Win32 call on background thread
                    let png_data = cx
                        .background_executor()
                        .spawn(async move { load_icon_png(&ext_bg) })
                        .await;

                    // Update model
                    // Note: WeakEntity::update usually expects `&mut AsyncApp` or similar.
                    // Since we have `mut cx: AsyncApp` (owned), we pass `&mut cx`.
                    let _ = this.update(
                        &mut cx,
                        |this: &mut IconCache, cx: &mut Context<IconCache>| {
                            this.pending.remove(&ext_clone);
                            if let Some(data) = png_data {
                                // Write to temp file
                                let hash = md5::compute(&ext_clone);
                                let file_path = temp_dir.join(format!("{:x}.png", hash));
                                if std::fs::write(&file_path, data).is_ok() {
                                    // ImageSource::from(PathBuf) works
                                    let source = ImageSource::from(file_path);
                                    this.cache.insert(ext_clone, source);
                                    cx.notify();
                                }
                            }
                        },
                    );
                }
            })
            .detach();
        }

        None
    }
}

#[cfg(target_os = "windows")]
fn load_icon_png(ext: &str) -> Option<Vec<u8>> {
    use image::{ImageBuffer, Rgba};
    use std::ffi::c_void;
    use std::io::Cursor;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, BITMAP,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo};

    unsafe {
        let mut shfi = SHFILEINFOW::default();
        let ext_wide = HSTRING::from(ext);

        let result = SHGetFileInfoW(
            PCWSTR(ext_wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0x80),
            Some(&mut shfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        );

        if result == 0 || shfi.hIcon.is_invalid() {
            return None;
        }

        let hicon = shfi.hIcon;

        let mut icon_info = std::mem::zeroed();
        if GetIconInfo(hicon, &mut icon_info).is_err() {
            let _ = DestroyIcon(hicon);
            return None;
        }

        let dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(dc);

        let hbm = if !icon_info.hbmColor.is_invalid() {
            icon_info.hbmColor
        } else {
            icon_info.hbmMask
        };

        let mut bmp = std::mem::zeroed::<BITMAP>();
        GetObjectW(
            hbm,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut c_void),
        );

        let width = bmp.bmWidth;
        let height = bmp.bmHeight;

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // Top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut pixels: Vec<u8> = vec![0; (width * height * 4) as usize];

        let success = GetDIBits(
            mem_dc,
            hbm,
            0,
            height.unsigned_abs(),
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        // Cleanup GDI objects
        let _ = DeleteObject(icon_info.hbmColor);
        let _ = DeleteObject(icon_info.hbmMask);
        let _ = DeleteDC(mem_dc);
        windows::Win32::Graphics::Gdi::ReleaseDC(None, dc);
        let _ = DestroyIcon(hicon);

        if success == 0 {
            return None;
        }

        // Convert BGRA -> RGBA
        for chunk in pixels.as_chunks_mut::<4>().0 {
            let b = chunk[0];
            let g = chunk[1];
            let r = chunk[2];
            let a = chunk[3];
            chunk[0] = r;
            chunk[1] = g;
            chunk[2] = b;
            chunk[3] = a;
        }

        // Encode to PNG
        let buffer: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_raw(width as u32, height.unsigned_abs(), pixels)?;
        let mut png_bytes = Vec::new();
        let mut cursor = Cursor::new(&mut png_bytes);
        buffer.write_to(&mut cursor, image::ImageFormat::Png).ok()?;

        Some(png_bytes)
    }
}

#[cfg(not(target_os = "windows"))]
fn load_icon_png(_ext: &str) -> Option<Vec<u8>> {
    None
}
