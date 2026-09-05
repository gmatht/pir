//! Test: verify that bold text renders differently from normal text
//! by comparing screenshots before and after toggling bold.
//! Run with: cargo run --example bold_test
//! Exits 0 on pass (pixels differ), 1 on fail (identical).

use rustxwidgets::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(windows))]
    {
        println!("bold_test requires Windows (NWG backend); skipped");
        return Ok(());
    }
    #[cfg(windows)]
    {
    let app = App::init()?;

    let window = app.create_window()?;
    window.set_title("Bold Test");
    window.set_default_size(600, 200);

    let canvas = app.create_canvas()?;
    canvas.set_size_request(600, 200);
    window.set_child(&canvas);

    let bold = Rc::new(RefCell::new(false));
    let bold_cb = bold.clone();
    canvas.set_draw_callback(Box::new(move |ctx: &mut dyn DrawContext, _w: i32, _h: i32| {
        ctx.clear(1.0, 1.0, 1.0, 1.0);
        let w = if *bold_cb.borrow() { 1 } else { 0 };
        ctx.draw_text_styled(10.0, 50.0, "Hello Bold Test", "monospace", 24.0, 0.0, 0.0, 0.0, 1.0, 0, w);
    }));

    window.present();

    let hwnd = *canvas.as_ref();

    // Force initial paint and drain messages
    pump(hwnd);

    // Capture screenshot 1 (normal weight)
    let (pixels1, w, h) = capture(hwnd)?;

    // Toggle bold
    *bold.borrow_mut() = true;
    canvas.queue_redraw();
    pump(hwnd);

    // Capture screenshot 2 (bold)
    let (pixels2, _, _) = capture(hwnd)?;

    // Save for manual inspection
    save_bmp("bold_normal.bmp", &pixels1, w, h)?;
    save_bmp("bold_active.bmp", &pixels2, w, h)?;

    // Compare
    let diff = count_diff(&pixels1, &pixels2);
    println!("Different pixels: {}", diff);

    if diff > 0 {
        println!("PASS: Bold text looks different from normal text");
        Ok(())
    } else {
        println!("FAIL: Bold text looks identical to normal text!");
        std::process::exit(1);
    }
    }
}

#[cfg(windows)]
fn pump(hwnd: *mut std::os::raw::c_void) {
    unsafe {
        winapi::um::winuser::UpdateWindow(hwnd as _);
        let mut msg: winapi::um::winuser::MSG = std::mem::zeroed();
        for _ in 0..100 {
            if winapi::um::winuser::PeekMessageW(
                &mut msg, std::ptr::null_mut(), 0, 0, winapi::um::winuser::PM_REMOVE,
            ) == 0 {
                break;
            }
            winapi::um::winuser::TranslateMessage(&mut msg);
            winapi::um::winuser::DispatchMessageW(&mut msg);
        }
    }
}

#[cfg(windows)]
fn capture(hwnd: *mut std::os::raw::c_void) -> Result<(Vec<u8>, i32, i32), Box<dyn std::error::Error>> {
    unsafe {
        let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
        winapi::um::winuser::GetClientRect(hwnd as _, &mut rect);
        let w = rect.right;
        let h = rect.bottom;
        if w == 0 || h == 0 {
            return Err("zero-sized window".into());
        }

        let hdc = winapi::um::winuser::GetDC(hwnd as _);
        if hdc.is_null() { return Err("GetDC failed".into()); }

        let mem_dc = winapi::um::wingdi::CreateCompatibleDC(hdc);
        let bmp = winapi::um::wingdi::CreateCompatibleBitmap(hdc, w, h);
        let old = winapi::um::wingdi::SelectObject(mem_dc, bmp as _);
        winapi::um::wingdi::BitBlt(mem_dc, 0, 0, w, h, hdc, 0, 0, winapi::um::wingdi::SRCCOPY);

        let mut bmp_info: winapi::um::wingdi::BITMAPINFO = std::mem::zeroed();
        bmp_info.bmiHeader.biSize = std::mem::size_of::<winapi::um::wingdi::BITMAPINFOHEADER>() as u32;
        bmp_info.bmiHeader.biWidth = w;
        bmp_info.bmiHeader.biHeight = -h; // top-down DIB
        bmp_info.bmiHeader.biPlanes = 1;
        bmp_info.bmiHeader.biBitCount = 32;
        bmp_info.bmiHeader.biCompression = winapi::um::wingdi::BI_RGB;

        let pixel_count = (w * h * 4) as usize;
        let mut pixels = vec![0u8; pixel_count];
        let result = winapi::um::wingdi::GetDIBits(
            mem_dc, bmp, 0, h as u32,
            pixels.as_mut_ptr() as _,
            &mut bmp_info,
            winapi::um::wingdi::DIB_RGB_COLORS,
        );

        winapi::um::wingdi::SelectObject(mem_dc, old);
        winapi::um::wingdi::DeleteObject(bmp as _);
        winapi::um::wingdi::DeleteDC(mem_dc);
        winapi::um::winuser::ReleaseDC(hwnd as _, hdc);

        if result == 0 { return Err("GetDIBits failed".into()); }

        Ok((pixels, w, h))
    }
}

#[cfg(windows)]
fn count_diff(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).filter(|(pa, pb)| pa != pb).count()
}

#[cfg(windows)]
fn save_bmp(path: &str, pixels: &[u8], w: i32, h: i32) -> Result<(), Box<dyn std::error::Error>> {
    let stride = (w * 4) as usize;
    let pixel_size = stride * h as usize;
    let file_size: u32 = 14 + 40 + pixel_size as u32;

    let mut data = Vec::with_capacity(file_size as usize);

    // BITMAPFILEHEADER (14 bytes)
    data.extend_from_slice(b"BM");
    data.extend_from_slice(&file_size.to_le_bytes());
    data.extend_from_slice(&[0u8; 4]);   // reserved1+reserved2
    data.extend_from_slice(&54u32.to_le_bytes()); // bfOffBits

    // BITMAPINFOHEADER (40 bytes)
    data.extend_from_slice(&40u32.to_le_bytes()); // biSize
    data.extend_from_slice(&(w as u32).to_le_bytes());
    data.extend_from_slice(&(h as u32).to_le_bytes()); // positive = bottom-up
    data.extend_from_slice(&1u16.to_le_bytes());  // planes
    data.extend_from_slice(&32u16.to_le_bytes()); // bit count
    data.extend_from_slice(&0u32.to_le_bytes());  // BI_RGB
    data.extend_from_slice(&0u32.to_le_bytes());  // image size (0 ok for BI_RGB)
    data.extend_from_slice(&0u32.to_le_bytes());  // xPelsPerMeter
    data.extend_from_slice(&0u32.to_le_bytes());  // yPelsPerMeter
    data.extend_from_slice(&0u32.to_le_bytes());  // clrUsed
    data.extend_from_slice(&0u32.to_le_bytes());  // clrImportant

    // pixels are top-down (GetDIBits was called with -height),
    // BMP expects bottom-up, so flip rows
    for row in (0..h).rev() {
        let start = (row as usize) * stride;
        data.extend_from_slice(&pixels[start..start + stride]);
    }

    std::fs::write(path, &data)?;
    println!("Saved {}", path);
    Ok(())
}
