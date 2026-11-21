//! Windows: a file's associated icon as PNG bytes (`Icon.ExtractAssociatedIcon(path).ToBitmap()` saved as PNG).
//! Shared by the process list and, later, shortcut and app icons.

#[cfg(windows)]
pub fn associated_icon_png(path: &str) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
        GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
    };
    use windows::Win32::UI::Shell::ExtractAssociatedIconW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    // the API reads the path from (and may write the icon's source into) a 128-unit buffer
    let mut buf = [0u16; 128];
    let wide: Vec<u16> = path.encode_utf16().collect();
    if wide.len() >= buf.len() {
        return None;
    }
    buf[..wide.len()].copy_from_slice(&wide);
    let mut index = 0u16;
    // SAFETY: the icon, its bitmaps and the screen DC are released on every path below; GetDIBits writes at most
    // width * height 32-bit pixels into buffers of exactly that size
    unsafe {
        let icon = ExtractAssociatedIconW(None, &mut buf, &mut index);
        if icon.is_invalid() {
            return None;
        }
        let mut info = ICONINFO::default();
        let got = GetIconInfo(icon, &mut info);
        let _ = DestroyIcon(icon);
        got.ok()?;
        let release = |b: HBITMAP| {
            if !b.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
        };
        let pixels = (|| {
            if info.hbmColor.is_invalid() {
                return None; // a monochrome icon
            }
            let mut bm = BITMAP::default();
            if GetObjectW(
                HGDIOBJ(info.hbmColor.0),
                size_of::<BITMAP>() as i32,
                Some((&mut bm as *mut BITMAP).cast()),
            ) == 0
            {
                return None;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            if w <= 0 || h <= 0 || w > 1024 || h > 1024 {
                return None;
            }
            let read = |bitmap: HBITMAP| -> Option<Vec<u8>> {
                let mut bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -h, // top-down rows
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut out = vec![0u8; (w * h * 4) as usize];
                let dc = GetDC(None);
                let lines = GetDIBits(
                    dc,
                    bitmap,
                    0,
                    h as u32,
                    Some(out.as_mut_ptr().cast()),
                    &mut bmi,
                    DIB_RGB_COLORS,
                );
                ReleaseDC(None, dc);
                (lines == h).then_some(out)
            };
            let mut bgra = read(info.hbmColor)?;
            // an icon without an alpha channel takes its transparency from the mask (white = transparent)
            if bgra.chunks(4).all(|p| p[3] == 0) {
                let mask = read(info.hbmMask)?;
                for (p, m) in bgra.chunks_mut(4).zip(mask.chunks(4)) {
                    p[3] = if m[0] == 0 { 255 } else { 0 };
                }
            }
            for p in bgra.chunks_mut(4) {
                p.swap(0, 2); // BGRA to RGBA
            }
            Some((w as u32, h as u32, bgra))
        })();
        release(info.hbmColor);
        release(info.hbmMask);
        let (w, h, rgba) = pixels?;
        Some(crate::png::encode_rgba(w, h, &rgba))
    }
}
