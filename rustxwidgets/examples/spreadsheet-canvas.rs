use rustxwidgets::prelude::*;
use rustxwidgets::backends_gtk_adapter as gtk;
use std::rc::Rc;
use std::cell::RefCell;
use std::ffi::CString;

const VISIBLE_ROWS: usize = 100;
const VISIBLE_COLS: usize = 26;
const CELL_W: i32 = 100;
const CELL_H: i32 = 28;

type CellFormat = (bool, bool, u8, String, String); // bold, italic, align, fg, bg

fn col_to_label(n: usize) -> String {
    if n < 26 {
        format!("{}", (b'A' + (n as u8)) as char)
    } else {
        let mut s = String::new();
        let mut v = n;
        loop {
            s.insert(0, (b'A' + (v % 26) as u8) as char);
            v /= 26;
            if v == 0 { break; }
            v -= 1;
        }
        s
    }
}

fn lookup_sym<T: Copy>(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, name: &str) -> Option<T> {
    let gtk_lib = loader.libs.get("libgtk")?;
    let lib: &libloading::os::unix::Library = &*gtk_lib;
    unsafe { lib.get::<T>(name.as_bytes()).ok().map(|s| { let v = *s; drop(s); v }) }
}

fn make_overlay(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>) -> Option<*mut std::ffi::c_void> {
    let overlay_new = loader.symbols.gtk_overlay_new?;
    let overlay = unsafe { overlay_new() };
    if overlay.is_null() { return None; }
    gtk_dynamic_loader::take_ownership(&*loader.symbols, &loader.version(), overlay);
    Some(overlay)
}

fn set_overlay_child(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, overlay: *mut std::ffi::c_void, child: *mut std::ffi::c_void, is_gtk4: bool) {
    if is_gtk4 {
        type SetChild = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);
        if let Some(set_child) = lookup_sym::<SetChild>(loader, "gtk_overlay_set_child") {
            unsafe { set_child(overlay, child); }
        }
    } else {
        if let Some(container_add) = loader.symbols.gtk_container_add {
            unsafe { container_add(overlay, child); }
        }
    }
}

fn add_overlay_child(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, overlay: *mut std::ffi::c_void, child: *mut std::ffi::c_void) {
    if let Some(add_overlay) = loader.symbols.gtk_overlay_add_overlay {
        unsafe { add_overlay(overlay, child); }
    } else if let Some(container_add) = loader.symbols.gtk_container_add {
        unsafe { container_add(overlay, child); }
    }
}

fn set_overlay_pass_through(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, overlay: *mut std::ffi::c_void, child: *mut std::ffi::c_void, pass: bool) {
    if let Some(set_pass) = loader.symbols.gtk_overlay_set_overlay_pass_through {
        unsafe { set_pass(overlay, child, if pass { 1 } else { 0 }); }
    }
}

fn set_can_target(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, widget: *mut std::ffi::c_void, can_target: bool) {
    if let Some(set) = loader.symbols.gtk_widget_set_can_target {
        unsafe { set(widget, if can_target { 1 } else { 0 }); }
    }
}

fn set_halign(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, widget: *mut std::ffi::c_void, align: i32) {
    if let Some(set) = loader.symbols.gtk_widget_set_halign {
        unsafe { set(widget, align); }
    }
}

fn set_valign(loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, widget: *mut std::ffi::c_void, align: i32) {
    if let Some(set) = loader.symbols.gtk_widget_set_valign {
        unsafe { set(widget, align); }
    }
}

fn compute_row_y(heights: &[i32], row: usize) -> i32 {
    let mut y = 0;
    for i in 0..row {
        if i < heights.len() { y += heights[i]; }
    }
    y
}

fn parse_color(hex: &str) -> (f64, f64, f64) {
    if hex.len() >= 7 && hex.as_bytes()[0] == b'#' {
        let r = u8::from_str_radix(&hex[1..3], 16).unwrap_or(0) as f64 / 255.0;
        let g = u8::from_str_radix(&hex[3..5], 16).unwrap_or(0) as f64 / 255.0;
        let b = u8::from_str_radix(&hex[5..7], 16).unwrap_or(0) as f64 / 255.0;
        (r, g, b)
    } else {
        (0.0, 0.0, 0.0)
    }
}

#[repr(C)]
struct CairoTextExtentsT {
    x_bearing: f64,
    y_bearing: f64,
    width: f64,
    height: f64,
    x_advance: f64,
    y_advance: f64,
}

