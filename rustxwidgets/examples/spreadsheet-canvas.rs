use rustxwidgets::prelude::*;
use rustxwidgets::backends_gtk_adapter as gtk;
use std::rc::Rc;
use std::cell::RefCell;

const VISIBLE_ROWS: usize = 100;
const VISIBLE_COLS: usize = 26;
const CELL_W: i32 = 100;
const CELL_H: i32 = 28;

fn col_to_label(n: usize) -> String {
    if n < 26 {
        format!("{}", (b'A' + (n as u8)) as char)
    } else {
        let mut s = String::new();
        let mut v = n;
        loop {
            s.insert(0, (b'A' + (v % 26) as u8) as char);
            v = v / 26;
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

fn compute_spans_from_texts(texts: &[Vec<String>], loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, per_cell_px: i32) -> Vec<(usize, usize, usize, String)> {
    let rows: Vec<Vec<(usize, String)>> = texts.iter().enumerate().map(|(r, row)| {
        row.iter().enumerate().map(|(c, t)| {
            ((r << 16) ^ c, t.clone())
        }).collect()
    }).collect();
    rustxwidgets::overflow::compute_spans_from_model(&rows, per_cell_px, |_k, s| {
        gtk_dynamic_loader::measure_text_px(loader, None, s)
    })
}

fn style_cell_label(lbl: &gtk::Label, text: &str) {
    lbl.set_text(text);
    lbl.remove_class("boolval");
    lbl.remove_class("numeric");
    lbl.remove_class("formula");
    lbl.set_xalign(0.0);
    let upper = text.to_uppercase();
    if upper == "TRUE" || upper == "FALSE" {
        lbl.add_class("boolval");
    } else if text.starts_with('-') || text.parse::<f64>().is_ok() {
        lbl.add_class("numeric");
        lbl.set_xalign(1.0);
    }
    if text.starts_with('=') {
        lbl.add_class("formula");
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
        let row = vec![String::new(); VISIBLE_COLS];
        texts.borrow_mut().push(row);
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

    // Toolbar
    let toolbar = gtk::create_box(gtk::Orientation::Horizontal, 2)?;
    let open_btn = app.create_button("Open")?;
    let save_btn = app.create_button("Save As")?;
    let quit_btn = app.create_button("Quit")?;
    // Fix button sizes so they don't stretch when window resizes in GTK3
    gtk_dynamic_loader::widget_set_size_request(&loader, *open_btn.as_ref(), 60, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *save_btn.as_ref(), 70, 24);
    gtk_dynamic_loader::widget_set_size_request(&loader, *quit_btn.as_ref(), 50, 24);
    toolbar.append(&open_btn);
    toolbar.append(&save_btn);
    toolbar.append(&quit_btn);

    // Formula bar
    let formula_hbox = gtk::create_box(gtk::Orientation::Horizontal, 4)?;
    let fx_label = app.create_label("  fx  ")?;
    formula_hbox.append(&fx_label);
    let formula_entry = gtk::create_entry()?;
    formula_entry.set_width_chars(40);
    formula_entry.set_size_request(400, 26);
    formula_hbox.append(&formula_entry);

    // Create overlay manually for both GTK3/GTK4
    let overlay_ptr = make_overlay(&loader).expect("failed to create overlay");
    let grid_widget = gtk::create_grid()?;
    set_overlay_child(&loader, overlay_ptr, *grid_widget.as_ref(), is_gtk4);

    // Corner
    let corner = app.create_label("")?;
    corner.add_class("header");
    grid_widget.attach(&corner, 0, 0, 1, 1);
    gtk_dynamic_loader::widget_set_size_request(&loader, *corner.as_ref(), 46, CELL_H);

    // Column headers
    for c in 0..VISIBLE_COLS {
        let lbl = col_to_label(c);
        let hdr = app.create_label(&lbl)?;
        hdr.set_text(&lbl);
        hdr.add_class("header");
        hdr.set_xalign(0.5);
        grid_widget.attach(&hdr, (c + 1) as i32, 0, 1, 1);
        gtk_dynamic_loader::widget_set_size_request(&loader, *hdr.as_ref(), CELL_W, CELL_H);
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
            style_cell_label(&lbl, &text);
            grid_widget.attach(&lbl, (c + 1) as i32, (r + 1) as i32, 1, 1);
            gtk_dynamic_loader::widget_set_size_request(&loader, *lbl.as_ref(), CELL_W, CELL_H);
            row_labels.push(lbl);
        }
        static_cells.borrow_mut().push(row_labels);
    }

    if let Ok(sc) = static_cells.try_borrow() {
        if !sc.is_empty() && !sc[0].is_empty() {
            sc[0][0].add_class("selected");
        }
    }

    // Click to select via grid button-press-event
    {
        let grid_ptr = *grid_widget.as_ref();
        let sc = static_cells.clone();
        let sel = selected_coord.clone();
        let fe = formula_entry.clone();
        let txt = texts.clone();
        let syms = loader.symbols.clone();
        let _ = unsafe {
            gtk_dynamic_loader::connect_signal_bool(syms.as_ref(), grid_ptr, "button-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;
                let loader_tmp = match rustxwidgets::backends::gtk::loader() {
                    Some(l) => l,
                    None => return 0,
                };
                let gtk_lib = match loader_tmp.libs.get("libgtk") {
                    Some(l) => l.clone(),
                    None => return 0,
                };
                let lib = &*gtk_lib;
                if let Ok(get_coords) = lib.get::<GetEventCoords>(b"gdk_event_get_coords") {
                    let mut x: f64 = 0.0;
                    let mut y: f64 = 0.0;
                    if get_coords(ev, &mut x as *mut f64, &mut y as *mut f64) != 0 {
                        let col_hdr_w = 46;
                        let col = ((x - col_hdr_w as f64) / CELL_W as f64).floor() as usize;
                        let row = ((y - CELL_H as f64) / CELL_H as f64).floor() as usize;
                        if col < VISIBLE_COLS && row < VISIBLE_ROWS {
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
                            fe.set_text(&txt.borrow()[row][col]);
                        }
                    }
                }
                0
            }))
        };
    }

    // Overlay labels
    let overlay_labels: Rc<RefCell<Vec<gtk_dynamic_loader::Label>>> = Rc::new(RefCell::new(Vec::new()));
    let overlay_labels_ref = overlay_labels.clone();
    let texts_ref = texts.clone();
    let loader_for_refresh = loader.clone();
    let overlay_ptr_for_refresh = overlay_ptr;

    let refresh = Rc::new(move || {
        let drained = { let mut ol = overlay_labels_ref.borrow_mut(); ol.drain(..).collect::<Vec<_>>() };
        for lbl in drained.into_iter() {
            gtk_dynamic_loader::destroy_widget(&loader_for_refresh, *lbl.as_ref());
        }
        let spans = compute_spans_from_texts(&texts_ref.borrow(), &loader_for_refresh, CELL_W);
        for (r, start_col, len, text) in spans.into_iter() {
            if let Ok(lbl) = gtk_dynamic_loader::Label::new(loader_for_refresh.clone(), &text) {
                lbl.add_class("rwx-overlay");
                add_overlay_child(&loader_for_refresh, overlay_ptr_for_refresh, *lbl.as_ref());
                set_overlay_pass_through(&loader_for_refresh, overlay_ptr_for_refresh, *lbl.as_ref(), true);
                let left = (start_col as i32) * CELL_W + 46;
                let top = (r as i32 + 1) * CELL_H;
                gtk_dynamic_loader::widget_set_size_request(&loader_for_refresh, *lbl.as_ref(), (len as i32) * CELL_W, CELL_H);
                gtk_dynamic_loader::widget_set_margin_start(&loader_for_refresh, *lbl.as_ref(), left);
                gtk_dynamic_loader::widget_set_margin_top(&loader_for_refresh, *lbl.as_ref(), top);
                overlay_labels_ref.borrow_mut().push(lbl);
            }
        }
    });

    refresh();

    // CSS
    if let Some(loader2) = rustxwidgets::backends::gtk::loader() {
        let css = r#"
        label.rwx-overlay { background-color: transparent; padding: 2px 4px; font-family: monospace; font-size: 13px; }
        label.header { font-weight: bold; background-color: #f0f0f0; font-size: 12px; border: 1px solid #d0d0d0; padding: 2px; }
        label.row-marker { color: #666666; background-color: #f8f8f8; border-right: 1px solid #d0d0d0; }
        label.cell { padding-left: 4px; padding-right: 4px; font-family: monospace; font-size: 13px; background-color: #ffffff; border-right: 1px solid #e0e0e0; border-bottom: 1px solid #e0e0e0; }
        label.boolval { font-weight: bold; color: #0000cc; }
        label.negative { color: #cc0000; }
        label.numeric { font-family: monospace; }
        label.formula { color: #006600; font-style: italic; }
        label.selected { background-color: #d4e8ff !important; border: 2px solid #1a73e8 !important; }
        button { font-size: 11px; padding: 1px 8px; min-height: 20px; }
        entry { border: 1px solid #c0c0c0; font-family: monospace; font-size: 13px; min-height: 24px; }
        grid { border: 1px solid #c0c0c0; }
        "#;
        if let Some(provider) = gtk_dynamic_loader::create_css_provider(&loader2, css) {
            gtk_dynamic_loader::add_provider_to_widget(&loader2, *win.as_ref(), provider, 600);
        }
    }

    // ScrolledWindow wrapping the overlay
    let scrolled_ptr: *mut std::ffi::c_void = unsafe {
        type ScrolledNew = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        type SetPolicy = unsafe extern "C" fn(*mut std::ffi::c_void, u32, u32);
        type SetChild = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);
        type SetExpand = unsafe extern "C" fn(*mut std::ffi::c_void, i32);

        match lookup_sym::<ScrolledNew>(&loader, "gtk_scrolled_window_new") {
            Some(scrolled_new) => {
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
                // Make scrolled window expand to fill available space
                if let Some(set_h) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_hexpand") {
                    set_h(sw, 1);
                }
                if let Some(set_v) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_vexpand") {
                    set_v(sw, 1);
                }
                // Also expand the overlay
                if let Some(set_h) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_hexpand") {
                    set_h(overlay_ptr, 1);
                }
                if let Some(set_v) = lookup_sym::<SetExpand>(&loader, "gtk_widget_set_vexpand") {
                    set_v(overlay_ptr, 1);
                }
                let total_w = 46 + (VISIBLE_COLS as i32) * CELL_W;
                let total_h = CELL_H + (VISIBLE_ROWS as i32) * CELL_H;
                gtk_dynamic_loader::widget_set_size_request(&loader, *grid_widget.as_ref(), total_w, total_h);
                sw
            }
            None => panic!("gtk_scrolled_window_new not available"),
        }
    };

    // Keyboard navigation
    if is_gtk4 {
        if let Some(ctrl_new) = loader.symbols.gtk_event_controller_key_new {
            let ctrl = unsafe { ctrl_new() };
            if !ctrl.is_null() {
                if let Some(add_ctrl) = loader.symbols.gtk_widget_add_controller {
                    unsafe { add_ctrl(*win.as_ref(), ctrl); }
                }
                let sel_coord = selected_coord.clone();
                let sc_nav = static_cells.clone();
                let texts_nav = texts.clone();
                let edit_entry_nav = editing_entry.clone();
                let loader_nav = loader.clone();
                let formula_e = formula_entry.clone();
                let refresh_nav = refresh.clone();

                unsafe {
                    let _ = gtk_dynamic_loader::connect_signal_bool(loader.symbols.as_ref(), ctrl, "key-pressed", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                        let keyval = if let Some(get_kv) = loader_nav.symbols.gdk_event_get_keyval {
                            get_kv(ev)
                        } else { 0 };

                        if edit_entry_nav.borrow().is_some() {
                            if keyval == 0xFF1B {
                                let entry = edit_entry_nav.borrow_mut().take();
                                if let Some(e) = entry {
                                    gtk_dynamic_loader::destroy_widget(&loader_nav, *e.as_ref());
                                }
                            }
                            return 1;
                        }

                        let mut coord = sel_coord.borrow_mut();
                        if let Some((r, c)) = *coord {
                            match keyval {
                                0xFF52 | 0xFE52 => { if r > 0 { *coord = Some((r - 1, c)); } }
                                0xFF54 | 0xFE54 => { if r + 1 < VISIBLE_ROWS { *coord = Some((r + 1, c)); } }
                                0xFF51 | 0xFE51 => { if c > 0 { *coord = Some((r, c - 1)); } }
                                0xFF53 | 0xFE53 => { if c + 1 < VISIBLE_COLS { *coord = Some((r, c + 1)); } }
                                0xFF0D | 0xFF8D => {
                                    if edit_entry_nav.borrow().is_none() {
                                        if let Ok(entry) = gtk::create_entry() {
                                            entry.set_text(&texts_nav.borrow()[r][c]);
                                            entry.set_width_chars(12);
                                            let left = (c as i32 + 1) * CELL_W;
                                            let top = (r as i32 + 1) * CELL_H;
                                            gtk_dynamic_loader::widget_set_margin_start(&loader_nav, *entry.as_ref(), left);
                                            gtk_dynamic_loader::widget_set_margin_top(&loader_nav, *entry.as_ref(), top);
                                            add_overlay_child(&loader_nav, overlay_ptr, *entry.as_ref());
                                            set_overlay_pass_through(&loader_nav, overlay_ptr, *entry.as_ref(), false);
                                            let entry_commit = entry.clone();
                                            let texts_c = texts_nav.clone();
                                            let sc_c = sc_nav.clone();
                                            let l_c = loader_nav.clone();
                                            let edit_ent = edit_entry_nav.clone();
                                            let formula_f = formula_e.clone();
                                            let refresh_r = refresh_nav.clone();
                                            let syms2 = l_c.symbols.clone();
                                            let entry_inst = *entry_commit.as_ref();
                                            let _ = gtk_dynamic_loader::connect_signal(syms2.as_ref(), entry_inst, "activate", Box::new(move || {
                                                let new_text = entry_commit.get_text().unwrap_or_default();
                                                if let Ok(mut t) = texts_c.try_borrow_mut() {
                                                    if r < t.len() && c < t[r].len() { t[r][c] = new_text.clone(); }
                                                }
                                                if let Ok(sc) = sc_c.try_borrow() {
                                                    if r < sc.len() && c < sc[r].len() {
                                                        style_cell_label(&sc[r][c], &new_text);
                                                    }
                                                }
                                                formula_f.set_text(&new_text);
                                                let old = edit_ent.borrow_mut().take();
                                                if let Some(e) = old { gtk_dynamic_loader::destroy_widget(&l_c, *e.as_ref()); }
                                                refresh_r();
                                            }), 2);
                                            entry.set_size_request(CELL_W, CELL_H);
                                            entry.grab_focus();
                                            *edit_entry_nav.borrow_mut() = Some(entry);
                                        }
                                    }
                                    return 1;
                                }
                                _ => {}
                            }
                        }

                        if let Some((r, c)) = *coord {
                            if let Ok(sc) = sc_nav.try_borrow() {
                                for rr in 0..sc.len() {
                                    for cc in 0..sc[rr].len() { sc[rr][cc].remove_class("selected"); }
                                }
                                if r < sc.len() && c < sc[r].len() { sc[r][c].add_class("selected"); }
                            }
                            if r < texts_nav.borrow().len() && c < texts_nav.borrow()[r].len() {
                                formula_e.set_text(&texts_nav.borrow()[r][c]);
                            }
                        }
                        0
                    }));
                }
            }
        }
    } else {
        let syms_arc = loader.symbols.clone();
        let syms_ref = syms_arc.as_ref();
        let instance = *win.as_ref();
        let sel_coord = selected_coord.clone();
        let sc_nav = static_cells.clone();
        let texts_nav = texts.clone();
        let edit_entry_nav = editing_entry.clone();
        let loader_nav = loader.clone();
        let formula_e = formula_entry.clone();
        let refresh_nav = refresh.clone();

        unsafe {
            let _ = gtk_dynamic_loader::connect_signal_bool(syms_ref, instance, "key-press-event", Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                let keyval = if let Some(get_kv) = loader_nav.symbols.gdk_event_get_keyval {
                    get_kv(ev)
                } else { 0 };

                if edit_entry_nav.borrow().is_some() {
                    if keyval == 0xFF1B {
                        let entry = edit_entry_nav.borrow_mut().take();
                        if let Some(e) = entry { gtk_dynamic_loader::destroy_widget(&loader_nav, *e.as_ref()); }
                    }
                    return 1;
                }

                let mut coord = sel_coord.borrow_mut();
                if let Some((r, c)) = *coord {
                    match keyval {
                        0xFF52 | 0xFE52 => { if r > 0 { *coord = Some((r - 1, c)); } }
                        0xFF54 | 0xFE54 => { if r + 1 < VISIBLE_ROWS { *coord = Some((r + 1, c)); } }
                        0xFF51 | 0xFE51 => { if c > 0 { *coord = Some((r, c - 1)); } }
                        0xFF53 | 0xFE53 => { if c + 1 < VISIBLE_COLS { *coord = Some((r, c + 1)); } }
                        0xFF0D | 0xFF8D => {
                            if edit_entry_nav.borrow().is_none() {
                                if let Ok(entry) = gtk::create_entry() {
                                    entry.set_text(&texts_nav.borrow()[r][c]);
                                    entry.set_width_chars(12);
                                    let left = (c as i32 + 1) * CELL_W;
                                    let top = (r as i32 + 1) * CELL_H;
                                    gtk_dynamic_loader::widget_set_margin_start(&loader_nav, *entry.as_ref(), left);
                                    gtk_dynamic_loader::widget_set_margin_top(&loader_nav, *entry.as_ref(), top);
                                    add_overlay_child(&loader_nav, overlay_ptr, *entry.as_ref());
                                    set_overlay_pass_through(&loader_nav, overlay_ptr, *entry.as_ref(), false);
                                    let entry_commit = entry.clone();
                                    let texts_c = texts_nav.clone();
                                    let sc_c = sc_nav.clone();
                                    let l_c = loader_nav.clone();
                                    let edit_ent = edit_entry_nav.clone();
                                    let formula_f = formula_e.clone();
                                    let refresh_r = refresh_nav.clone();
                                    let syms2 = l_c.symbols.clone();
                                    let entry_inst = *entry_commit.as_ref();
                                    let _ = gtk_dynamic_loader::connect_signal(syms2.as_ref(), entry_inst, "activate", Box::new(move || {
                                        let new_text = entry_commit.get_text().unwrap_or_default();
                                        if let Ok(mut t) = texts_c.try_borrow_mut() {
                                            if r < t.len() && c < t[r].len() { t[r][c] = new_text.clone(); }
                                        }
                                        if let Ok(sc) = sc_c.try_borrow() {
                                            if r < sc.len() && c < sc[r].len() {
                                                style_cell_label(&sc[r][c], &new_text);
                                            }
                                        }
                                        formula_f.set_text(&new_text);
                                        let old = edit_ent.borrow_mut().take();
                                        if let Some(e) = old { gtk_dynamic_loader::destroy_widget(&l_c, *e.as_ref()); }
                                        refresh_r();
                                    }), 2);
                                    entry.set_size_request(CELL_W, CELL_H);
                                    entry.grab_focus();
                                    *edit_entry_nav.borrow_mut() = Some(entry);
                                }
                            }
                            return 1;
                        }
                        _ => {}
                    }
                }

                if let Some((r, c)) = *coord {
                    if let Ok(sc) = sc_nav.try_borrow() {
                        for rr in 0..sc.len() {
                            for cc in 0..sc[rr].len() { sc[rr][cc].remove_class("selected"); }
                        }
                        if r < sc.len() && c < sc[r].len() { sc[r][c].add_class("selected"); }
                    }
                    if r < texts_nav.borrow().len() && c < texts_nav.borrow()[r].len() {
                        formula_e.set_text(&texts_nav.borrow()[r][c]);
                    }
                }
                0
            }));
        }
    }

    // File operations
    let syms_arc = loader.symbols.clone();
    let texts_open = texts.clone();
    let sc_open = static_cells.clone();
    let refresh_open = refresh.clone();

    let _ = open_btn.on_click(move || {
        let syms = syms_arc.as_ref();
        if let (Some(chooser_new), Some(native_run), Some(get_fn), Some(destroy_widget_fn), Some(gfree)) = (
            syms.gtk_file_chooser_native_new,
            syms.gtk_native_dialog_run,
            syms.gtk_file_chooser_get_filename,
            syms.gtk_widget_destroy,
            syms.g_free,
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
                                            style_cell_label(&sc[r][c], &texts_open.borrow()[r][c]);
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

    let syms_save = loader.symbols.clone();
    let texts_save = texts.clone();

    let _ = save_btn.on_click(move || {
        let syms = syms_save.as_ref();
        if let (Some(chooser_new), Some(native_run), Some(get_fn), Some(destroy_widget_fn), Some(gfree)) = (
            syms.gtk_file_chooser_native_new,
            syms.gtk_native_dialog_run,
            syms.gtk_file_chooser_get_filename,
            syms.gtk_widget_destroy,
            syms.g_free,
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

    let _ = quit_btn.on_click(|| std::process::exit(0));

    // Layout
    let vbox = gtk::create_box(gtk::Orientation::Vertical, 0)?;
    let toolbar_box = gtk::create_box(gtk::Orientation::Vertical, 0)?;
    toolbar_box.append(&toolbar);

    let formula_and_grid = gtk::create_box(gtk::Orientation::Vertical, 2)?;
    formula_and_grid.append(&formula_hbox);

    // Wrap scrolled in an adapter BoxWidget for our layout
    // Use the raw scrolled pointer through a thin wrapper
    struct ScrolledWindow(*mut std::ffi::c_void);
    impl AsRef<*mut std::ffi::c_void> for ScrolledWindow {
        fn as_ref(&self) -> &*mut std::ffi::c_void { &self.0 }
    }
    let sw = ScrolledWindow(scrolled_ptr);
    formula_and_grid.append(&sw);

    // Make formula_and_grid expand to fill window
    let fg_ptr = *formula_and_grid.as_ref();
    if let Some(set_vexpand) = lookup_sym::<unsafe extern "C" fn(*mut std::ffi::c_void, i32)>(&loader, "gtk_widget_set_vexpand") {
        unsafe { set_vexpand(fg_ptr, 1); }
    }
    // Also make vbox expand its children
    let vbox_ptr = *vbox.as_ref();
    if let Some(set_vexpand) = lookup_sym::<unsafe extern "C" fn(*mut std::ffi::c_void, i32)>(&loader, "gtk_widget_set_hexpand") {
        unsafe { set_vexpand(vbox_ptr, 1); }
    }
    vbox.append(&toolbar_box);
    vbox.append(&formula_and_grid);
    win.set_child(&vbox);
    win.present();

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
