#![allow(non_upper_case_globals)]

use crate::grid::CellAddr;
use rustxwidgets::backends_gtk_adapter::*;
use rustxwidgets::core::DrawContext;

const CELL_W: f64 = 100.0;
const CELL_H: f64 = 28.0;
const HEADER_W: f64 = 50.0;
const HEADER_H: f64 = 28.0;

const GDK_KEY_Return: u32 = 0xFF0D;
const GDK_KEY_KP_Enter: u32 = 0xFF8D;
const GDK_KEY_Escape: u32 = 0xFF1B;
const GDK_KEY_Tab: u32 = 0xFF09;
const GDK_KEY_BackSpace: u32 = 0xFF08;
const GDK_KEY_Delete: u32 = 0xFFFF;
const GDK_KEY_Left: u32 = 0xFF51;
const GDK_KEY_Up: u32 = 0xFF52;
const GDK_KEY_Right: u32 = 0xFF53;
const GDK_KEY_Down: u32 = 0xFF54;
const GDK_KEY_Home: u32 = 0xFF50;
const GDK_KEY_End: u32 = 0xFF57;
const GDK_KEY_Page_Up: u32 = 0xFF55;
const GDK_KEY_Page_Down: u32 = 0xFF56;
const GDK_KEY_F2: u32 = 0xFFBF;