fn draw_grid(
    cr: *mut std::ffi::c_void,
    _w: i32,
    _h: i32,
    loader: &std::sync::Arc<gtk_dynamic_loader::Loader>,
    texts: &RefCell<Vec<Vec<String>>>,
    fmts: &RefCell<Vec<Vec<CellFormat>>>,
    sel: &RefCell<Option<(usize, usize)>>,
    _col_widths: &RefCell<Vec<i32>>,
    _row_heights: &RefCell<Vec<i32>>,
) {
    let s = &loader.symbols;
    macro_rules! c {
        ($f:ident($($a:expr),*)) => {
            if let Some(f) = s.$f { unsafe { f($($a),*) } }
        };
    }

    let chw = 46_f64;
    let cw = CELL_W as f64;
    let ch = CELL_H as f64;
    let total_w = chw + VISIBLE_COLS as f64 * cw;
    let total_h = ch + VISIBLE_ROWS as f64 * ch;

    c!(cairo_set_source_rgb(cr, 1.0, 1.0, 1.0));
    c!(cairo_rectangle(cr, 0.0, 0.0, total_w, total_h));
    c!(cairo_fill(cr));

    c!(cairo_set_source_rgb(cr, 0.91, 0.91, 0.91));
    c!(cairo_rectangle(cr, 0.0, 0.0, chw, ch));
    c!(cairo_fill(cr));

    c!(cairo_set_source_rgb(cr, 0.8, 0.8, 0.8));
    c!(cairo_rectangle(cr, chw, 0.0, VISIBLE_COLS as f64 * cw, ch));
    c!(cairo_fill(cr));
    c!(cairo_rectangle(cr, 0.0, ch, chw, VISIBLE_ROWS as f64 * ch));
    c!(cairo_fill(cr));

    c!(cairo_set_source_rgb(cr, 0.7, 0.7, 0.7));
    c!(cairo_set_line_width(cr, 0.5));
    for c in 0..=VISIBLE_COLS {
        let x = chw + c as f64 * cw;
        c!(cairo_move_to(cr, x, 0.0));
        c!(cairo_line_to(cr, x, total_h));
        c!(cairo_stroke(cr));
    }
    for r in 0..=VISIBLE_ROWS {
        let y = ch + r as f64 * ch;
        c!(cairo_move_to(cr, 0.0, y));
        c!(cairo_line_to(cr, total_w, y));
        c!(cairo_stroke(cr));
    }

    c!(cairo_select_font_face(cr, CString::new("monospace").unwrap().as_ptr(), 0, 1));
    c!(cairo_set_font_size(cr, 12.0));
    c!(cairo_set_source_rgb(cr, 0.0, 0.0, 0.0));
    for c in 0..VISIBLE_COLS {
        let lbl = col_to_label(c);
        let c_lbl = CString::new(lbl.as_str()).unwrap();
        let mut ext: CairoTextExtentsT = unsafe { std::mem::zeroed() };
        c!(cairo_text_extents(cr, c_lbl.as_ptr(), &mut ext as *mut _ as *mut std::ffi::c_void));
        let x = chw + c as f64 * cw + cw / 2.0 - ext.x_bearing - ext.width / 2.0;
        let y = ch / 2.0 - ext.y_bearing - ext.height / 2.0;
        c!(cairo_move_to(cr, x, y));
        c!(cairo_show_text(cr, c_lbl.as_ptr()));
    }

    c!(cairo_set_font_size(cr, 12.0));
    for r in 0..VISIBLE_ROWS {
        let lbl = format!("{}", r + 1);
        let c_lbl = CString::new(lbl.as_str()).unwrap();
        let mut ext: CairoTextExtentsT = unsafe { std::mem::zeroed() };
        c!(cairo_text_extents(cr, c_lbl.as_ptr(), &mut ext as *mut _ as *mut std::ffi::c_void));
        let x = chw / 2.0 - ext.x_bearing - ext.width / 2.0;
        let y = ch + r as f64 * ch + ch / 2.0 - ext.y_bearing - ext.height / 2.0;
        c!(cairo_move_to(cr, x, y));
        c!(cairo_show_text(cr, c_lbl.as_ptr()));
    }

    let t = texts.borrow();
    let f = fmts.borrow();
    c!(cairo_set_font_size(cr, 13.0));
    for r in 0..VISIBLE_ROWS {
        for c in 0..VISIBLE_COLS {
            let text = &t[r][c];
            if text.is_empty() { continue; }
            let (bold, italic, align, fg_hex, bg_hex) = &f[r][c];

            let cx = chw + c as f64 * cw;
            let cy = ch + r as f64 * ch;

            if bg_hex != "#ffffff" {
                let (br, bg, bb) = parse_color(bg_hex);
                c!(cairo_set_source_rgb(cr, br, bg, bb));
                c!(cairo_rectangle(cr, cx, cy, cw, ch));
                c!(cairo_fill(cr));
            }

            if fg_hex != "#000000" {
                let (fr, fgg, fb) = parse_color(fg_hex);
                c!(cairo_set_source_rgb(cr, fr, fgg, fb));
            } else {
                c!(cairo_set_source_rgb(cr, 0.0, 0.0, 0.0));
            }

            let slant = if *italic { 1 } else { 0 };
            let weight = if *bold { 1 } else { 0 };
            c!(cairo_select_font_face(cr, CString::new("monospace").unwrap().as_ptr(), slant, weight));

            let c_text = CString::new(text.as_str()).unwrap();
            let mut ext: CairoTextExtentsT = unsafe { std::mem::zeroed() };
            c!(cairo_text_extents(cr, c_text.as_ptr(), &mut ext as *mut _ as *mut std::ffi::c_void));

            let pad = 4.0;
            let tx = match *align {
                0 => cx + pad - ext.x_bearing,
                1 => cx + cw / 2.0 - ext.x_bearing - ext.width / 2.0,
                _ => cx + cw - pad - ext.x_bearing - ext.width,
            };
            let ty = cy + ch / 2.0 - ext.y_bearing - ext.height / 2.0;

            c!(cairo_save(cr));
            let text_left = tx + ext.x_bearing;
            let text_right = text_left + ext.width;
            let cell_right = cx + cw;
            let clip_right = if text_right > cell_right {
                let mut cr2 = text_right;
                for oc in (c + 1)..VISIBLE_COLS {
                    if !t[r][oc].is_empty() {
                        let ocx = chw + oc as f64 * cw;
                        cr2 = cr2.min(ocx);
                        break;
                    }
                    cr2 = (chw + (oc + 1) as f64 * cw).max(cr2);
                }
                cr2.min(total_w)
            } else {
                cell_right
            };
            c!(cairo_rectangle(cr, cx, cy, clip_right - cx, ch));
            c!(cairo_clip(cr));
            c!(cairo_move_to(cr, tx, ty));
            c!(cairo_show_text(cr, c_text.as_ptr()));
            c!(cairo_restore(cr));
        }
    }
    drop(t);
    drop(f);

    if let Some((sr, sc)) = *sel.borrow() {
        let sx = chw + sc as f64 * cw;
        let sy = ch + sr as f64 * ch;
        c!(cairo_set_source_rgba(cr, 0.83, 0.91, 1.0, 0.3));
        c!(cairo_rectangle(cr, sx, sy, cw, ch));
        c!(cairo_fill(cr));
        c!(cairo_set_source_rgb(cr, 0.1, 0.45, 0.91));
        c!(cairo_set_line_width(cr, 2.0));
        c!(cairo_rectangle(cr, sx + 0.5, sy + 0.5, cw - 1.0, ch - 1.0));
        c!(cairo_stroke(cr));
    }
}

