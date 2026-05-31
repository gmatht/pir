use rustxwidgets::prelude::*;
use rustxwidgets::backends_gtk_adapter as gtk;
use std::rc::Rc;
use std::cell::RefCell;

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
    if let Some(ref_sink) = loader.symbols.g_object_ref_sink { unsafe { ref_sink(overlay); } }
    else if let Some(gref) = loader.symbols.g_object_ref { unsafe { gref(overlay); } }
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

fn compute_col_x(widths: &[i32], col: usize) -> i32 {
    let mut x = 0;
    for i in 0..col {
        if i < widths.len() { x += widths[i]; }
    }
    x
}

fn apply_formatting(lbl: &gtk::Label, text: &str, fmt: &CellFormat) {
    lbl.set_text(text);
    lbl.remove_class("boolval");
    lbl.remove_class("numeric");
    lbl.remove_class("formula");
    lbl.remove_class("bold-cell");
    lbl.remove_class("italic-cell");
    let (bold, italic, align, fg, bg) = fmt;
    if *bold { lbl.add_class("bold-cell"); }
    if *italic { lbl.add_class("italic-cell"); }
    lbl.set_xalign(if *align == 1 { 0.5 } else if *align == 2 { 1.0 } else { 0.0 });
    if !fg.is_empty() && fg != "#000000" {
        lbl.set_markup(&format!("<span foreground=\"{}\">{}</span>", fg, text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")));
    } else {
        let upper = text.to_uppercase();
        if upper == "TRUE" || upper == "FALSE" {
            lbl.add_class("boolval");
            lbl.set_text(text);
        } else if text.starts_with('-') || text.parse::<f64>().is_ok() {
            lbl.add_class("numeric");
            lbl.set_text(text);
        } else if text.starts_with('=') {
            lbl.add_class("formula");
            lbl.set_text(text);
        } else {
            lbl.set_text(text);
        }
    }
    if !bg.is_empty() && bg != "#ffffff" {
        lbl.set_markup(&format!("<span background=\"{}\">{}</span>", bg,
            if fg.is_empty() || fg == "#000000" {
                text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
            } else {
                text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
            }));
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

    // Column widths (mutable for resize)
    let col_widths: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(vec![CELL_W; VISIBLE_COLS]));

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

    // Overlay
    let overlay_ptr = make_overlay(&loader).expect("failed to create overlay");
    let grid_widget = gtk::create_grid()?;
    set_overlay_child(&loader, overlay_ptr, *grid_widget.as_ref(), is_gtk4);

    // Corner
    let corner = app.create_label("")?;
    corner.add_class("header");
    grid_widget.attach(&corner, 0, 0, 1, 1);
    gtk_dynamic_loader::widget_set_size_request(&loader, *corner.as_ref(), 46, CELL_H);

    // Column headers
    let mut col_headers: Vec<gtk::Label> = Vec::new();
    for c in 0..VISIBLE_COLS {
        let lbl = col_to_label(c);
        let hdr = app.create_label(&lbl)?;
        hdr.set_text(&lbl);
        hdr.add_class("header");
        hdr.set_xalign(0.5);
        grid_widget.attach(&hdr, (c + 1) as i32, 0, 1, 1);
        let w = col_widths.borrow()[c];
        gtk_dynamic_loader::widget_set_size_request(&loader, *hdr.as_ref(), w, CELL_H);
        col_headers.push(hdr);
    }

    // Cells
    let static_cells: Rc<RefCell<Vec<Vec<gtk::Label>>>> = Rc::new(RefCell::new(Vec::new()));
    let selected_coord: Rc<RefCell<Option<(usize, usize)>>> = Rc::new(RefCell::new(Some((0, 0))));
    let editing_entry: Rc<RefCell<Option<gtk::Entry>>> = Rc::new(RefCell::new(None));

    for r in 0..VISIBLE_ROWS {
        let rm = app.create_label(&format!("{}", r + 1))?;
        rm.set_text(&format!("{}", r + 1));
        rm.add_class("row-marker");
        rm.add_class("header");
        rm.set_xalign(0.5);
        grid_widget.attach(&rm, 0, (r + 1) as i32, 1, 1);
        gtk_dynamic_loader::widget_set_size_request(&loader, *rm.as_ref(), 46, CELL_H);

        let mut row_labels = Vec::new();
        for c in 0..VISIBLE_COLS {
            let text = texts.borrow()[r][c].clone();
            let lbl = app.create_label(&text)?;
            let fmt = cell_formats.borrow()[r][c].clone();
            apply_formatting(&lbl, &text, &fmt);
            grid_widget.attach(&lbl, (c + 1) as i32, (r + 1) as i32, 1, 1);
            let w = col_widths.borrow()[c];
            gtk_dynamic_loader::widget_set_size_request(&loader, *lbl.as_ref(), w, CELL_H);
            row_labels.push(lbl);
        }
        static_cells.borrow_mut().push(row_labels);
    }

    if let Ok(sc) = static_cells.try_borrow() {
        if !sc.is_empty() && !sc[0].is_empty() {
            sc[0][0].add_class("selected");
        }
    }

    // Refresh function - rebuilds overflow overlay labels
    let overlay_labels: Rc<RefCell<Vec<gtk_dynamic_loader::Label>>> = Rc::new(RefCell::new(Vec::new()));
    let overlay_labels_ref = overlay_labels.clone();
    let texts_ref = texts.clone();
    let loader_for_refresh = loader.clone();
    let overlay_ptr_for_refresh = overlay_ptr;
    let cw_for_refresh = col_widths.clone();

    let refresh = Rc::new(move || {
        let drained = { let mut ol = overlay_labels_ref.borrow_mut(); ol.drain(..).collect::<Vec<_>>() };
        for lbl in drained.into_iter() {
            gtk_dynamic_loader::destroy_widget(&loader_for_refresh, *lbl.as_ref());
        }
        let texts_b = texts_ref.borrow();
        let cw = cw_for_refresh.borrow();
        for r in 0..texts_b.len() {
            for c in 0..texts_b[r].len() {
                let text = &texts_b[r][c];
                if text.is_empty() { continue; }
                let char_w = gtk_dynamic_loader::measure_text_px(&loader_for_refresh, None, text);
                let cell_w = cw.get(c).copied().unwrap_or(CELL_W) as f64;
                if char_w as f64 > cell_w - 8.0 {
                    if let Ok(lbl) = gtk_dynamic_loader::Label::new(loader_for_refresh.clone(), &text) {
                        lbl.add_class("rwx-overlay");
                        add_overlay_child(&loader_for_refresh, overlay_ptr_for_refresh, *lbl.as_ref());
                        set_overlay_pass_through(&loader_for_refresh, overlay_ptr_for_refresh, *lbl.as_ref(), true);
                        let total_w: i32 = (0..=c).map(|i| cw.get(i).copied().unwrap_or(CELL_W)).sum();
                        let left = 46 + total_w - cw.get(c).copied().unwrap_or(CELL_W);
                        let top = (r as i32 + 1) * CELL_H;
                        gtk_dynamic_loader::widget_set_size_request(&loader_for_refresh, *lbl.as_ref(), (char_w as i32 + 8).min(total_w), CELL_H);
                        gtk_dynamic_loader::widget_set_margin_start(&loader_for_refresh, *lbl.as_ref(), left);
                        gtk_dynamic_loader::widget_set_margin_top(&loader_for_refresh, *lbl.as_ref(), top);
                        overlay_labels_ref.borrow_mut().push(lbl);
                    }
                }
            }
        }
    });
    refresh();

    // CSS
    if let Some(loader2) = rustxwidgets::backends::gtk::loader() {
        let css = r#"
        label.rwx-overlay { background-color: transparent; padding: 2px 4px; font-family: monospace; font-size: 13px; }
        label.header { font-weight: bold; background-color: #ffffff; color: #000000; font-size: 12px; border: 1px solid #000000; padding: 2px 4px; }
        label.row-marker { color: #000000; background-color: #f8f8f8; border-right: 1px solid #000000; font-weight: bold; }
        label.cell { padding-left: 4px; padding-right: 4px; font-family: monospace; font-size: 13px; background-color: #ffffff; border-right: 1px solid #000000; border-bottom: 1px solid #000000; }
        label.boolval { font-weight: bold; color: #0000cc; }
        label.negative { color: #cc0000; }
        label.numeric { font-family: monospace; }
        label.formula { color: #006600; font-style: italic; }
        label.selected { background-color: #d4e8ff !important; border: 2px solid #1a73e8 !important; }
        label.bold-cell { font-weight: bold; }
        label.italic-cell { font-style: italic; }
        button { font-size: 11px; padding: 1px 8px; min-height: 20px; }
        entry { border: 1px solid #000000; font-family: monospace; font-size: 13px; min-height: 24px; }
        grid { border: 1px solid #000000; }
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
            if let Some(ref_sink) = loader.symbols.g_object_ref_sink { ref_sink(sw); }
            else if let Some(gref) = loader.symbols.g_object_ref { gref(sw); }
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
            let total_w: i32 = 46 + col_widths.borrow().iter().sum::<i32>();
            let total_h = CELL_H + (VISIBLE_ROWS as i32) * CELL_H;
            gtk_dynamic_loader::widget_set_size_request(&loader, *grid_widget.as_ref(), total_w, total_h);
            sw
        } else { panic!("gtk_scrolled_window_new not available"); }
    };

    // Helper to start editing a cell
    let start_edit = {
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let overlay_for_edit = overlay_ptr;
        let loader_for_edit = loader.clone();
        let cw_edit = col_widths.clone();
        move |r: usize, c: usize| {
            if edit_entry_nav.borrow().is_some() { return; }
            if let Ok(entry) = gtk::create_entry() {
                entry.set_text(&texts_nav.borrow()[r][c]);
                entry.set_width_chars(12);
                let cw = cw_edit.borrow();
                let left = 46 + compute_col_x(&cw, c);
                let top = (r as i32 + 1) * CELL_H;
                drop(cw);
                gtk_dynamic_loader::widget_set_margin_start(&loader_for_edit, *entry.as_ref(), left);
                gtk_dynamic_loader::widget_set_margin_top(&loader_for_edit, *entry.as_ref(), top);
                add_overlay_child(&loader_for_edit, overlay_for_edit, *entry.as_ref());
                set_overlay_pass_through(&loader_for_edit, overlay_for_edit, *entry.as_ref(), false);
                entry.set_size_request(cw_edit.borrow()[c], CELL_H);
                entry.grab_focus();
                *edit_entry_nav.borrow_mut() = Some(entry);
            }
        }
    };

    // Commit editing entry
    let commit_edit = {
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let sc_nav = static_cells.clone();
        let formula_e = formula_entry.clone();
        let refresh_nav = refresh.clone();
        let fmts_nav = cell_formats.clone();
        let loader_nav = loader.clone();
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
                    if let Ok(sc) = sc_nav.try_borrow() {
                        if r < sc.len() && c < sc[r].len() {
                            let fmt = fmts_nav.borrow()[r][c].clone();
                            apply_formatting(&sc[r][c], &new_text, &fmt);
                        }
                    }
                }
                formula_e.set_text(&new_text);
                gtk_dynamic_loader::destroy_widget(&loader_nav, *e.as_ref());
                refresh_nav();
            }
        }
    };

    // Refresh cell display when selection moves (update display classes etc.)
    let refresh_selection = {
        let texts_nav = texts.clone();
        let sc_nav = static_cells.clone();
        let formula_e = formula_entry.clone();
        let sel = selected_coord.clone();
        let fmts_nav = cell_formats.clone();
        move || {
            let coord = *sel.borrow();
            if let Some((r, c)) = coord {
                if let Ok(sc) = sc_nav.try_borrow() {
                    for rr in 0..sc.len() {
                        for cc in 0..sc[rr].len() { sc[rr][cc].remove_class("selected"); }
                    }
                    if r < sc.len() && c < sc[r].len() { sc[r][c].add_class("selected"); }
                }
                let t = texts_nav.borrow();
                if r < t.len() && c < t[r].len() {
                    formula_e.set_text(&t[r][c]);
                }
            }
        }
    };

    // Click to select and edit via grid button-press-event
    {
        let grid_ptr = *grid_widget.as_ref();
        let sc = static_cells.clone();
        let sel = selected_coord.clone();
        let fe = formula_entry.clone();
        let txt = texts.clone();
        let syms = loader.symbols.clone();
        let edit_e = editing_entry.clone();
        let commit_fn = commit_edit.clone();
        let start_fn = start_edit.clone();
        let fmts = cell_formats.clone();
        let cw = col_widths.clone();
        let _ = unsafe {
            gtk_dynamic_loader::connect_signal_bool(syms.as_ref(), grid_ptr, "button-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;
                let loader_tmp = match rustxwidgets::backends::gtk::loader() { Some(l) => l, None => return 0, };
                let gtk_lib = match loader_tmp.libs.get("libgtk") { Some(l) => l.clone(), None => return 0, };
                let lib = &*gtk_lib;
                if let Ok(get_coords) = lib.get::<GetEventCoords>(b"gdk_event_get_coords") {
                    let mut x: f64 = 0.0;
                    let mut y: f64 = 0.0;
                    if get_coords(ev, &mut x as *mut f64, &mut y as *mut f64) != 0 {
                        let col_hdr_w = 46;
                        let col_widths_b = cw.borrow();
                        let col = {
                            let mut cx = x - col_hdr_w as f64;
                            let mut ci = 0;
                            while ci < VISIBLE_COLS && cx > 0.0 {
                                cx -= col_widths_b[ci] as f64;
                                ci += 1;
                            }
                            ci.saturating_sub(1).min(VISIBLE_COLS - 1)
                        };
                        drop(col_widths_b);
                        let row = ((y - CELL_H as f64) / CELL_H as f64).floor() as usize;

                        if row == 0 && col < VISIBLE_COLS {
                            // Header click: log to console
                            println!("Header clicked: col={}", col_to_label(col));
                            return 1;
                        }
                        if col == 0 && row >= 1 && row <= VISIBLE_ROWS {
                            // Row marker click: log to console
                            println!("Row header clicked: row={}", row);
                            return 1;
                        }

                        if col < VISIBLE_COLS && row < VISIBLE_ROWS {
                            // Commit any ongoing edit first
                            commit_fn();
                            // Select cell
                            if let Ok(sc_b) = sc.try_borrow() {
                                for rr in 0..sc_b.len() {
                                    for cc in 0..sc_b[rr].len() {
                                        sc_b[rr][cc].remove_class("selected");
                                    }
                                }
                                if row < sc_b.len() && col < sc_b[row].len() {
                                    sc_b[row][col].add_class("selected");
                                }
                            }
                            if let Ok(mut cs) = sel.try_borrow_mut() {
                                *cs = Some((row, col));
                            }
                            let t = txt.borrow();
                            let fmt = fmts.borrow()[row][col].clone();
                            if row < t.len() && col < t[row].len() {
                                let text = &t[row][col];
                                // Update formula bar with formatted text
                                if fmt.0 { fe.set_text(&format!("*{}*", text)); }
                                else if fmt.1 { fe.set_text(&format!("/{}/", text)); }
                                else { fe.set_text(text); }
                            }
                            // Start editing the cell on click
                            drop(t);
                            // Position entry
                            if edit_e.borrow().is_none() {
                                start_fn(row, col);
                                // Put the clicked position into the new entry
                                if let Some(e) = edit_e.borrow().as_ref() {
                                    let text = txt.borrow()[row][col].clone();
                                    e.set_text(&text);
                                }
                            }
                        }
                    }
                }
                0
            }))
        };
    }

    // Formatting button handlers
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = bold_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].0 = !f[r][c].0;
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = italic_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].1 = !f[r][c].1;
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = al_l_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].2 = 0;
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = al_c_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].2 = 1;
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = al_r_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    f[r][c].2 = 2;
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }
    // Highlight toggle (cycles through: none -> yellow -> green -> none)
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = hl_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    let bg = &f[r][c].4;
                    f[r][c].4 = if bg == "#ffff00" { "#88ff88".into() }
                               else if bg == "#88ff88" { "#ffffff".into() }
                               else { "#ffff00".into() };
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }
    // Text color toggle (cycles through: black -> red -> blue -> green -> black)
    {
        let sel = selected_coord.clone();
        let fmts = cell_formats.clone();
        let sc = static_cells.clone();
        let txt = texts.clone();
        let _ = fg_btn.on_click(move || {
            if let Some((r, c)) = *sel.borrow() {
                let mut f = fmts.borrow_mut();
                if r < f.len() && c < f[r].len() {
                    let fg = &f[r][c].3;
                    f[r][c].3 = if fg == "#000000" { "#cc0000".into() }
                               else if fg == "#cc0000" { "#0000cc".into() }
                               else if fg == "#0000cc" { "#006600".into() }
                               else { "#000000".into() };
                    if let Ok(sc_b) = sc.try_borrow() {
                        if r < sc_b.len() && c < sc_b[r].len() {
                            let fmt = f[r][c].clone();
                            apply_formatting(&sc_b[r][c], &txt.borrow()[r][c], &fmt);
                        }
                    }
                }
            }
        });
    }

    // Column resize state
    let resize_col: Rc<RefCell<Option<usize>>> = Rc::new(RefCell::new(None));
    let resize_start_x: Rc<RefCell<i32>> = Rc::new(RefCell::new(0));
    let resize_start_w: Rc<RefCell<i32>> = Rc::new(RefCell::new(0));

    // Keyboard navigation + click-to-edit + resize
    {
        let grid_ptr = *grid_widget.as_ref();
        let sc_nav = static_cells.clone();
        let sel_coord = selected_coord.clone();
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let formula_e = formula_entry.clone();
        let refresh_nav = refresh.clone();
        let commit_fn = commit_edit.clone();
        let refresh_sel = refresh_selection.clone();
        let rs_nav = resize_col.clone();
        let rsx_nav = resize_start_x.clone();
        let rsw_nav = resize_start_w.clone();
        let cw_nav = col_widths.clone();
        let loader_nav = loader.clone();
        let loader_for_resize = loader_nav.clone();

        // Keyboard navigation
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
                                let entry = edit_entry_nav.borrow_mut().take();
                                if let Some(e) = entry { gtk_dynamic_loader::destroy_widget(&loader_nav, *e.as_ref()); }
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
                        let entry = edit_entry_nav.borrow_mut().take();
                        if let Some(e) = entry { gtk_dynamic_loader::destroy_widget(&loader_gtk3, *e.as_ref()); }
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

        // Column resize
        {
            let cw_r = cw_nav.clone();
            let rs_r = rs_nav.clone();
            let rsx_r = rsx_nav.clone();
            let rsw_r = rsw_nav.clone();
            let ch_r = col_headers.clone();
            let sc_r = sc_nav.clone();
            let ref_r = refresh_nav.clone();
            let ld_r = loader_for_resize.clone();
            let btn_syms = loader.symbols.clone();
            let _ = unsafe {
                gtk_dynamic_loader::connect_signal_bool(btn_syms.as_ref(), grid_ptr, "button-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                    type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;
                    let loader_tmp = match rustxwidgets::backends::gtk::loader() { Some(l) => l, None => return 0, };
                    let gtk_lib = match loader_tmp.libs.get("libgtk") { Some(l) => l.clone(), None => return 0, };
                    let lib = &*gtk_lib;
                    if let Ok(get_coords) = lib.get::<GetEventCoords>(b"gdk_event_get_coords") {
                        let mut x: f64 = 0.0;
                        let mut y: f64 = 0.0;
                        if get_coords(ev, &mut x as *mut f64, &mut y as *mut f64) != 0 {
                            if y <= CELL_H as f64 {
                                let cw_b = cw_r.borrow();
                                let mut edge_x = 46.0f64;
                                for ci in 0..VISIBLE_COLS {
                                    let w = cw_b[ci] as f64;
                                    if (x - (edge_x + w)).abs() <= 6.0 && x >= edge_x + w - 6.0 {
                                        *rs_r.borrow_mut() = Some(ci);
                                        *rsx_r.borrow_mut() = x as i32;
                                        *rsw_r.borrow_mut() = cw_b[ci];
                                        return 1;
                                    }
                                    edge_x += w;
                                }
                            }
                        }
                    }
                    0
                }))
            };

            // Motion handler for resize
            let motion_syms = loader.symbols.clone();
            if is_gtk4 {
                type MotionNew = unsafe extern "C" fn() -> *mut std::ffi::c_void;
                type AddCtrlFn = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);
                if let Some(motion_new) = lookup_sym::<MotionNew>(&loader, "gtk_event_controller_motion_new") {
                    let ctrl = unsafe { motion_new() };
                    if !ctrl.is_null() {
                        if let Some(add_ctrl) = loader.symbols.gtk_widget_add_controller {
                            unsafe { add_ctrl(grid_ptr, ctrl); }
                        }
                        let rs_m = rs_nav.clone();
                        let rsx_m = rsx_nav.clone();
                        let rsw_m = rsw_nav.clone();
                        let cw_m = cw_nav.clone();
                        let ch_m = col_headers.clone();
                        let sc_m = sc_nav.clone();
                        let ld_m = loader_for_resize.clone();
                        let ref_m = refresh_nav.clone();
                        let _ = unsafe {
                            gtk_dynamic_loader::connect_signal_bool(motion_syms.as_ref(), ctrl, "motion", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                                if rs_m.borrow().is_none() { return 0; }
                                type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;
                                let loader_tmp = match rustxwidgets::backends::gtk::loader() { Some(l) => l, None => return 0, };
                                let gtk_lib = match loader_tmp.libs.get("libgtk") { Some(l) => l.clone(), None => return 0, };
                                let lib = &*gtk_lib;
                                if let Ok(get_coords) = lib.get::<GetEventCoords>(b"gdk_event_get_coords") {
                                    let mut x: f64 = 0.0;
                                    let mut y: f64 = 0.0;
                                    if get_coords(ev, &mut x as *mut f64, &mut y as *mut f64) != 0 {
                                        if let Some(col) = *rs_m.borrow() {
                                            let delta = (x as i32) - *rsx_m.borrow();
                                            let new_w = (*rsw_m.borrow() + delta).max(20);
                                            cw_m.borrow_mut()[col] = new_w;
                                            if col < ch_m.len() {
                                                gtk_dynamic_loader::widget_set_size_request(&ld_m, *ch_m[col].as_ref(), new_w, CELL_H);
                                            }
                                            if let Ok(sc) = sc_m.try_borrow() {
                                                for row in sc.iter() {
                                                    if col < row.len() {
                                                        gtk_dynamic_loader::widget_set_size_request(&ld_m, *row[col].as_ref(), new_w, CELL_H);
                                                    }
                                                }
                                            }
                                            ref_m();
                                        }
                                    }
                                }
                                0
                            }))
                        };
                    }
                }
            } else {
                let rs_m = rs_nav.clone();
                let rsx_m = rsx_nav.clone();
                let rsw_m = rsw_nav.clone();
                let cw_m = cw_nav.clone();
                let ch_m = col_headers.clone();
                let sc_m = sc_nav.clone();
                let ld_m = loader_for_resize.clone();
                let ref_m = refresh_nav.clone();
                let _ = unsafe {
                    gtk_dynamic_loader::connect_signal_bool(motion_syms.as_ref(), grid_ptr, "motion-notify-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                        if rs_m.borrow().is_none() { return 0; }
                        type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;
                        let loader_tmp = match rustxwidgets::backends::gtk::loader() { Some(l) => l, None => return 0, };
                        let gtk_lib = match loader_tmp.libs.get("libgtk") { Some(l) => l.clone(), None => return 0, };
                        let lib = &*gtk_lib;
                        if let Ok(get_coords) = lib.get::<GetEventCoords>(b"gdk_event_get_coords") {
                            let mut x: f64 = 0.0;
                            let mut y: f64 = 0.0;
                            if get_coords(ev, &mut x as *mut f64, &mut y as *mut f64) != 0 {
                                if let Some(col) = *rs_m.borrow() {
                                    let delta = (x as i32) - *rsx_m.borrow();
                                    let new_w = (*rsw_m.borrow() + delta).max(20);
                                    cw_m.borrow_mut()[col] = new_w;
                                    if col < ch_m.len() {
                                        gtk_dynamic_loader::widget_set_size_request(&ld_m, *ch_m[col].as_ref(), new_w, CELL_H);
                                    }
                                    if let Ok(sc) = sc_m.try_borrow() {
                                        for row in sc.iter() {
                                            if col < row.len() {
                                                gtk_dynamic_loader::widget_set_size_request(&ld_m, *row[col].as_ref(), new_w, CELL_H);
                                            }
                                        }
                                    }
                                    ref_m();
                                }
                            }
                        }
                        0
                    }))
                };
                let rs_end = rs_nav.clone();
                let _ = unsafe {
                    gtk_dynamic_loader::connect_signal_bool(motion_syms.as_ref(), grid_ptr, "button-release-event", Box::new(move |_ev: *mut std::ffi::c_void| -> i32 {
                        *rs_end.borrow_mut() = None;
                        0
                    }))
                };
            }

            if is_gtk4 {
                let rs_end = rs_nav.clone();
                let btn_rel_syms = loader.symbols.clone();
                unsafe {
                    let _ = gtk_dynamic_loader::connect_signal_bool(btn_rel_syms.as_ref(), *win.as_ref(), "button-release-event", Box::new(move |_ev: *mut std::ffi::c_void| -> i32 {
                        *rs_end.borrow_mut() = None;
                        0
                    }));
                }
            }
        }
    }

    // File operations
    {
        let syms_arc = loader.symbols.clone();
        let texts_open = texts.clone();
        let sc_open = static_cells.clone();
        let refresh_open = refresh.clone();
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
                                    if let Ok(sc) = sc_open.try_borrow() {
                                        for r in 0..sc.len() {
                                            for c in 0..sc[r].len() {
                                                let text = texts_open.borrow()[r][c].clone();
                                                apply_formatting(&sc[r][c], &text, &(false, false, 0, "#000000".into(), "#ffffff".into()));
                                            }
                                        }
                                    }
                                    refresh_open();
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
    if let Some(set_hexpand) = lookup_sym::<unsafe extern "C" fn(*mut std::ffi::c_void, i32)>(&loader, "gtk_widget_set_hexpand") {
        unsafe { set_hexpand(vbox_ptr, 1); }
    }
    vbox.append(&toolbar_box);
    vbox.append(&formula_and_grid);
    win.set_child(&vbox);
    win.present();

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
