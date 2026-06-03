use rustxwidgets::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const ROWS: usize = 50;
const COLS: usize = 20;
const CELL_W: f64 = 120.0;
const CELL_H: f64 = 28.0;
const COL_HDR_W: f64 = 46.0;
const ROW_HDR_H: f64 = CELL_H;

type CellFormat = (bool, bool, u8, String, String); // bold, italic, align, fg_hex, bg_hex

fn col_label(n: usize) -> String {
    if n < 26 {
        format!("{}", (b'A' + (n as u8)) as char)
    } else {
        let mut s = String::new();
        let mut v = n;
        loop {
            s.insert(0, (b'A' + (v % 26) as u8) as char);
            v /= 26;
            if v == 0 {
                break;
            }
            v -= 1;
        }
        s
    }
}

fn parse_hex(hex: &str) -> (f64, f64, f64) {
    if hex.len() >= 7 && hex.as_bytes()[0] == b'#' {
        let r = u8::from_str_radix(&hex[1..3], 16).unwrap_or(0) as f64 / 255.0;
        let g = u8::from_str_radix(&hex[3..5], 16).unwrap_or(0) as f64 / 255.0;
        let b = u8::from_str_radix(&hex[5..7], 16).unwrap_or(0) as f64 / 255.0;
        (r, g, b)
    } else {
        (0.0, 0.0, 0.0)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows = ROWS;
    let cols = COLS;
    let cw = CELL_W;
    let ch = CELL_H;
    let chw = COL_HDR_W;

    let app = App::init()?;
    let win = app.create_window()?;
    win.set_title("x-sheet");
    win.set_default_size(1000, 700);

    // Data model
    let texts: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(
        (0..rows).map(|_| (0..cols).map(|_| String::new()).collect()).collect()
    ));
    let fmts: Rc<RefCell<Vec<Vec<CellFormat>>>> = Rc::new(RefCell::new(
        (0..rows).map(|_| (0..cols).map(|_| (false, false, 0u8, "#000000".into(), "#ffffff".into())).collect()).collect()
    ));
    let selected: Rc<RefCell<Option<(usize, usize)>>> = Rc::new(RefCell::new(Some((0, 0))));

    // Seed some demo data
    {
        let mut t = texts.borrow_mut();
        let mut f = fmts.borrow_mut();
        t[0][0] = "Revenue".into();       f[0][0].0 = true;
        t[0][1] = "1000".into();
        t[0][2] = "2000".into();
        t[0][3] = "=B1+C1".into();
        t[1][0] = "Costs".into();         f[1][0].0 = true;
        t[1][1] = "400".into();
        t[1][2] = "600".into();
        t[1][3] = "=B2+C2".into();
        t[2][0] = "Profit".into();        f[2][0].0 = true;
        t[2][3] = "=B3+C3".into();
        t[3][0] = "VeryLongCellContentThatOverflowsAcrossEmptyCells".into();
        t[4][0] = "42".into();
        t[4][1] = "=SUM".into();
        t[5][1] = "Hello".into();
        t[5][2] = "World".into();
        t[6..10].iter_mut().for_each(|r| {
            r[1] = "alpha".into();
            r[2] = "beta".into();
        });
    }

    // --- Layout (cross-platform box widget) ---
    let vbox = app.create_box(Orientation::Vertical, 2)?;
    let toolbar = app.create_box(Orientation::Horizontal, 2)?;
    let formula_bar = app.create_box(Orientation::Horizontal, 4)?;

    let bold_btn = app.create_button("B")?;
    let italic_btn = app.create_button("I")?;
    let al_l = app.create_button("L")?;
    let al_c = app.create_button("C")?;
    let al_r = app.create_button("R")?;
    let hl_btn = app.create_button("HL")?;
    let fg_btn = app.create_button("FG")?;
    let quit_btn = app.create_button("Quit")?;

    for btn in [&bold_btn, &italic_btn, &al_l, &al_c, &al_r] {
        btn.set_size_request(28, 24);
        toolbar.append(btn);
    }
    toolbar.append(&hl_btn);
    toolbar.append(&fg_btn);
    toolbar.append(&quit_btn);

    let fx_label = app.create_label("  fx  ")?;
    formula_bar.append(&fx_label);
    let formula_entry = app.create_entry()?;
    formula_entry.set_width_chars(40);
    formula_entry.set_size_request(400, 26);
    formula_bar.append(&formula_entry);

    // --- Canvas ---
    let canvas = app.create_canvas()?;
    canvas.set_size_request(chw as i32 + cols as i32 * cw as i32, ROW_HDR_H as i32 + rows as i32 * ch as i32);
    // Set content size for scrolling in GTK4
    canvas.set_content_size(chw as i32 + cols as i32 * cw as i32, ROW_HDR_H as i32 + rows as i32 * ch as i32);

    let canvas_clone = canvas.clone();

    // Draw callback
    let t_draw = texts.clone();
    let f_draw = fmts.clone();
    let sel_draw = selected.clone();
    canvas.set_draw_callback(Box::new(move |ctx: &mut dyn DrawContext, _w: i32, _h: i32| {
        let total_w = chw + cols as f64 * cw;
        let total_h = ROW_HDR_H + rows as f64 * ch;

        // White background
        ctx.clear(1.0, 1.0, 1.0, 1.0);

        // Header backgrounds
        ctx.save();
        ctx.clip(0.0, 0.0, total_w, ROW_HDR_H);
        ctx.fill_rect(0.0, 0.0, chw, ROW_HDR_H, 0.91, 0.91, 0.91, 1.0);
        ctx.fill_rect(chw, 0.0, cols as f64 * cw, ROW_HDR_H, 0.8, 0.8, 0.8, 1.0);
        ctx.fill_rect(0.0, ROW_HDR_H, chw, rows as f64 * ch, 0.8, 0.8, 0.8, 1.0);
        ctx.restore();

        // Grid lines
        for c in 0..=cols {
            let x = chw + c as f64 * cw;
            ctx.stroke_rect(x, 0.0, 0.0, total_h, 0.7, 0.7, 0.7, 1.0, 0.5);
        }
        for r in 0..=rows {
            let y = ROW_HDR_H + r as f64 * ch;
            ctx.stroke_rect(0.0, y, total_w, 0.0, 0.7, 0.7, 0.7, 1.0, 0.5);
        }

        // Column headers
        for c in 0..cols {
            let lbl = col_label(c);
            let (xb, _, w, _) = ctx.text_extents(&lbl, "monospace", 12.0);
            let x = chw + c as f64 * cw + cw / 2.0 - xb - w / 2.0;
            let y = ROW_HDR_H / 2.0;
            ctx.draw_text(x, y, &lbl, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
        }

        // Row headers
        for r in 0..rows {
            let lbl = format!("{}", r + 1);
            let (xb, _, w, _) = ctx.text_extents(&lbl, "monospace", 12.0);
            let x = chw / 2.0 - xb - w / 2.0;
            let y = ROW_HDR_H + r as f64 * ch + ch / 2.0;
            ctx.draw_text(x, y, &lbl, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
        }

        // Cell contents
        let t = t_draw.borrow();
        let f = f_draw.borrow();
        for r in 0..rows.min(t.len()) {
            for c in 0..cols.min(t[r].len()) {
                let text = &t[r][c];
                if text.is_empty() {
                    continue;
                }
                let (bold, italic, align, fg_hex, bg_hex) = &f[r][c];
                let cx = chw + c as f64 * cw;
                let cy = ROW_HDR_H + r as f64 * ch;

                // Background
                if bg_hex != "#ffffff" {
                    let (br, bg, bb) = parse_hex(bg_hex);
                    ctx.fill_rect(cx, cy + 1.0, cw, ch - 2.0, br, bg, bb, 1.0);
                }

                // Text color
                let (fr, fgg, fb) = if fg_hex != "#000000" {
                    parse_hex(fg_hex)
                } else {
                    (0.0, 0.0, 0.0)
                };

                let weight = if *bold { 1 } else { 0 };
                let slant = if *italic { 1 } else { 0 };
                let font_name = if *bold || *italic {
                    if *bold && *italic { "monospace:bold:italic" } else if *bold { "monospace:bold" } else { "monospace:italic" }
                } else {
                    "monospace"
                };

                let (xb, _, tw, th) = ctx.text_extents(text, font_name, 13.0);
                let pad = 4.0;
                let tx = match *align {
                    0 => cx + pad - xb,
                    1 => cx + cw / 2.0 - xb - tw / 2.0,
                    _ => cx + cw - pad - xb - tw,
                };
                let ty = cy + ch / 2.0 + th / 2.0;

                ctx.save();
                ctx.clip(cx, cy, cw, ch);
                ctx.draw_text(tx, ty, text, font_name, 13.0, fr, fgg, fb, 1.0);
                ctx.restore();
            }
        }

        // Selection highlight
        if let Some((sr, sc)) = *sel_draw.borrow() {
            let sx = chw + sc as f64 * cw;
            let sy = ROW_HDR_H + sr as f64 * ch;
            ctx.fill_rect(sx, sy, cw, ch, 0.83, 0.91, 1.0, 0.3);
            ctx.stroke_rect(sx + 0.5, sy + 0.5, cw - 1.0, ch - 1.0, 0.1, 0.45, 0.91, 1.0, 2.0);
        }
    }));

    // --- Layout assembly ---
    vbox.append(&toolbar);
    vbox.append(&formula_bar);

    // Build entry grid for clickable cell editing
    let cell_entries: Rc<RefCell<Vec<Vec<Entry>>>> = Rc::new(RefCell::new(Vec::new()));
    let entry_grid = app.create_grid()?;
    for r in 0..rows {
        let mut row_entries = Vec::new();
        for c in 0..cols {
            let entry = app.create_entry()?;
            entry.set_text(&texts.borrow()[r][c]);
            entry.set_size_request(cw as i32, ch as i32);
            entry_grid.attach(&entry, c as i32, r as i32, 1, 1);
            row_entries.push(entry);
        }
        cell_entries.borrow_mut().push(row_entries);
    }
    // Entry grid is placed below the Canvas, using a Box with the canvas on top
    let grid_container = app.create_box(Orientation::Vertical, 0)?;
    grid_container.append(&canvas_clone);
    grid_container.append(&entry_grid);

    vbox.append(&grid_container);
    win.set_child(&vbox);
    win.present();

    // Connect cell entry changes back to data model
    let t_edit = texts.clone();
    let cv = canvas.clone();
    for r in 0..rows {
        for c in 0..cols {
            let t2 = t_edit.clone();
            let cv2 = cv.clone();
            let entry = cell_entries.borrow()[r][c].clone();
            let e2 = entry.clone();
            entry.connect_changed(move || {
                if let Some(text) = e2.get_text() {
                    if r < t2.borrow().len() && c < t2.borrow()[r].len() {
                        t2.borrow_mut()[r][c] = text;
                        cv2.queue_redraw();
                    }
                }
            });
        }
    }

    // Formatting buttons
    let sel_fmt = selected.clone();
    let fmts_fmt = fmts.clone();
    let cv_fmt = canvas.clone();
    let _ = bold_btn.on_click(move || {
        if let Some((r, c)) = *sel_fmt.borrow() {
            let mut f = fmts_fmt.borrow_mut();
            if r < f.len() && c < f[r].len() {
                f[r][c].0 = !f[r][c].0;
                cv_fmt.queue_redraw();
            }
        }
    });

    let sel_ital = selected.clone();
    let fmts_ital = fmts.clone();
    let cv_ital = canvas.clone();
    let _ = italic_btn.on_click(move || {
        if let Some((r, c)) = *sel_ital.borrow() {
            let mut f = fmts_ital.borrow_mut();
            if r < f.len() && c < f[r].len() {
                f[r][c].1 = !f[r][c].1;
                cv_ital.queue_redraw();
            }
        }
    });

    let sel_hl = selected.clone();
    let fmts_hl = fmts.clone();
    let cv_hl = canvas.clone();
    let _ = hl_btn.on_click(move || {
        if let Some((r, c)) = *sel_hl.borrow() {
            let mut f = fmts_hl.borrow_mut();
            if r < f.len() && c < f[r].len() {
                let bg = &f[r][c].4.clone();
                f[r][c].4 = if bg == "#ffff00" { "#88ff88".into() }
                            else if bg == "#88ff88" { "#ffffff".into() }
                            else { "#ffff00".into() };
                cv_hl.queue_redraw();
            }
        }
    });

    let sel_quit = selected.clone();
    let _ = quit_btn.on_click(move || {
        let _ = sel_quit.borrow();
        std::process::exit(0);
    });

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