struct ScopedDrawingArea(*mut std::ffi::c_void);
impl AsRef<*mut std::ffi::c_void> for ScopedDrawingArea {
    fn as_ref(&self) -> &*mut std::ffi::c_void { &self.0 }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--prefer-gtk3" || a == "-3") {
        std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1");
    }

    let loader = match rustxwidgets::backends::gtk::loader() {
        Some(l) => l,
        None => { let _ = App::init()?; rustxwidgets::backends::gtk::loader().expect("loader") }
    };

    let is_gtk4 = loader.symbols.gtk_container_add.is_none();
    let app = App::init()?;
    let win = app.create_window()?;
    win.set_title("Spreadsheet");
    win.set_default_size(1600, 1000);

    // Data model
    let texts: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    for _r in 0..VISIBLE_ROWS {
        texts.borrow_mut().push(vec![String::new(); VISIBLE_COLS]);
    }
    {
        let mut t = texts.borrow_mut();
        t[0][0] = "Short".into();
        t[1][0] = "VeryLongHeaderThatOverflows".into();
        t[2][0] = "CellWithAVeryLongWordThatWillSpanMultipleCells".into();
        t[3][0] = "NoOverflowHereBcNextIsUsed".into();
        t[3][1] = "X".into();
        t[4][0] = "42".into();
        t[4][1] = "=SUM".into();
        t[5][0] = "TRUE".into();
        t[5][1] = "FALSE".into();
        t[6][0] = "-123.45".into();
        t[7][0] = "Hello".into();
        t[7][1] = "World".into();
        t[8][0] = "Revenue".into();
        t[8][1] = "1000".into();
        t[8][2] = "2000".into();
        t[8][3] = "=SUM(B9:C9)".into();
    }

    // Per-cell formatting
    let cell_formats: Rc<RefCell<Vec<Vec<CellFormat>>>> = Rc::new(RefCell::new(
        (0..VISIBLE_ROWS).map(|_| (0..VISIBLE_COLS).map(|_| (false, false, 0u8, "#000000".into(), "#ffffff".into())).collect()).collect()
    ));

    // Column widths (constant)
    let col_widths: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(vec![CELL_W; VISIBLE_COLS]));
    let row_heights: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(vec![CELL_H; VISIBLE_ROWS]));

    // Toolbar
    let toolbar = gtk::create_box(gtk::Orientation::Horizontal, 2)?;
    let open_btn = app.create_button("Open")?;
    let save_btn = app.create_button("Save As")?;
    let quit_btn = app.create_button("Quit")?;
    let bold_btn = app.create_button("B")?;
    let italic_btn = app.create_button("I")?;
    let al_l_btn = app.create_button("AL")?;
    let al_c_btn = app.create_button("AC")?;
    let al_r_btn = app.create_button("AR")?;
    let hl_btn = app.create_button("HL")?;
    let fg_btn = app.create_button("FG")?;
    gtk_dynamic_loader::widget_set_size_request(&loader, *open_btn.as_ref(), 60, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *save_btn.as_ref(), 70, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *quit_btn.as_ref(), 50, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *bold_btn.as_ref(), 28, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *italic_btn.as_ref(), 28, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *al_l_btn.as_ref(), 28, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *al_c_btn.as_ref(), 28, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *al_r_btn.as_ref(), 28, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *hl_btn.as_ref(), 28, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *fg_btn.as_ref(), 28, 24);
    toolbar.append(&open_btn);
    toolbar.append(&save_btn);
    toolbar.append(&quit_btn);
    toolbar.append(&bold_btn);
    toolbar.append(&italic_btn);
    toolbar.append(&al_l_btn);
    toolbar.append(&al_c_btn);
    toolbar.append(&al_r_btn);
    toolbar.append(&hl_btn);
    toolbar.append(&fg_btn);

    // Formula bar
    let formula_hbox = gtk::create_box(gtk::Orientation::Horizontal, 4)?;
    let fx_label = app.create_label("  fx  ")?;
    formula_hbox.append(&fx_label);
    let formula_entry = gtk::create_entry()?;
    formula_entry.set_width_chars(40);
    formula_entry.set_size_request(400, 26);
    formula_hbox.append(&formula_entry);

    // DrawingArea (canvas) replaces the GtkGrid
    let drawing_area = gtk_dynamic_loader::DrawingArea::new(loader.clone())?;
    let overlay_ptr = make_overlay(&loader).expect("failed to create overlay");
    let draw_ptr_raw = *drawing_area.as_ref();
    set_overlay_child(&loader, overlay_ptr, draw_ptr_raw, is_gtk4);

    // Draw callback
    let selected_coord: Rc<RefCell<Option<(usize, usize)>>> = Rc::new(RefCell::new(Some((0, 0))));
    let editing_entry: Rc<RefCell<Option<gtk::Entry>>> = Rc::new(RefCell::new(None));

    if is_gtk4 {
        let ld = loader.clone();
        let tx = texts.clone();
        let fm = cell_formats.clone();
        let sl = selected_coord.clone();
        let cw_draw = col_widths.clone();
        let rh_draw = row_heights.clone();
        drawing_area.set_draw_func(Box::new(move |cr, w, h| {
            draw_grid(cr, w, h, &ld, &tx, &fm, &sl, &cw_draw, &rh_draw);
        })).ok();
        drawing_area.set_content_width(46 + VISIBLE_COLS as i32 * CELL_W);
        drawing_area.set_content_height(CELL_H + VISIBLE_ROWS as i32 * CELL_H);
    } else {
        let ld = loader.clone();
        let tx = texts.clone();
        let fm = cell_formats.clone();
        let sl = selected_coord.clone();
        let cw = col_widths.clone();
        let rh = row_heights.clone();
        unsafe {
            drawing_area.connect_draw_gtk3(Box::new(move |_widget, cr| {
                draw_grid(cr, 0, 0, &ld, &tx, &fm, &sl, &cw, &rh);
                0
            })).ok();
        }
    }

    // Prevent Rust Drop from double-freeing (overlay owns it now)
    let da_ptr = ScopedDrawingArea(*drawing_area.as_ref());
    std::mem::forget(drawing_area);

    // CSS for toolbar/entry only (no label styles needed anymore)
    if let Some(loader2) = rustxwidgets::backends::gtk::loader() {
        let css = r#"
        button { font-size: 11px; padding: 1px 8px; min-height: 20px; }
        entry { border: 1px solid #000000; font-family: monospace; font-size: 13px; min-height: 24px; }
        "#;
        if let Some(provider) = gtk_dynamic_loader::create_css_provider(&loader2, css) {
            gtk_dynamic_loader::add_css_provider_global(&loader2, *win.as_ref(), provider, 600);
        }
    }

    // ScrolledWindow
    let scrolled_ptr: *mut std::ffi::c_void = unsafe {
        type ScrolledNew = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        type SetPolicy = unsafe extern "C" fn(*mut std::ffi::c_void, u32, u32);
        type SetChild = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);
        type SetExpand = unsafe extern "C" fn(*mut std::ffi::c_void, i32);

        if let Some(scrolled_new) = lookup_sym::<ScrolledNew>(&loader, "gtk_scrolled_window_new") {
            let sw = scrolled_new(std::ptr::null_mut(), std::ptr::null_mut());
            if sw.is_null() { panic!("scrolled_window_new returned null"); }
            gtk_dynamic_loader::take_ownership(&*loader.symbols, &loader.version(), sw);
            if let Some(set_policy) = lookup_sym::<SetPolicy>(&loader, "gtk_scrolled_window_set_policy") {
                set_policy(sw, 0, 0);
            }
            if is_gtk4 {
                if let Some(set_child) = lookup_sym::<SetChild>(&loader, "gtk_scrolled_window_set_child") {
                    set_child(sw, overlay_ptr);
                }
            } else {
                if let Some(container_add) = loader.symbols.gtk_container_add {
                    container_add(sw, overlay_ptr);
                }
            }
            if let Some(set_h) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_hexpand") { set_h(sw, 1); }
            if let Some(set_v) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_vexpand") { set_v(sw, 1); }
            if let Some(set_h) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_hexpand") { set_h(overlay_ptr, 1); }
            if let Some(set_v) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_vexpand") { set_v(overlay_ptr, 1); }
            let total_w = 46 + VISIBLE_COLS as i32 * CELL_W;
            let total_h = CELL_H + VISIBLE_ROWS as i32 * CELL_H;
            gtk_dynamic_loader::widget_set_size_request(&loader, draw_ptr_raw, total_w, total_h);
            sw
        } else { panic!("gtk_scrolled_window_new not available"); }
    };

    // Queue redraw helper
    let loader_qr = loader.clone();
    let queue_redraw = {
        let da = da_ptr.as_ref();
        let d = *da;
        move || {
            if let Some(q) = loader_qr.symbols.gtk_widget_queue_draw { unsafe { q(d); } }
        }
    };
    let queue_redraw = Rc::new(queue_redraw);

    // Helper to start editing a cell
    let start_edit = {
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let overlay_for_edit = overlay_ptr;
        let loader_for_edit = loader.clone();
        let rh_edit = row_heights.clone();
        move |r: usize, c: usize| {
            if edit_entry_nav.borrow().is_some() { return; }
            if let Ok(entry) = gtk::create_entry() {
                entry.set_text(&texts_nav.borrow()[r][c]);
                let rh = rh_edit.borrow();
                let left = 46 + c as i32 * CELL_W;
                let top = CELL_H + compute_row_y(&rh, r);
                drop(rh);
                gtk_dynamic_loader::widget_set_margin_start(&loader_for_edit, *entry.as_ref(), left);
                gtk_dynamic_loader::widget_set_margin_top(&loader_for_edit, *entry.as_ref(), top);
                set_halign(&loader_for_edit, *entry.as_ref(), 1);
                set_valign(&loader_for_edit, *entry.as_ref(), 1);
                add_overlay_child(&loader_for_edit, overlay_for_edit, *entry.as_ref());
                set_overlay_pass_through(&loader_for_edit, overlay_for_edit, *entry.as_ref(), false);
                entry.set_size_request(CELL_W, rh_edit.borrow()[r]);
                entry.grab_focus();
                *edit_entry_nav.borrow_mut() = Some(entry);
            }
        }
    };

    // Commit editing entry
    let commit_edit = {
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let formula_e = formula_entry.clone();
        let qr = queue_redraw.clone();
        let sel_nav = selected_coord.clone();
        move || {
            let entry_opt = edit_entry_nav.borrow_mut().take();
            if let Some(e) = entry_opt {
                let new_text = e.get_text().unwrap_or_default();
                let coord = *sel_nav.borrow();
                if let Some((r, c)) = coord {
                    if let Ok(mut t) = texts_nav.try_borrow_mut() {
                        if r < t.len() && c < t[r].len() { t[r][c] = new_text.clone(); }
                    }
                }
                formula_e.set_text(&new_text);
                qr();
            }
        }
    };

    // Refresh cell display when selection moves
    let refresh_selection = {
        let texts_nav = texts.clone();
        let formula_e = formula_entry.clone();
        let sel = selected_coord.clone();
        let qr = queue_redraw.clone();
        move || {
            let coord = *sel.borrow();
            if let Some((r, c)) = coord {
                let t = texts_nav.borrow();
                if r < t.len() && c < t[r].len() {
                    formula_e.set_text(&t[r][c]);
                }
            }
            qr();
        }
    };

    // Click to select and edit
    {
        let sel2 = selected_coord.clone();
        let fe2 = formula_entry.clone();
        let txt2 = texts.clone();
        let edit_e2 = editing_entry.clone();
        let commit_fn2 = commit_edit.clone();
        let start_fn2 = start_edit.clone();
        let fmts2 = cell_formats.clone();
        let rh3 = row_heights.clone();
        let qr = queue_redraw.clone();

        let click_logic: Rc<RefCell<Option<Box<dyn FnMut(f64, f64)>>>> = Rc::new(RefCell::new(None));
        {
            let sel = sel2.clone();
            let fe = fe2.clone();
            let txt = txt2.clone();
            let edit_e = edit_e2.clone();
            let commit_fn = commit_fn2.clone();
            let start_fn = start_fn2.clone();
            let fmts = fmts2.clone();
            let rh = rh3.clone();
            let qr2 = qr.clone();
            *click_logic.borrow_mut() = Some(Box::new(move |x: f64, y: f64| {
                let col_hdr_w = 46;
                let col = {
                    let cx = x - col_hdr_w as f64;
                    if cx <= 0.0 { 0 } else { ((cx / CELL_W as f64) as usize).min(VISIBLE_COLS - 1) }
                };
                let rh_b = rh.borrow();
                let grid_row = if y < CELL_H as f64 {
                    drop(rh_b);
                    0
                } else {
                    let data_y = y - CELL_H as f64;
                    let mut acc = 0.0f64;
                    let mut ri = 0;
                    for hi in rh_b.iter() {
                        if acc + *hi as f64 > data_y { break; }
                        acc += *hi as f64;
                        ri += 1;
                    }
                    drop(rh_b);
                    (ri + 1).min(VISIBLE_ROWS)
                };

                if grid_row == 0 && col < VISIBLE_COLS {
                    println!("Header clicked: col={}", col_to_label(col));
                    return;
                }
                if x < col_hdr_w as f64 && grid_row >= 1 && grid_row <= VISIBLE_ROWS {
                    println!("Row header clicked: row={}", grid_row);
                    return;
                }

                let data_row = grid_row.saturating_sub(1);
                if col < VISIBLE_COLS && data_row < VISIBLE_ROWS {
                    println!("Cell clicked: row={}, col={}", data_row + 1, col_to_label(col));
                    commit_fn();
                    if let Ok(mut cs) = sel.try_borrow_mut() {
                        *cs = Some((data_row, col));
                    }
                    let t = txt.borrow();
                    let fmt = fmts.borrow()[data_row][col].clone();
                    if data_row < t.len() && col < t[data_row].len() {
                        let text = &t[data_row][col];
                        if fmt.0 { fe.set_text(&format!("*{}*", text)); }
                        else if fmt.1 { fe.set_text(&format!("/{}/", text)); }
                        else { fe.set_text(text); }
                    }
                    drop(t);
                    qr2();
                    if edit_e.borrow().is_none() {
                        start_fn(data_row, col);
                        if let Some(e) = edit_e.borrow().as_ref() {
                            let text = txt.borrow()[data_row][col].clone();
                            e.set_text(&text);
                        }
                    }
                }
            }));
        }

        if is_gtk4 {
            if let Some(gesture_new) = loader.symbols.gtk_gesture_click_new {
                let gesture = unsafe { gesture_new() };
                if !gesture.is_null() {
                    if let Some(add_ctrl) = loader.symbols.gtk_widget_add_controller {
                        unsafe { add_ctrl(overlay_ptr, gesture); }
                    }
                    let cl = click_logic.clone();
                    let sw_ptr = scrolled_ptr;
                    let ld = loader.clone();
                    unsafe {
                        let _ = gtk_dynamic_loader::connect_signal_gesture(loader.symbols.as_ref(), gesture, "pressed", Box::new(move |_n: i32, x: f64, y: f64| {
                            let adj_value = |sym: Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> *mut std::ffi::c_void>, get_val: Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> f64>| -> f64 {
                                sym.and_then(|f| {
                                    let adj = unsafe { f(sw_ptr) };
                                    if adj.is_null() { None } else { get_val.map(|gv| unsafe { gv(adj) }) }
                                }).unwrap_or(0.0)
                            };
                            let scroll_x = adj_value(ld.symbols.gtk_scrolled_window_get_hadjustment, ld.symbols.gtk_adjustment_get_value);
                            let scroll_y = adj_value(ld.symbols.gtk_scrolled_window_get_vadjustment, ld.symbols.gtk_adjustment_get_value);
                            if let Some(f) = cl.borrow_mut().as_mut() { f(x + scroll_x, y + scroll_y); }
                        }));
                    }
                }
            }
        } else {
            let cl = click_logic.clone();
            let ov_ptr = overlay_ptr;
            unsafe {
                let _ = gtk_dynamic_loader::connect_signal_bool(loader.symbols.as_ref(), ov_ptr, "button-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                    type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;
                    let loader_tmp = match rustxwidgets::backends::gtk::loader() { Some(l) => l, None => return 0, };
                    let get_coords = loader_tmp.libs.get("libgdk").and_then(|gdk_lib| {
                        unsafe { (*gdk_lib).get::<GetEventCoords>(b"gdk_event_get_coords").ok().map(|s| *s) }
                    }).or_else(|| {
                        loader_tmp.libs.get("libgtk").and_then(|gtk_lib| {
                            unsafe { (*gtk_lib).get::<GetEventCoords>(b"gdk_event_get_coords").ok().map(|s| *s) }
                        })
                    });
                    if let Some(get_coords) = get_coords {
                        let mut x: f64 = 0.0;
                        let mut y: f64 = 0.0;
                        if get_coords(ev, &mut x as *mut f64, &mut y as *mut f64) != 0 {
                            if let Some(f) = cl.borrow_mut().as_mut() { f(x, y); }
                        }
                    }
                    0
                }));
            }
        }
    }

    // Formatting button handlers
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = bold_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].0 = !f[r][c].0;
                    qr();
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = italic_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].1 = !f[r][c].1;
                    qr();
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = al_l_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].2 = 0;
                    qr();
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = al_c_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].2 = 1;
                    qr();
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = al_r_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].2 = 2;
                    qr();
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = hl_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    let bg = &f[r][c].4;
                    f[r][c].4 = if bg == "#ffff00" { "#88ff88".into() }
                               else if bg == "#88ff88" { "#ffffff".into() }
                               else { "#ffff00".into() };
                    qr();
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let qr = queue_redraw.clone();
        let _ = fg_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    let fg = &f[r][c].3;
                    f[r][c].3 = if fg == "#000000" { "#cc0000".into() }
                               else if fg == "#cc0000" { "#0000cc".into() }
                               else if fg == "#0000cc" { "#006600".into() }
                               else { "#000000".into() };
                    qr();
                }
            }
        });
    }

    // Keyboard navigation + click-to-edit
    {
        let sel_coord = selected_coord.clone();
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let formula_e = formula_entry.clone();
        let commit_fn = commit_edit.clone();
        let refresh_sel = refresh_selection.clone();
        let loader_nav = loader.clone();

        if is_gtk4 {
            if let Some(ctrl_new) = loader.symbols.gtk_event_controller_key_new {
                let ctrl = unsafe { ctrl_new() };
                if !ctrl.is_null() {
                    if let Some(add_ctrl) = loader.symbols.gtk_widget_add_controller {
                        unsafe { add_ctrl(*win.as_ref(), ctrl); }
                    }
                    let key_handler = move |ev: *mut std::ffi::c_void| -> i32 {
                        let keyval = unsafe { if let Some(get_kv) = loader_nav.symbols.gdk_event_get_keyval { get_kv(ev) } else { 0 } };
                        if edit_entry_nav.borrow().is_some() {
                            if keyval == 0xFF1B {
                                let _ = edit_entry_nav.borrow_mut().take();
                            } else if keyval == 0xFF0D || keyval == 0xFF8D {
                                commit_fn();
                            }
                            return 0;
                        }
                        let mut coord = sel_coord.borrow_mut();
                        if let Some((r, c)) = *coord {
                            match keyval {
                                0xFF52 | 0xFE52 => { if r > 0 { *coord = Some((r - 1, c)); } }
                                0xFF54 | 0xFE54 => { if r + 1 < VISIBLE_ROWS { *coord = Some((r + 1, c)); } }
                                0xFF51 | 0xFE51 => { if c > 0 { *coord = Some((r, c - 1)); } }
                                0xFF53 | 0xFE53 => { if c + 1 < VISIBLE_COLS { *coord = Some((r, c + 1)); } }
                                0xFF0D | 0xFF8D => {
                                    drop(coord);
                                    commit_fn();
                                    start_edit(r, c);
                                    return 1;
                                }
                                _ => {
                                    if keyval >= 0x20 && keyval <= 0x7E {
                                        drop(coord);
                                        commit_fn();
                                        start_edit(r, c);
                                        if let Some(entry) = edit_entry_nav.borrow().as_ref() {
                                            let ch = std::char::from_u32(keyval).unwrap_or(' ');
                                            entry.set_text(&ch.to_string());
                                        }
                                        return 1;
                                    }
                                }
                            }
                        }
                        drop(coord);
                        refresh_sel();
                        0
                    };
                    unsafe {
                        let _ = gtk_dynamic_loader::connect_signal_bool(loader.symbols.as_ref(), ctrl, "key-pressed", Box::new(key_handler));
                    }
                }
            }
        } else {
            let syms_arc = loader.symbols.clone();
            let instance = *win.as_ref();
            let loader_gtk3 = loader_nav.clone();
            let start_for_gtk3 = start_edit.clone();
            let key_handler = move |ev: *mut std::ffi::c_void| -> i32 {
                let keyval = unsafe { if let Some(get_kv) = loader_gtk3.symbols.gdk_event_get_keyval { get_kv(ev) } else { 0 } };
                if edit_entry_nav.borrow().is_some() {
                    if keyval == 0xFF1B {
                        let _ = edit_entry_nav.borrow_mut().take();
                    } else if keyval == 0xFF0D || keyval == 0xFF8D {
                        commit_fn();
                    }
                    return 0;
                }
                let mut coord = sel_coord.borrow_mut();
                if let Some((r, c)) = *coord {
                    match keyval {
                        0xFF52 | 0xFE52 => { if r > 0 { *coord = Some((r - 1, c)); } }
                        0xFF54 | 0xFE54 => { if r + 1 < VISIBLE_ROWS { *coord = Some((r + 1, c)); } }
                        0xFF51 | 0xFE51 => { if c > 0 { *coord = Some((r, c - 1)); } }
                        0xFF53 | 0xFE53 => { if c + 1 < VISIBLE_COLS { *coord = Some((r, c + 1)); } }
                        0xFF0D | 0xFF8D => {
                            drop(coord);
                            commit_fn();
                            start_edit(r, c);
                            return 1;
                        }
                        _ => {
                            if keyval >= 0x20 && keyval <= 0x7E {
                                drop(coord);
                                commit_fn();
                                start_edit(r, c);
                                if let Some(entry) = edit_entry_nav.borrow().as_ref() {
                                    let ch = std::char::from_u32(keyval).unwrap_or(' ');
                                    entry.set_text(&ch.to_string());
                                }
                                return 1;
                            }
                        }
                    }
                }
                drop(coord);
                refresh_sel();
                0
            };
            unsafe {
                let _ = gtk_dynamic_loader::connect_signal_bool(syms_arc.as_ref(), instance, "key-press-event", Box::new(key_handler));
            }
        }
    }

    // File operations
    {
        let syms_arc = loader.symbols.clone();
        let texts_open = texts.clone();
        let qr = queue_redraw.clone();
        let _ = open_btn.on_click(move || {
            let syms = syms_arc.as_ref();
            if let (Some(chooser_new), Some(native_run), Some(get_fn), Some(destroy_widget_fn), Some(gfree)) = (
                syms.gtk_file_chooser_native_new, syms.gtk_native_dialog_run,
                syms.gtk_file_chooser_get_filename, syms.gtk_widget_destroy, syms.g_free,
            ) {
                unsafe {
                    let native = chooser_new("Open spreadsheet\0".as_ptr() as *const i8, std::ptr::null_mut(), 0, "Open\0".as_ptr() as *const i8, std::ptr::null::<i8>() as *const i8);
                    if !native.is_null() {
                        if native_run(native) == -3 {
                            let fname_ptr = get_fn(native);
                            if !fname_ptr.is_null() {
                                let fname = std::ffi::CStr::from_ptr(fname_ptr).to_string_lossy().into_owned();
                                gfree(fname_ptr as *mut std::ffi::c_void);
                                if let Ok(data) = std::fs::read_to_string(&fname) {
                                    if let Ok(mut t) = texts_open.try_borrow_mut() {
                                        for row in t.iter_mut() { for cell in row.iter_mut() { *cell = String::new(); } }
                                        for (i, line) in data.lines().enumerate() {
                                            if i >= VISIBLE_ROWS { break; }
                                            for (j, val) in line.split('\t').enumerate() {
                                                if j >= VISIBLE_COLS { break; }
                                                t[i][j] = val.to_string();
                                            }
                                        }
                                    }
                                    qr();
                                }
                            }
                        }
                        destroy_widget_fn(native);
                    }
                }
            }
        });
    }
    {
        let syms_save = loader.symbols.clone();
        let texts_save = texts.clone();
        let _ = save_btn.on_click(move || {
            let syms = syms_save.as_ref();
            if let (Some(chooser_new), Some(native_run), Some(get_fn), Some(destroy_widget_fn), Some(gfree)) = (
                syms.gtk_file_chooser_native_new, syms.gtk_native_dialog_run,
                syms.gtk_file_chooser_get_filename, syms.gtk_widget_destroy, syms.g_free,
            ) {
                unsafe {
                    let native = chooser_new("Save spreadsheet as\0".as_ptr() as *const i8, std::ptr::null_mut(), 1, "Save\0".as_ptr() as *const i8, std::ptr::null::<i8>() as *const i8);
                    if !native.is_null() {
                        if native_run(native) == -3 {
                            let fname_ptr = get_fn(native);
                            if !fname_ptr.is_null() {
                                let fname = std::ffi::CStr::from_ptr(fname_ptr).to_string_lossy().into_owned();
                                gfree(fname_ptr as *mut std::ffi::c_void);
                                let mut out = String::new();
                                if let Ok(t) = texts_save.try_borrow() {
                                    for row in t.iter() {
                                        for (j, val) in row.iter().enumerate() {
                                            if j > 0 { out.push('\t'); }
                                            out.push_str(val);
                                        }
                                        out.push('\n');
                                    }
                                }
                                let _ = std::fs::write(&fname, &out);
                            }
                        }
                        destroy_widget_fn(native);
                    }
                }
            }
        });
    }
    let _ = quit_btn.on_click(|| std::process::exit(0));

    // Layout
    let vbox = gtk::create_box(gtk::Orientation::Vertical, 0)?;
    let toolbar_box = gtk::create_box(gtk::Orientation::Vertical, 0)?;
    toolbar_box.append(&toolbar);
    let formula_and_grid = gtk::create_box(gtk::Orientation::Vertical, 2)?;
    formula_and_grid.append(&formula_hbox);

    struct ScrolledWindow(*mut std::ffi::c_void);
    impl AsRef<*mut std::ffi::c_void> for ScrolledWindow {
        fn as_ref(&self) -> &*mut std::ffi::c_void { &self.0 }
    }
    let sw = ScrolledWindow(scrolled_ptr);
    formula_and_grid.append(&sw);

    let fg_ptr = *formula_and_grid.as_ref();
    if let Some(set_vexpand) = lookup_sym::<unsafe extern "C" fn(*mut std::ffi::c_void, i32)>(&loader, "gtk_widget_set_vexpand") {
        unsafe { set_vexpand(fg_ptr, 1); }
    }
    let vbox_ptr = *vbox.as_ref();
    if let Some(set_vexpand) = lookup_sym::<unsafe extern "C" fn(*mut std::ffi::c_void, i32)>(&loader, "gtk_widget_set_vexpand") {
        unsafe { set_vexpand(vbox_ptr, 1); }
    }
    if let Some(set_hexpand) = lookup_sym::<unsafe extern "C" fn(*mut std::ffi::c_void, i32)>(&loader, "gtk_widget_set_hexpand") {
        unsafe { set_hexpand(vbox_ptr, 1); }
    }
    vbox.append(&toolbar_box);
    vbox.append(&formula_and_grid);
    win.set_child(&vbox);
    win.present();

    let result = app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>);

    // Cleanup: unparent drawing area from overlay to avoid double-free from Rust Drop
    *editing_entry.borrow_mut() = None;
    if let Some(unparent) = loader.symbols.gtk_widget_unparent {
        unsafe { unparent(draw_ptr_raw); }
    }

    result
}