pub fn run_gtk(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::gtk::init()
        .map_err(|e| format!("GTK init failed: {e}"))?;

    let win = create_window()?;
    win.set_title(&format!("corro {}", env!("CARGO_PKG_VERSION")));
    win.set_default_size(1200, 800);

    let vbox = create_box(Orientation::Vertical, 0)?;

    let formula_bar = create_box(Orientation::Horizontal, 2)?;
    let addr_label = create_label("A1")?;
    addr_label.set_xalign(0.0);
    addr_label.set_size_request(80, 28);
    let f_label = create_label("  fx  ")?;
    let formula_entry = create_entry()?;
    formula_entry.set_hexpand(true);
    formula_bar.append(&addr_label);
    formula_bar.append(&f_label);
    formula_bar.append(&formula_entry);
    vbox.append(&formula_bar);

    let sheet = app.core.workbook.active_sheet().clone();
    let total_rows = sheet.grid.main_rows().max(50) as usize;
    let total_cols = sheet.grid.main_cols().max(10) as usize;

    let spreadsheet = create_spreadsheet(total_rows, total_cols)?;

    let data: std::collections::HashMap<(u32, u32), String> = {
        let mut map = std::collections::HashMap::new();
        for r in 0..total_rows.min(200) as u32 {
            for c in 0..total_cols.min(50) as u32 {
                let addr = CellAddr::Main { row: r, col: c };
                if let Some(val) = sheet.grid.get(&addr) {
                    map.insert((r, c), val.clone());
                }
            }
        }
        map
    };

    let shared = std::rc::Rc::new(super::sheet::SharedState {
        cursor_row: std::cell::Cell::new(0),
        cursor_col: std::cell::Cell::new(0),
        scroll_row: std::cell::Cell::new(0),
        scroll_col: std::cell::Cell::new(0),
        edit_buf: std::cell::RefCell::new(String::new()),
        editing: std::cell::Cell::new(false),
        data: std::cell::RefCell::new(data),
    });

    let visible_rows = 25u32;
    let visible_cols = 10u32;

    spreadsheet.set_draw_callback(Box::new({
        let s = shared.clone();
        let tr = total_rows as u32;
        let tc = total_cols as u32;
        move |dc: &mut dyn DrawContext, _w: i32, _h: i32| {
            dc.clear(1.0, 1.0, 1.0, 1.0);
            let top_r = s.scroll_row.get() as usize;
            let left_c = s.scroll_col.get() as usize;
            let max_r = (tr as usize).min(top_r + visible_rows as usize + 2);
            let max_c = (tc as usize).min(left_c + visible_cols as usize + 2);

            dc.save();

            for c in left_c..max_c {
                let x = HEADER_W + (c - left_c) as f64 * CELL_W;
                let label = column_label(c as u32);
                dc.fill_rect(x, 0.0, CELL_W, HEADER_H, 0.92, 0.92, 0.92, 1.0);
                let te = dc.text_extents(&label, "monospace", 12.0);
                dc.draw_text_styled(x + (CELL_W - te.2) / 2.0, HEADER_H - 8.0,
                    &label, "monospace", 12.0, 0.2, 0.2, 0.2, 1.0, 0, 1);
            }

            for r in top_r..max_r {
                let y = HEADER_H + (r - top_r) as f64 * CELL_H;
                let label = format!("{}", r + 1);
                dc.fill_rect(0.0, y, HEADER_W, CELL_H, 0.92, 0.92, 0.92, 1.0);
                let te = dc.text_extents(&label, "monospace", 12.0);
                dc.draw_text(HEADER_W - te.2 - 5.0, y + CELL_H - 8.0, &label, "monospace", 12.0, 0.2, 0.2, 0.2, 1.0);
            }

            let grid_w = (max_c - left_c) as f64 * CELL_W;
            let grid_h = (max_r - top_r) as f64 * CELL_H;
            dc.stroke_rect(HEADER_W, 0.0, grid_w, grid_h, 0.85, 0.85, 0.85, 1.0, 0.5);

            let is_editing = s.editing.get();
            let eb_str = s.edit_buf.borrow().clone();
            let data_borrow = s.data.borrow();

            for r in top_r..max_r {
                for c in left_c..max_c {
                    let is_cursor = r as u32 == s.cursor_row.get() && c as u32 == s.cursor_col.get();
                    let x = HEADER_W + (c - left_c) as f64 * CELL_W;
                    let y = HEADER_H + (r - top_r) as f64 * CELL_H;

                    if is_editing && is_cursor {
                        dc.fill_rect(x, y, CELL_W, CELL_H, 0.8, 1.0, 0.8, 0.3);
                        dc.stroke_rect(x, y, CELL_W, CELL_H, 0.0, 0.7, 0.0, 1.0, 2.0);
                        dc.draw_text(x + 3.0, y + CELL_H - 8.0, &eb_str, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
                    } else if is_cursor {
                        dc.fill_rect(x, y, CELL_W, CELL_H, 0.2, 0.5, 1.0, 0.15);
                        dc.stroke_rect(x, y, CELL_W, CELL_H, 0.2, 0.5, 1.0, 1.0, 1.5);
                        if let Some(text) = data_borrow.get(&(r as u32, c as u32)) {
                            dc.draw_text(x + 3.0, y + CELL_H - 8.0, text, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
                        }
                    } else if let Some(text) = data_borrow.get(&(r as u32, c as u32)) {
                        dc.draw_text(x + 3.0, y + CELL_H - 8.0, text, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
                    }
                }
            }
            dc.restore();
        }
    }));

    spreadsheet.on_key(Box::new({
        let s = shared.clone();
        let f_entry = formula_entry.clone();
        let a_label = addr_label.clone();
        let ss = spreadsheet.clone();
        let tr = total_rows as u32;
        let tc = total_cols as u32;
        move |keyval: u32, _state: u32| -> bool {
            handle_key(keyval, &s, &f_entry, &a_label, tr, tc, visible_rows, visible_cols, ss.canvas())
        }
    }));

    spreadsheet.on_click(Box::new({
        let s = shared.clone();
        let ss = spreadsheet.clone();
        move |x: f64, y: f64| {
            let col = ((x - HEADER_W) / CELL_W) as i32 + s.scroll_col.get() as i32;
            let row = ((y - HEADER_H) / CELL_H) as i32 + s.scroll_row.get() as i32;
            if col >= 0 && row >= 0 {
                s.cursor_col.set(col as u32);
                s.cursor_row.set(row as u32);
                ss.queue_redraw();
            }
        }
    }));

    vbox.append(&spreadsheet);

    let status_bar = create_label("Ready")?;
    status_bar.set_xalign(0.0);
    vbox.append(&status_bar);

    win.set_child(&vbox);
    win.present();

    _backend.run().map_err(|e| format!("GUI error: {e}"))?;
    Ok(())
}

fn handle_key(
    keyval: u32,
    shared: &super::sheet::SharedState,
    formula_entry: &Entry,
    addr_label: &Label,
    total_rows: u32, total_cols: u32,
    visible_rows: u32, visible_cols: u32,
    canvas: &Canvas,
) -> bool {
    macro_rules! redraw { () => { canvas.queue_redraw(); } }

    if shared.editing.get() {
    #[allow(non_upper_case_globals)]
    match keyval {
            GDK_KEY_Return | GDK_KEY_KP_Enter => {
                commit_edit(shared, formula_entry);
                let next = (shared.cursor_row.get() + 1).min(total_rows - 1);
                shared.cursor_row.set(next);
                if next >= shared.scroll_row.get() + visible_rows {
                    shared.scroll_row.set(shared.scroll_row.get() + 1);
                }
                redraw!(); return true;
            }
            GDK_KEY_Escape => {
                shared.editing.set(false);
                shared.edit_buf.borrow_mut().clear();
                update_formula_bar(shared, formula_entry, addr_label);
                redraw!(); return true;
            }
            GDK_KEY_Tab => {
                commit_edit(shared, formula_entry);
                let next = (shared.cursor_col.get() + 1).min(total_cols - 1);
                shared.cursor_col.set(next);
                if next >= shared.scroll_col.get() + visible_cols { shared.scroll_col.set(shared.scroll_col.get() + 1); }
                update_formula_bar(shared, formula_entry, addr_label);
                redraw!(); return true;
            }
            GDK_KEY_BackSpace => { shared.edit_buf.borrow_mut().pop(); redraw!(); return true; }
            GDK_KEY_Delete => { shared.edit_buf.borrow_mut().clear(); redraw!(); return true; }
            GDK_KEY_Left  => { commit_and_move(shared, |s| if s.cursor_col.get() > 0 { s.cursor_col.set(s.cursor_col.get() - 1) }); redraw!(); return true; }
            GDK_KEY_Right => { commit_and_move(shared, |s| s.cursor_col.set((s.cursor_col.get() + 1).min(total_cols - 1))); redraw!(); return true; }
            GDK_KEY_Up    => { commit_and_move(shared, |s| if s.cursor_row.get() > 0 { s.cursor_row.set(s.cursor_row.get() - 1) }); redraw!(); return true; }
            GDK_KEY_Down  => { commit_and_move(shared, |s| s.cursor_row.set((s.cursor_row.get() + 1).min(total_rows - 1))); redraw!(); return true; }
            _ => {
                if keyval >= 32 && keyval <= 126 {
                    shared.edit_buf.borrow_mut().push(char::from_u32(keyval).unwrap_or('?'));
                    formula_entry.set_text(&shared.edit_buf.borrow());
                    redraw!(); return true;
                }
            }
        }
        return false;
    }

    #[allow(non_upper_case_globals)]
    match keyval {
        GDK_KEY_Left  => { nav(shared, formula_entry, addr_label, |s| if s.cursor_col.get() > 0 { s.cursor_col.set(s.cursor_col.get() - 1); if s.cursor_col.get() < s.scroll_col.get() { s.scroll_col.set(s.cursor_col.get()); } }); }
        GDK_KEY_Right => { nav(shared, formula_entry, addr_label, |s| { let n = (s.cursor_col.get() + 1).min(total_cols - 1); s.cursor_col.set(n); if n >= s.scroll_col.get() + visible_cols { s.scroll_col.set(s.scroll_col.get() + 1); } }); }
        GDK_KEY_Up    => { nav(shared, formula_entry, addr_label, |s| if s.cursor_row.get() > 0 { s.cursor_row.set(s.cursor_row.get() - 1); if s.cursor_row.get() < s.scroll_row.get() { s.scroll_row.set(s.cursor_row.get()); } }); }
        GDK_KEY_Down  => { nav(shared, formula_entry, addr_label, |s| { let n = (s.cursor_row.get() + 1).min(total_rows - 1); s.cursor_row.set(n); if n >= s.scroll_row.get() + visible_rows { s.scroll_row.set(s.scroll_row.get() + 1); } }); }
        GDK_KEY_Return | GDK_KEY_KP_Enter => { start_edit(shared, formula_entry); }
        GDK_KEY_Tab   => { nav(shared, formula_entry, addr_label, |s| { let n = (s.cursor_col.get() + 1).min(total_cols - 1); s.cursor_col.set(n); if n >= s.scroll_col.get() + visible_cols { s.scroll_col.set(s.scroll_col.get() + 1); } }); }
        GDK_KEY_Home  => { nav(shared, formula_entry, addr_label, |s| { s.cursor_col.set(0); s.scroll_col.set(0); }); }
        GDK_KEY_End   => { nav(shared, formula_entry, addr_label, |s| s.cursor_col.set(total_cols - 1)); }
        GDK_KEY_Page_Up   => { pg(shared, visible_rows, |s, p| { if s.cursor_row.get() >= p { s.cursor_row.set(s.cursor_row.get() - p); } else { s.cursor_row.set(0); } if s.cursor_row.get() < s.scroll_row.get() { s.scroll_row.set(s.cursor_row.get()); } }); }
        GDK_KEY_Page_Down => { pg(shared, visible_rows, |s, p| { let n = (s.cursor_row.get() + p).min(total_rows - 1); s.cursor_row.set(n); if n >= s.scroll_row.get() + visible_rows { s.scroll_row.set(s.scroll_row.get() + p); } }); }
        GDK_KEY_Delete => { shared.data.borrow_mut().remove(&(shared.cursor_row.get(), shared.cursor_col.get())); formula_entry.set_text(""); redraw!(); }
        GDK_KEY_F2 => { start_edit(shared, formula_entry); }
        _ => {
            if keyval >= 32 && keyval <= 126 {
                start_edit_with(shared, formula_entry, char::from_u32(keyval).unwrap_or('?'));
            } else { return false; }
        }
    }
    redraw!();
    true
}

fn commit_edit(shared: &super::sheet::SharedState, formula_entry: &Entry) {
    let text = shared.edit_buf.borrow().clone();
    shared.data.borrow_mut().insert((shared.cursor_row.get(), shared.cursor_col.get()), text.clone());
    shared.editing.set(false);
    shared.edit_buf.borrow_mut().clear();
    formula_entry.set_text(&text);
}

fn commit_and_move(shared: &super::sheet::SharedState, f: impl FnOnce(&super::sheet::SharedState)) {
    let text = shared.edit_buf.borrow().clone();
    shared.data.borrow_mut().insert((shared.cursor_row.get(), shared.cursor_col.get()), text);
    shared.editing.set(false);
    shared.edit_buf.borrow_mut().clear();
    f(shared);
}

fn start_edit(shared: &super::sheet::SharedState, formula_entry: &Entry) {
    let (r, c) = (shared.cursor_row.get(), shared.cursor_col.get());
    if let Some(val) = shared.data.borrow().get(&(r, c)) {
        shared.edit_buf.borrow_mut().clone_from(val);
    } else { shared.edit_buf.borrow_mut().clear(); }
    shared.editing.set(true);
    formula_entry.set_text("");
}

fn start_edit_with(shared: &super::sheet::SharedState, formula_entry: &Entry, ch: char) {
    let (r, c) = (shared.cursor_row.get(), shared.cursor_col.get());
    if let Some(val) = shared.data.borrow().get(&(r, c)) {
        shared.edit_buf.borrow_mut().clone_from(val);
    } else { shared.edit_buf.borrow_mut().clear(); }
    shared.edit_buf.borrow_mut().push(ch);
    shared.editing.set(true);
    formula_entry.set_text("");
}

fn pg(shared: &super::sheet::SharedState, _page: u32, f: impl FnOnce(&super::sheet::SharedState, u32)) {
    f(shared, _page);
}

fn nav(shared: &super::sheet::SharedState, formula_entry: &Entry, addr_label: &Label, f: impl FnOnce(&super::sheet::SharedState)) {
    f(shared);
    update_formula_bar(shared, formula_entry, addr_label);
}

fn update_formula_bar(shared: &super::sheet::SharedState, formula_entry: &Entry, addr_label: &Label) {
    let (r, c) = (shared.cursor_row.get(), shared.cursor_col.get());
    let val = shared.data.borrow().get(&(r, c)).cloned().unwrap_or_default();
    formula_entry.set_text(&val);
    addr_label.set_text(&addr_col_row(r, c));
}

fn column_label(idx: u32) -> String {
    if idx < 26 {
        let c = (b'A' + idx as u8) as char;
        c.to_string()
    } else {
        let prefix = (idx / 26 - 1) as u8;
        let suffix = (idx % 26) as u8;
        format!("{}{}", (b'A' + prefix) as char, (b'A' + suffix) as char)
    }
}

fn addr_col_row(row: u32, col: u32) -> String {
    format!("{}{}", column_label(col), row + 1)
}
