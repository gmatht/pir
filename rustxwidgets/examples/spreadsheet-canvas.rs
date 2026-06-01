use rustxwidgets::prelude::*;
use rustxwidgets::backends_gtk_adapter as gtk;
use std::rc::Rc;
use std::cell::RefCell;

const VISIBLE_ROWS: usize = 100;
const VISIBLE_COLS: usize = 26;
const CELL_W: i32 = 150;
const CELL_H: i32 = 28;

type CellFormat = (bool, bool, u8, String, String);

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
    editing: &RefCell<Option<gtk::Entry>>,
) {
    let cc = gtk_dynamic_loader::CairoContext::new(loader, cr);

    let chw = 46_f64;
    let cw = CELL_W as f64;
    let ch = CELL_H as f64;
    let total_w = chw + VISIBLE_COLS as f64 * cw;
    let total_h = ch + VISIBLE_ROWS as f64 * ch;

    cc.set_source_rgb(1.0, 1.0, 1.0);
    cc.rectangle(0.0, 0.0, total_w, total_h);
    cc.fill();

    cc.set_source_rgb(0.91, 0.91, 0.91);
    cc.rectangle(0.0, 0.0, chw, ch);
    cc.fill();

    cc.set_source_rgb(0.8, 0.8, 0.8);
    cc.rectangle(chw, 0.0, VISIBLE_COLS as f64 * cw, ch);
    cc.fill();
    cc.rectangle(0.0, ch, chw, VISIBLE_ROWS as f64 * ch);
    cc.fill();

    cc.set_source_rgb(0.7, 0.7, 0.7);
    cc.set_line_width(0.5);
    for c in 0..=VISIBLE_COLS {
        let x = chw + c as f64 * cw;
        cc.move_to(x, 0.0);
        cc.line_to(x, total_h);
        cc.stroke();
    }
    for r in 0..=VISIBLE_ROWS {
        let y = ch + r as f64 * ch;
        cc.move_to(0.0, y);
        cc.line_to(total_w, y);
        cc.stroke();
    }

    cc.select_font_face("monospace", 0, 1);
    cc.set_font_size(12.0);
    cc.set_source_rgb(0.0, 0.0, 0.0);
    for c in 0..VISIBLE_COLS {
        let lbl = col_to_label(c);
        let ext = cc.text_extents(&lbl);
        let x = chw + c as f64 * cw + cw / 2.0 - ext.x_bearing - ext.width / 2.0;
        let y = ch / 2.0 - ext.y_bearing - ext.height / 2.0;
        cc.move_to(x, y);
        cc.show_text(&lbl);
    }

    cc.set_font_size(12.0);
    for r in 0..VISIBLE_ROWS {
        let lbl = format!("{}", r + 1);
        let ext = cc.text_extents(&lbl);
        let x = chw / 2.0 - ext.x_bearing - ext.width / 2.0;
        let y = ch + r as f64 * ch + ch / 2.0 - ext.y_bearing - ext.height / 2.0;
        cc.move_to(x, y);
        cc.show_text(&lbl);
    }

    let t = texts.borrow();
    let f = fmts.borrow();
    let mut overflow_end_col = vec![vec![0usize; VISIBLE_COLS]; VISIBLE_ROWS];

    cc.set_font_size(13.0);
    for r in 0..VISIBLE_ROWS {
        for c in 0..VISIBLE_COLS {
            let text = &t[r][c];
            if text.is_empty() { continue; }
            let (bold, italic, _align, _fg_hex, _bg_hex) = &f[r][c];
            let slant = if *italic { 1 } else { 0 };
            let weight = if *bold { 1 } else { 0 };
            cc.select_font_face("monospace", slant, weight);
            let ext = cc.text_extents(text);
            let cx = chw + c as f64 * cw;
            let pad = 4.0;
            let tx = match *_align {
                0 => cx + pad - ext.x_bearing,
                1 => cx + cw / 2.0 - ext.x_bearing - ext.width / 2.0,
                _ => cx + cw - pad - ext.x_bearing - ext.width,
            };
            let text_right = tx + ext.x_bearing + ext.width;
            if text_right > cx + cw {
                let mut lo = c;
                for oc in (c + 1)..VISIBLE_COLS {
                    if !t[r][oc].is_empty() { break; }
                    lo = oc;
                }
                overflow_end_col[r][c] = lo;
            }
        }
    }

    cc.set_source_rgb(1.0, 1.0, 1.0);
    cc.rectangle(0.0, 0.0, total_w, total_h);
    cc.fill();

    cc.set_source_rgb(0.91, 0.91, 0.91);
    cc.rectangle(0.0, 0.0, chw, ch);
    cc.fill();

    cc.set_source_rgb(0.8, 0.8, 0.8);
    cc.rectangle(chw, 0.0, VISIBLE_COLS as f64 * cw, ch);
    cc.fill();
    cc.rectangle(0.0, ch, chw, VISIBLE_ROWS as f64 * ch);
    cc.fill();

    cc.set_source_rgb(0.7, 0.7, 0.7);
    cc.set_line_width(0.5);
    for r in 0..=VISIBLE_ROWS {
        let y = ch + r as f64 * ch;
        cc.move_to(0.0, y);
        cc.line_to(total_w, y);
        cc.stroke();
    }

    for bc in 0..=VISIBLE_COLS {
        let x = chw + bc as f64 * cw;
        cc.move_to(x, 0.0);
        cc.line_to(x, ch);
        cc.stroke();
        for r in 0..VISIBLE_ROWS {
            let skip = bc > 0 && overflow_end_col[r][bc - 1] >= bc;
            if !skip {
                let y1 = ch + r as f64 * ch;
                let y2 = ch + (r + 1) as f64 * ch;
                cc.move_to(x, y1);
                cc.line_to(x, y2);
                cc.stroke();
            }
        }
    }

    cc.save();
    let edit_target: Option<(usize, usize)> = if editing.borrow().is_some() {
        *sel.borrow()
    } else {
        None
    };

    cc.set_font_size(13.0);
    for r in 0..VISIBLE_ROWS {
        for c in 0..VISIBLE_COLS {
            let text = &t[r][c];
            if text.is_empty() { continue; }
            if Some((r, c)) == edit_target { continue; }
            let (bold, italic, align, fg_hex, bg_hex) = &f[r][c];

            let cx = chw + c as f64 * cw;
            let cy = ch + r as f64 * ch;

            if fg_hex != "#000000" {
                let (fr, fgg, fb) = parse_color(fg_hex);
                cc.set_source_rgb(fr, fgg, fb);
            } else {
                cc.set_source_rgb(0.0, 0.0, 0.0);
            }

            let slant = if *italic { 1 } else { 0 };
            let weight = if *bold { 1 } else { 0 };
            cc.select_font_face("monospace", slant, weight);

            let ext = cc.text_extents(text);

            let pad = 4.0;
            let tx = match *align {
                0 => cx + pad - ext.x_bearing,
                1 => cx + cw / 2.0 - ext.x_bearing - ext.width / 2.0,
                _ => cx + cw - pad - ext.x_bearing - ext.width,
            };
            let ty = cy + ch / 2.0 - ext.y_bearing - ext.height / 2.0;

            let last_oc = overflow_end_col[r][c];
            let text_right = tx + ext.x_bearing + ext.width + 2.0;
            let clip_right = if last_oc > c {
                let mut limit = text_right;
                for oc in (c + 1)..VISIBLE_COLS {
                    if !t[r][oc].is_empty() {
                        limit = limit.min(chw + oc as f64 * cw);
                        break;
                    }
                }
                limit.min(total_w)
            } else {
                cx + cw
            };

            if bg_hex != "#ffffff" || last_oc > c {
                let (br, bg, bb) = parse_color(bg_hex);
                cc.set_source_rgb(br, bg, bb);
                cc.rectangle(cx, cy + 1.0, clip_right - cx, ch - 2.0);
                cc.fill();
            }

            if fg_hex != "#000000" {
                let (fr, fgg, fb) = parse_color(fg_hex);
                cc.set_source_rgb(fr, fgg, fb);
            } else {
                cc.set_source_rgb(0.0, 0.0, 0.0);
            }

            cc.save();
            cc.rectangle(cx, cy, clip_right - cx, ch);
            cc.clip();
            cc.move_to(tx, ty);
            cc.show_text(text);
            cc.restore();
        }
    }
    cc.restore();

    cc.select_font_face("monospace", 0, 1);
    cc.set_font_size(12.0);
    cc.set_source_rgb(0.0, 0.0, 0.0);
    for c in 0..VISIBLE_COLS {
        let lbl = col_to_label(c);
        let ext = cc.text_extents(&lbl);
        let x = chw + c as f64 * cw + cw / 2.0 - ext.x_bearing - ext.width / 2.0;
        let y = ch / 2.0 - ext.y_bearing - ext.height / 2.0;
        cc.move_to(x, y);
        cc.show_text(&lbl);
    }

    cc.set_font_size(12.0);
    for r in 0..VISIBLE_ROWS {
        let lbl = format!("{}", r + 1);
        let ext = cc.text_extents(&lbl);
        let x = chw / 2.0 - ext.x_bearing - ext.width / 2.0;
        let y = ch + r as f64 * ch + ch / 2.0 - ext.y_bearing - ext.height / 2.0;
        cc.move_to(x, y);
        cc.show_text(&lbl);
    }

    if let Some((sr, sc)) = *sel.borrow() {
        let sx = chw + sc as f64 * cw;
        let sy = ch + sr as f64 * ch;
        cc.set_source_rgba(0.83, 0.91, 1.0, 0.3);
        cc.rectangle(sx, sy, cw, ch);
        cc.fill();
        cc.set_source_rgb(0.1, 0.45, 0.91);
        cc.set_line_width(2.0);
        cc.rectangle(sx + 0.5, sy + 0.5, cw - 1.0, ch - 1.0);
        cc.stroke();
    }
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

    let cell_formats: Rc<RefCell<Vec<Vec<CellFormat>>>> = Rc::new(RefCell::new(
        (0..VISIBLE_ROWS).map(|_| (0..VISIBLE_COLS).map(|_| (false, false, 0u8, "#000000".into(), "#ffffff".into())).collect()).collect()
    ));

    let col_widths: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(vec![CELL_W; VISIBLE_COLS]));
    let row_heights: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(vec![CELL_H; VISIBLE_ROWS]));

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

    let formula_hbox = gtk::create_box(gtk::Orientation::Horizontal, 4)?;
    let fx_label = app.create_label("  fx  ")?;
    formula_hbox.append(&fx_label);
    let formula_entry = gtk::create_entry()?;
    formula_entry.set_width_chars(40);
    formula_entry.set_size_request(400, 26);
    formula_hbox.append(&formula_entry);

    let drawing_area = gtk_dynamic_loader::DrawingArea::new(loader.clone())?;
    let drawing_area_ptr = *drawing_area.as_ref();
    let overlay = gtk_dynamic_loader::Overlay::new(loader.clone())?;
    overlay.set_child(&drawing_area);
    let overlay_ptr = *overlay.as_ref();

    let selected_coord: Rc<RefCell<Option<(usize, usize)>>> = Rc::new(RefCell::new(Some((0, 0))));
    let editing_entry: Rc<RefCell<Option<gtk::Entry>>> = Rc::new(RefCell::new(None));

    if is_gtk4 {
        let ld = loader.clone();
        let tx = texts.clone();
        let fm = cell_formats.clone();
        let sl = selected_coord.clone();
        let cw_draw = col_widths.clone();
        let rh_draw = row_heights.clone();
        let edit_draw = editing_entry.clone();
        drawing_area.set_draw_func(Box::new(move |cr, w, h| {
            draw_grid(cr, w, h, &ld, &tx, &fm, &sl, &cw_draw, &rh_draw, &edit_draw);
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
        let edit_draw = editing_entry.clone();
        drawing_area.connect_draw_gtk3(Box::new(move |_widget, cr| {
            draw_grid(cr, 0, 0, &ld, &tx, &fm, &sl, &cw, &rh, &edit_draw);
            0
        })).ok();
    }

    let scrolled = gtk_dynamic_loader::ScrolledWindow::new(loader.clone())?;
    scrolled.set_policy(0, 0);
    scrolled.set_child(&overlay);
    gtk_dynamic_loader::widget_set_hexpand(&loader, *scrolled.as_ref(), true);
    gtk_dynamic_loader::widget_set_vexpand(&loader, *scrolled.as_ref(), true);
    gtk_dynamic_loader::widget_set_hexpand(&loader, *overlay.as_ref(), true);
    gtk_dynamic_loader::widget_set_vexpand(&loader, *overlay.as_ref(), true);
    let total_w = 46 + VISIBLE_COLS as i32 * CELL_W;
    let total_h = CELL_H + VISIBLE_ROWS as i32 * CELL_H;
    gtk_dynamic_loader::widget_set_size_request(&loader, drawing_area_ptr, total_w, total_h);

    if let Some(loader2) = rustxwidgets::backends::gtk::loader() {
        let css = r#"
        button { font-size: 11px; padding: 1px 8px; min-height: 20px; }
        entry { padding: 0; border: none; font-family: monospace; font-size: 13px; min-height: 0; }
        entry:focus { outline: none; }
        "#;
        if let Some(provider) = gtk_dynamic_loader::create_css_provider(&loader2, css) {
            gtk_dynamic_loader::add_css_provider_global(&loader2, *win.as_ref(), provider, 600);
        }
    }

    let queue_redraw = Rc::new({
        let ld = loader.clone();
        move || gtk_dynamic_loader::widget_queue_draw(&ld, drawing_area_ptr)
    });

    let start_edit = {
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let overlay_for_edit = overlay.clone();
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
                gtk_dynamic_loader::widget_set_halign(&loader_for_edit, *entry.as_ref(), 1);
                gtk_dynamic_loader::widget_set_valign(&loader_for_edit, *entry.as_ref(), 1);
                overlay_for_edit.add_overlay(&entry);
                overlay_for_edit.set_overlay_pass_through(&entry, false);
                entry.set_size_request(CELL_W, rh_edit.borrow()[r]);
                entry.grab_focus();
                *edit_entry_nav.borrow_mut() = Some(entry);
            }
        }
    };

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
            if let Ok(gesture) = gtk_dynamic_loader::GestureClick::new(loader.clone()) {
                gesture.add_to_widget(&overlay);
                let cl = click_logic.clone();
                let sw = scrolled.clone();
                gesture.connect_pressed(move |_n: i32, x: f64, y: f64| {
                    let scroll_x = sw.get_hadjustment_value();
                    let scroll_y = sw.get_vadjustment_value();
                    if let Some(f) = cl.borrow_mut().as_mut() { f(x + scroll_x, y + scroll_y); }
                }).ok();
            }
        } else {
            let cl = click_logic.clone();
            let ld = loader.clone();
            gtk_dynamic_loader::widget_connect_signal_bool(&loader, overlay_ptr, "button-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                if let Some((x, y)) = gtk_dynamic_loader::gdk_event_get_coords(&ld, ev) {
                    if let Some(f) = cl.borrow_mut().as_mut() { f(x, y); }
                }
                0
            })).ok();
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
        let edit_entry_nav = editing_entry.clone();
        let commit_fn = commit_edit.clone();
        let refresh_sel = refresh_selection.clone();

        if is_gtk4 {
            if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(loader.clone()) {
                ctrl.add_to_widget(&win);
                let loader_ctrl = loader.clone();
                ctrl.connect_key_pressed(Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                    let keyval = gtk_dynamic_loader::EventControllerKey::get_keyval_static(&loader_ctrl, ev);
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
                })).ok();
            }
        } else {
            let win_ptr = *win.as_ref();
            let loader_kb = loader.clone();
            gtk_dynamic_loader::widget_connect_signal_bool(&loader, win_ptr, "key-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                let keyval = gtk_dynamic_loader::EventControllerKey::get_keyval_static(&loader_kb, ev);
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
            })).ok();
        }
    }

    // File operations
    {
        let loader_open = loader.clone();
        let texts_open = texts.clone();
        let qr = queue_redraw.clone();
        let _ = open_btn.on_click(move || {
            if let Ok(chooser) = gtk_dynamic_loader::FileChooserNative::open(loader_open.clone(), "Open spreadsheet", std::ptr::null_mut()) {
                if chooser.run() == -3 {
                    if let Some(fname) = chooser.get_filename() {
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
            }
        });
    }
    {
        let loader_save = loader.clone();
        let texts_save = texts.clone();
        let _ = save_btn.on_click(move || {
            if let Ok(chooser) = gtk_dynamic_loader::FileChooserNative::save(loader_save.clone(), "Save spreadsheet as", std::ptr::null_mut()) {
                if chooser.run() == -3 {
                    if let Some(fname) = chooser.get_filename() {
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
            }
        });
    }
    {
        let loader_save = loader.clone();
        let texts_save = texts.clone();
        let _ = save_btn.on_click(move || {
            if let Ok(chooser) = gtk_dynamic_loader::FileChooserNative::save(loader_save.clone(), "Save spreadsheet as", std::ptr::null_mut()) {
                if chooser.run() == -3 {
                    if let Some(fname) = chooser.get_filename() {
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
    formula_and_grid.append(&scrolled);
    let fg_ptr = *formula_and_grid.as_ref();

    gtk_dynamic_loader::widget_set_vexpand(&loader, fg_ptr, true);
    gtk_dynamic_loader::widget_set_vexpand(&loader, *vbox.as_ref(), true);
    gtk_dynamic_loader::widget_set_hexpand(&loader, *vbox.as_ref(), true);
    vbox.append(&toolbar_box);
    vbox.append(&formula_and_grid);
    win.set_child(&vbox);
    win.present();

    let result = app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>);

    // Cleanup: unparent editing entry from overlay
    if let Some(e) = editing_entry.borrow_mut().take() {
        gtk_dynamic_loader::widget_unparent(&loader, *e.as_ref());
    }

    result
}
