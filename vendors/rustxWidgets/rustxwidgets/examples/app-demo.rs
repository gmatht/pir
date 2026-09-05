use rustxwidgets::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const ROWS: usize = 100;
const COLS: usize = 26;
const CELL_W: i32 = 150;
const CELL_H: i32 = 28;
const CHW: f64 = 46.0;

type CellFormat = (bool, bool, u8, String, String);

fn col_label(n: usize) -> String {
    if n < 26 {
        format!("{}", (b'A' + n as u8) as char)
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

fn compute_row_y(heights: &[i32], row: usize) -> i32 {
    let mut y = 0;
    for i in 0..row {
        if i < heights.len() {
            y += heights[i];
        }
    }
    y
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

fn apply_format(entry: &Entry, r: usize, c: usize, fmts: &RefCell<Vec<Vec<CellFormat>>>) {
    let b = fmts.borrow();
    if r >= b.len() || c >= b[r].len() {
        return;
    }
    let fmt = &b[r][c];
    entry.remove_class("cell-bold");
    entry.remove_class("cell-italic");
    entry.remove_class("cell-both");
    entry.remove_class("cell-fg-red");
    entry.remove_class("cell-fg-blue");
    entry.remove_class("cell-fg-green");
    if fmt.0 && fmt.1 {
        entry.add_class("cell-both");
    } else if fmt.0 {
        entry.add_class("cell-bold");
    } else if fmt.1 {
        entry.add_class("cell-italic");
    }
    match fmt.3.as_str() {
        "#cc0000" => entry.add_class("cell-fg-red"),
        "#0000cc" => entry.add_class("cell-fg-blue"),
        "#006600" => entry.add_class("cell-fg-green"),
        _ => {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cw = CELL_W as f64;
    let ch = CELL_H as f64;
    let total_w = CHW + COLS as f64 * cw;
    let total_h = ch + ROWS as f64 * ch;

    let app = App::init()?;

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork")))]
    if let Some(loader) = rustxwidgets::backends::gtk::loader() {
        println!("GTK version: {:?}", loader.version());
    }

    let win = app.new_window()?;
    win.set_title("App Demo \u{2014} Spreadsheet with Menus and Dialogs");
    win.set_default_size(1600, 1000);

    // ── Data model ──────────────────────────────────────────────────
    let texts: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(
        (0..ROWS)
            .map(|_| (0..COLS).map(|_| String::new()).collect())
            .collect(),
    ));
    let fmts: Rc<RefCell<Vec<Vec<CellFormat>>>> = Rc::new(RefCell::new(
        (0..ROWS)
            .map(|_| {
                (0..COLS)
                    .map(|_| {
                        (
                            false,
                            false,
                            0u8,
                            "#000000".into(),
                            "#ffffff".into(),
                        )
                    })
                    .collect()
            })
            .collect(),
    ));
    let row_heights: Rc<RefCell<Vec<i32>>> =
        Rc::new(RefCell::new(vec![CELL_H; ROWS]));
    let sel: Rc<RefCell<Option<(usize, usize)>>> =
        Rc::new(RefCell::new(Some((0, 0))));
    let editing_entry: Rc<RefCell<Option<Entry>>> =
        Rc::new(RefCell::new(None));
    let text_input_active: Rc<RefCell<bool>> =
        Rc::new(RefCell::new(false));

    // seed demo data
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

    // ── Top-level layout ────────────────────────────────────────────
    let mut vbox = app.new_box(Orientation::Vertical, 0)?;

    // ── Menu bar: File, Edit, Help ──────────────────────────────────
    let mut file_menu = app.new_menu()?;
    file_menu.append("New", "app.new");
    file_menu.append("Open...", "app.open");
    file_menu.append("Save As...", "app.save");
    file_menu.append("Quit", "app.quit");

    let mut edit_menu = app.new_menu()?;
    edit_menu.append("Find && Replace...", "app.find");
    edit_menu.append("Clear Cell", "app.clear");

    let mut help_menu = app.new_menu()?;
    help_menu.append("About App Demo", "app.about");

    let mut menubar_model = app.new_menu()?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);
    menubar_model.append_submenu("Help", &help_menu);

    let menubar =
        app.new_menubar(&menubar_model, win.hwnd())?;
    vbox.append(&menubar);

    // ── Toolbar ─────────────────────────────────────────────────────
    let mut toolbar = app.new_box(Orientation::Horizontal, 2)?;
    let open_btn = app.create_button("Open")?;
    let save_btn = app.create_button("Save")?;
    let quit_btn = app.create_button("Quit")?;
    let bold_btn = app.create_button("B")?;
    let italic_btn = app.create_button("I")?;
    let al_l = app.create_button("AL")?;
    let al_c = app.create_button("AC")?;
    let al_r = app.create_button("AR")?;
    let hl_btn = app.create_button("HL")?;
    let fg_btn = app.create_button("FG")?;

    open_btn.set_size_request(60, 24);
    save_btn.set_size_request(70, 24);
    quit_btn.set_size_request(50, 24);
    for b in [
        &bold_btn, &italic_btn, &al_l, &al_c, &al_r, &hl_btn, &fg_btn,
    ] {
        b.set_size_request(28, 24);
    }
    bold_btn.set_font_style(700, false);
    italic_btn.set_font_style(400, true);
    hl_btn.add_class("hl-btn");
    toolbar.append(&open_btn);
    toolbar.append(&save_btn);
    toolbar.append(&quit_btn);
    toolbar.append(&bold_btn);
    toolbar.append(&italic_btn);
    toolbar.append(&al_l);
    toolbar.append(&al_c);
    toolbar.append(&al_r);
    toolbar.append(&hl_btn);
    toolbar.append(&fg_btn);
    vbox.append(&toolbar);

    // ── Formula bar ─────────────────────────────────────────────────
    let mut formula_bar = app.new_box(Orientation::Horizontal, 4)?;
    let fx_label = app.new_label("  fx  ")?;
    formula_bar.append(&fx_label);
    let formula_entry = app.new_entry()?;
    formula_entry.set_width_chars(40);
    formula_entry.set_size_request(400, 26);
    formula_bar.append(&formula_entry);
    vbox.append(&formula_bar);

    // ── Spreadsheet canvas ──────────────────────────────────────────
    let overlay = app.create_overlay()?;
    let canvas = app.new_canvas()?;
    canvas.set_size_request(total_w as i32, total_h as i32);
    canvas.set_content_size(total_w as i32, total_h as i32);
    overlay.set_child(&canvas);

    #[cfg(feature = "gtk")]
    {
        overlay.set_vexpand(true);
        overlay.set_hexpand(true);
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork")))]
    let scrolled = {
        let gtk_loader = rustxwidgets::backends::gtk::loader()
            .expect("GTK loader not initialized");
        let s = gtk_dynamic_loader::ScrolledWindow::new(
            gtk_loader.clone(),
        )?;
        s.set_policy(0, 0);
        s.set_child(&overlay);
        let css = r#"
        button { font-size: 11px; padding: 1px 8px; min-height: 20px; }
        entry { padding: 0; border: none; font-family: monospace; font-size: 13px; min-height: 0; }
        entry:focus { outline: none; }
        .cell-bold { font-weight: bold; }
        .cell-italic { font-style: italic; }
        .cell-both { font-weight: bold; font-style: italic; }
        .cell-fg-red { color: #cc0000; }
        .cell-fg-blue { color: #0000cc; }
        .cell-fg-green { color: #006600; }
        .hl-btn { background: #ffff00; }
        "#;
        if let Some(provider) =
            gtk_dynamic_loader::create_css_provider(&gtk_loader, css)
        {
            gtk_dynamic_loader::add_css_provider_global(
                &gtk_loader,
                win.hwnd(),
                provider,
                600,
            );
        }
        s
    };
    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork")))]
    scrolled.set_hexpand(true);
    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork")))]
    scrolled.set_vexpand(true);

    #[cfg(not(all(feature = "gtk", target_os = "linux", not(feature = "zork"))))]
    let scrolled = {
        let s = app.create_scrolled_window()?;
        s.set_policy(0, 0);
        s.set_child(&overlay);
        s
    };
    #[cfg(not(all(feature = "gtk", target_os = "linux", not(feature = "zork"))))]
    {
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);
    }

    // focus tracking for formula entry
    let tia2 = text_input_active.clone();
    let _ = formula_entry.connect_focus_in_event(move |_| {
        *tia2.borrow_mut() = true;
        0
    });
    let tia3 = text_input_active.clone();
    let _ = formula_entry.connect_focus_out_event(move |_| {
        *tia3.borrow_mut() = false;
        0
    });

    // ── Cell editing helpers ────────────────────────────────────────
    let commit_edit = {
        let te = texts.clone();
        let ee = editing_entry.clone();
        let ta = text_input_active.clone();
        let fe = formula_entry.clone();
        let cv = canvas.clone();
        let ov = overlay.clone();
        let sc = sel.clone();
        move || {
            if let Some(e) = ee.borrow_mut().take() {
                *ta.borrow_mut() = false;
                let new_text = e.get_text().unwrap_or_default();
                if let Some((r, c)) = *sc.borrow() {
                    if r < te.borrow().len()
                        && c < te.borrow()[r].len()
                    {
                        te.borrow_mut()[r][c] = new_text.clone();
                    }
                }
                fe.set_text(&new_text);
                ov.remove(&e);
                cv.queue_redraw();
            }
        }
    };

    let refresh_selection = {
        let tr = texts.clone();
        let fr = formula_entry.clone();
        let sr = sel.clone();
        let cv = canvas.clone();
        move || {
            if let Some((r, c)) = *sr.borrow() {
                if r < tr.borrow().len() && c < tr.borrow()[r].len() {
                    fr.set_text(&tr.borrow()[r][c]);
                }
            }
            cv.queue_redraw();
        }
    };

    let start_edit: Rc<dyn Fn(usize, usize)> = {
        let te = texts.clone();
        let fe = fmts.clone();
        let ee = editing_entry.clone();
        let ta = text_input_active.clone();
        let ov = overlay.clone();
        let rh = row_heights.clone();
        let commit = commit_edit.clone();
        let se = sel.clone();
        let refresh = refresh_selection.clone();
        let ae = app.clone();
        let self_ref: Rc<RefCell<Option<Rc<dyn Fn(usize, usize)>>>> =
            Rc::new(RefCell::new(None));
        let sr2 = self_ref.clone();
        let start: Rc<dyn Fn(usize, usize)> =
            Rc::new(move |r: usize, c: usize| {
                if ee.borrow().is_some() {
                    return;
                }
                if let Ok(entry) = ae.new_entry() {
                    *ta.borrow_mut() = true;
                    entry.set_text(&te.borrow()[r][c]);
                    let fmt = fe.borrow()[r][c].clone();
                    if fmt.0 && fmt.1 {
                        entry.add_class("cell-both");
                    } else if fmt.0 {
                        entry.add_class("cell-bold");
                    } else if fmt.1 {
                        entry.add_class("cell-italic");
                    }
                    match fmt.3.as_str() {
                        "#cc0000" => entry.add_class("cell-fg-red"),
                        "#0000cc" => entry.add_class("cell-fg-blue"),
                        "#006600" => entry.add_class("cell-fg-green"),
                        _ => {}
                    }
                    let rhb = rh.borrow();
                    let left = CHW as i32 + c as i32 * CELL_W;
                    let top = CELL_H + compute_row_y(&rhb, r);
                    drop(rhb);
                    ov.add_overlay(&entry);
                    ov.set_overlay_pass_through(&entry, false);
                    entry.set_margin_start(left);
                    entry.set_margin_top(top);
                    entry.set_halign(1);
                    entry.set_valign(1);
                    entry.set_visible(true);
                    ov.show_all();
                    entry.set_size_request(CELL_W, rh.borrow()[r]);

                    let c2 = commit.clone();
                    let s2 = se.clone();
                    let r2 = refresh.clone();
                    let sa = sr2.clone();
                    let _ = entry.connect_activate(move |_| {
                        c2();
                        let cur = *s2.borrow();
                        if let Some((_, _)) = cur {
                            let next = cur
                                .map(|(row, col)| (row + 1, col))
                                .filter(|(row, _)| *row < ROWS);
                            if let Some((nr, nc)) = next {
                                *s2.borrow_mut() = Some((nr, nc));
                                r2();
                                if let Some(s) =
                                    sa.borrow().as_ref().cloned()
                                {
                                    s(nr, nc);
                                }
                            }
                        }
                    });

                    entry.grab_focus();
                    *ee.borrow_mut() = Some(entry);
                }
            });
        *self_ref.borrow_mut() = Some(start.clone());
        start
    };

    // ── Draw callback ───────────────────────────────────────────────
    let td = texts.clone();
    let fd = fmts.clone();
    let sd = sel.clone();
    let ed = editing_entry.clone();
    canvas.set_draw_callback(Box::new(
        move |ctx: &mut dyn DrawContext, _w: i32, _h: i32| {
            ctx.clear(1.0, 1.0, 1.0, 1.0);

            // header areas
            ctx.fill_rect(0.0, 0.0, CHW, ch, 0.91, 0.91, 0.91, 1.0);
            ctx.fill_rect(
                CHW,
                0.0,
                COLS as f64 * cw,
                ch,
                0.8,
                0.8,
                0.8,
                1.0,
            );
            ctx.fill_rect(
                0.0,
                ch,
                CHW,
                ROWS as f64 * ch,
                0.8,
                0.8,
                0.8,
                1.0,
            );

            // grid lines
            for c in 0..=COLS {
                let x = CHW + c as f64 * cw;
                ctx.stroke_rect(
                    x, 0.0, 0.0, total_h, 0.7, 0.7, 0.7, 1.0, 0.5,
                );
            }
            for r in 0..=ROWS {
                let y = ch + r as f64 * ch;
                ctx.stroke_rect(
                    0.0, y, total_w, 0.0, 0.7, 0.7, 0.7, 1.0, 0.5,
                );
            }

            // column headers
            for c in 0..COLS {
                let lbl = col_label(c);
                let (xb, _, w, _) = ctx.text_extents_styled(
                    &lbl, "monospace", 12.0, 0, 0,
                );
                let x = CHW + c as f64 * cw + cw / 2.0 - xb - w / 2.0;
                ctx.draw_text(
                    x, ch / 2.0, &lbl, "monospace", 12.0, 0.0, 0.0,
                    0.0, 1.0,
                );
            }
            // row headers
            for r in 0..ROWS {
                let lbl = format!("{}", r + 1);
                let (xb, _, w, _) = ctx.text_extents_styled(
                    &lbl, "monospace", 12.0, 0, 0,
                );
                let x = CHW / 2.0 - xb - w / 2.0;
                ctx.draw_text(
                    x,
                    ch + r as f64 * ch + ch / 2.0,
                    &lbl,
                    "monospace",
                    12.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                );
            }

            // overflow computation
            let t = td.borrow();
            let f = fd.borrow();
            let mut overflow_end_col = vec![vec![0usize; COLS]; ROWS];
            for r in 0..ROWS {
                for bc in 0..COLS {
                    let text = &t[r][bc];
                    if text.is_empty() {
                        continue;
                    }
                    let cb = f[r][bc].0;
                    let ci = f[r][bc].1;
                    let (_, _, tw, _) = ctx.text_extents_styled(
                        text,
                        "monospace",
                        13.0,
                        if ci { 1 } else { 0 },
                        if cb { 1 } else { 0 },
                    );
                    let cx = CHW + bc as f64 * cw;
                    if cx + tw > cx + cw {
                        let mut lo = bc;
                        for oc in (bc + 1)..COLS {
                            if !t[r][oc].is_empty() {
                                break;
                            }
                            lo = oc;
                        }
                        overflow_end_col[r][bc] = lo;
                    }
                }
            }

            // redraw clean background
            ctx.clear(1.0, 1.0, 1.0, 1.0);
            ctx.fill_rect(0.0, 0.0, CHW, ch, 0.91, 0.91, 0.91, 1.0);
            ctx.fill_rect(
                CHW,
                0.0,
                COLS as f64 * cw,
                ch,
                0.8,
                0.8,
                0.8,
                1.0,
            );
            ctx.fill_rect(
                0.0,
                ch,
                CHW,
                ROWS as f64 * ch,
                0.8,
                0.8,
                0.8,
                1.0,
            );

            for r in 0..=ROWS {
                let y = ch + r as f64 * ch;
                ctx.stroke_rect(
                    0.0, y, total_w, 0.0, 0.7, 0.7, 0.7, 1.0, 0.5,
                );
            }
            for bc in 0..=COLS {
                let x = CHW + bc as f64 * cw;
                ctx.stroke_rect(
                    x, 0.0, 0.0, ch, 0.7, 0.7, 0.7, 1.0, 0.5,
                );
                for r in 0..ROWS {
                    let skip =
                        bc > 0 && overflow_end_col[r][bc - 1] >= bc;
                    if !skip {
                        let y1 = ch + r as f64 * ch;
                        let y2 = ch + (r + 1) as f64 * ch;
                        ctx.stroke_rect(
                            x,
                            y1,
                            0.0,
                            y2 - y1,
                            0.7,
                            0.7,
                            0.7,
                            1.0,
                            0.5,
                        );
                    }
                }
            }

            let editing = ed.borrow().is_some();
            let edit_target = if editing { *sd.borrow() } else { None };
            for r in 0..ROWS {
                for bc in 0..COLS {
                    let text = &t[r][bc];
                    if text.is_empty() {
                        continue;
                    }
                    if edit_target == Some((r, bc)) {
                        continue;
                    }
                    let (bold, italic, align, fg_hex, bg_hex) =
                        &f[r][bc];
                    let cx = CHW + bc as f64 * cw;
                    let cy = ch + r as f64 * ch;

                    if bg_hex != "#ffffff" {
                        let (br, bg, bb) = parse_hex(bg_hex);
                        ctx.fill_rect(
                            cx,
                            cy + 1.0,
                            cw,
                            ch - 2.0,
                            br,
                            bg,
                            bb,
                            1.0,
                        );
                    }

                    let (fr, fgg, fb) = if fg_hex != "#000000" {
                        parse_hex(fg_hex)
                    } else {
                        (0.0, 0.0, 0.0)
                    };
                    let slant = if *italic { 1 } else { 0 };
                    let weight = if *bold { 1 } else { 0 };
                    let (xb, _, tw, _) =
                        ctx.text_extents(text, "monospace", 13.0);
                    let pad = 4.0;
                    let tx = match *align {
                        0 => cx + pad - xb,
                        1 => cx + cw / 2.0 - xb - tw / 2.0,
                        _ => cx + cw - pad - xb - tw,
                    };
                    let ty = cy + ch / 2.0;

                    let last_oc = overflow_end_col[r][bc];
                    let text_right = tx + xb + tw + 2.0;
                    let clip_right = if last_oc > bc {
                        let mut limit = text_right;
                        for oc in (bc + 1)..COLS {
                            if !t[r][oc].is_empty() {
                                limit = limit
                                    .min(CHW + oc as f64 * cw);
                                break;
                            }
                        }
                        limit.min(total_w)
                    } else {
                        cx + cw
                    };

                    ctx.save();
                    ctx.clip(cx, cy, clip_right - cx, ch);
                    ctx.draw_text_styled(
                        tx, ty, text, "monospace", 13.0, fr, fgg, fb,
                        1.0, slant, weight,
                    );
                    ctx.restore();
                }
            }

            // selection highlight
            if let Some((sr, sc)) = *sd.borrow() {
                let sx = CHW + sc as f64 * cw;
                let sy = ch + sr as f64 * ch;
                ctx.fill_rect(
                    sx, sy, cw, ch, 0.83, 0.91, 1.0, 0.3,
                );
                ctx.stroke_rect(
                    sx + 0.5,
                    sy + 0.5,
                    cw - 1.0,
                    ch - 1.0,
                    0.1,
                    0.45,
                    0.91,
                    1.0,
                    2.0,
                );
            }
        },
    ));

    // ── Click handler ───────────────────────────────────────────────
    let sc = sel.clone();
    let fe = formula_entry.clone();
    let tc = texts.clone();
    let ec = editing_entry.clone();
    let cc = commit_edit.clone();
    let ste = start_edit.clone();
    let fc = fmts.clone();
    let rhc = row_heights.clone();
    let cvc = canvas.clone();
    canvas.on_click(Box::new(move |x: f64, y: f64| {
        let col = {
            let cx = x - CHW;
            if cx <= 0.0 {
                0
            } else {
                ((cx / cw) as usize).min(COLS - 1)
            }
        };
        let rhb = rhc.borrow();
        let grid_row = if y < ch {
            drop(rhb);
            0
        } else {
            let data_y = y - ch;
            let mut acc = 0.0f64;
            let mut ri = 0;
            for hi in rhb.iter() {
                if acc + *hi as f64 > data_y {
                    break;
                }
                acc += *hi as f64;
                ri += 1;
            }
            drop(rhb);
            (ri + 1).min(ROWS)
        };

        if grid_row == 0 && col < COLS {
            return;
        }
        if x < CHW && grid_row >= 1 && grid_row <= ROWS {
            return;
        }

        let data_row = grid_row.saturating_sub(1);
        if col < COLS && data_row < ROWS {
            cc();
            *sc.borrow_mut() = Some((data_row, col));
            let t = tc.borrow();
            let fmt = fc.borrow()[data_row][col].clone();
            if data_row < t.len() && col < t[data_row].len() {
                let text = &t[data_row][col];
                if fmt.0 {
                    fe.set_text(&format!("*{}*", text));
                } else if fmt.1 {
                    fe.set_text(&format!("/{}/", text));
                } else {
                    fe.set_text(text);
                }
            }
            drop(t);
            cvc.queue_redraw();
            if ec.borrow().is_none() {
                ste(data_row, col);
                if let Some(e) = ec.borrow().as_ref() {
                    e.set_text(&tc.borrow()[data_row][col]);
                }
            }
        }
    }));

    // ── Keyboard navigation ─────────────────────────────────────────
    let sk = sel.clone();
    let ek = editing_entry.clone();
    let tak = text_input_active.clone();
    let ck = commit_edit.clone();
    let stk = start_edit.clone();
    let rk = refresh_selection.clone();
    canvas.on_key(Box::new(move |keyval: u32| -> bool {
        if *tak.borrow() {
            return false;
        }
        if ek.borrow().is_some() {
            if keyval == 0xFF1B {
                let _ = ek.borrow_mut().take();
                return true;
            } else if keyval == 0xFF0D || keyval == 0xFF8D {
                ck();
                if let Some((r, c)) = sk
                    .borrow()
                    .map(|(r, c)| (r + 1, c))
                    .filter(|(r, _)| *r < ROWS)
                {
                    *sk.borrow_mut() = Some((r, c));
                    rk();
                    stk(r, c);
                }
                return true;
            }
            return false;
        }
        let mut coord = sk.borrow_mut();
        if let Some((r, c)) = *coord {
            match keyval {
                0xFF52 | 0xFE52 => {
                    if r > 0 {
                        *coord = Some((r - 1, c));
                    }
                }
                0xFF54 | 0xFE54 => {
                    if r + 1 < ROWS {
                        *coord = Some((r + 1, c));
                    }
                }
                0xFF51 | 0xFE51 => {
                    if c > 0 {
                        *coord = Some((r, c - 1));
                    }
                }
                0xFF53 | 0xFE53 => {
                    if c + 1 < COLS {
                        *coord = Some((r, c + 1));
                    }
                }
                0xFF0D | 0xFF8D => {
                    drop(coord);
                    ck();
                    stk(r, c);
                    return true;
                }
                _ => {
                    if keyval >= 0x20 && keyval <= 0x7E {
                        drop(coord);
                        ck();
                        stk(r, c);
                        if let Some(entry) = ek.borrow().as_ref() {
                            if let Some(ch) =
                                std::char::from_u32(keyval)
                            {
                                entry.set_text(&ch.to_string());
                            }
                        }
                        return true;
                    }
                }
            }
        }
        drop(coord);
        rk();
        false
    }));

    // ── Formatting toolbar buttons ──────────────────────────────────
    let eef = editing_entry.clone();
    let sf = sel.clone();
    let ff = fmts.clone();
    let cvf = canvas.clone();
    let _ = bold_btn.on_click(move || {
        let coord = *sf.borrow();
        if let Some((r, c)) = coord {
            let mut f = ff.borrow_mut();
            if r < f.len() && c < f[r].len() {
                f[r][c].0 = !f[r][c].0;
                cvf.queue_redraw();
            }
        }
        if let Some((r, c)) = coord {
            if let Some(ee) = eef.borrow().as_ref() {
                apply_format(ee, r, c, &ff);
            }
        }
    });

    let eei = editing_entry.clone();
    let si = sel.clone();
    let fi = fmts.clone();
    let cvi = canvas.clone();
    let _ = italic_btn.on_click(move || {
        let coord = *si.borrow();
        if let Some((r, c)) = coord {
            let mut f = fi.borrow_mut();
            if r < f.len() && c < f[r].len() {
                f[r][c].1 = !f[r][c].1;
                cvi.queue_redraw();
            }
        }
        if let Some((r, c)) = coord {
            if let Some(ee) = eei.borrow().as_ref() {
                apply_format(ee, r, c, &fi);
            }
        }
    });

    let _ = al_l.on_click({
        let sa = sel.clone();
        let fa = fmts.clone();
        let cva = canvas.clone();
        let eea = editing_entry.clone();
        move || {
            let coord = *sa.borrow();
            if let Some((r, c)) = coord {
                fa.borrow_mut()[r][c].2 = 0;
                cva.queue_redraw();
            }
            if let Some((r, c)) = coord {
                if let Some(ee) = eea.borrow().as_ref() {
                    apply_format(ee, r, c, &fa);
                }
            }
        }
    });
    let _ = al_c.on_click({
        let sac = sel.clone();
        let fac = fmts.clone();
        let cvac = canvas.clone();
        let eeac = editing_entry.clone();
        move || {
            let coord = *sac.borrow();
            if let Some((r, c)) = coord {
                fac.borrow_mut()[r][c].2 = 1;
                cvac.queue_redraw();
            }
            if let Some((r, c)) = coord {
                if let Some(ee) = eeac.borrow().as_ref() {
                    apply_format(ee, r, c, &fac);
                }
            }
        }
    });
    let _ = al_r.on_click({
        let sar = sel.clone();
        let far = fmts.clone();
        let cvar = canvas.clone();
        let eear = editing_entry.clone();
        move || {
            let coord = *sar.borrow();
            if let Some((r, c)) = coord {
                far.borrow_mut()[r][c].2 = 2;
                cvar.queue_redraw();
            }
            if let Some((r, c)) = coord {
                if let Some(ee) = eear.borrow().as_ref() {
                    apply_format(ee, r, c, &far);
                }
            }
        }
    });

    let eeh = editing_entry.clone();
    let sh = sel.clone();
    let fh = fmts.clone();
    let cvh = canvas.clone();
    let _ = hl_btn.on_click(move || {
        let coord = *sh.borrow();
        if let Some((r, c)) = coord {
            let mut f = fh.borrow_mut();
            if r < f.len() && c < f[r].len() {
                let bg = f[r][c].4.clone();
                f[r][c].4 = if bg == "#ffff00" {
                    "#88ff88".into()
                } else if bg == "#88ff88" {
                    "#ffffff".into()
                } else {
                    "#ffff00".into()
                };
                cvh.queue_redraw();
            }
        }
        if let Some((r, c)) = coord {
            if let Some(ee) = eeh.borrow().as_ref() {
                apply_format(ee, r, c, &fh);
            }
        }
    });

    let eefg = editing_entry.clone();
    let sfg = sel.clone();
    let ffg = fmts.clone();
    let cvfg = canvas.clone();
    let _ = fg_btn.on_click(move || {
        let coord = *sfg.borrow();
        if let Some((r, c)) = coord {
            let mut f = ffg.borrow_mut();
            if r < f.len() && c < f[r].len() {
                let fg = f[r][c].3.clone();
                f[r][c].3 = if fg == "#000000" {
                    "#cc0000".into()
                } else if fg == "#cc0000" {
                    "#0000cc".into()
                } else if fg == "#0000cc" {
                    "#006600".into()
                } else {
                    "#000000".into()
                };
                cvfg.queue_redraw();
            }
        }
        if let Some((r, c)) = coord {
            if let Some(ee) = eefg.borrow().as_ref() {
                apply_format(ee, r, c, &ffg);
            }
        }
    });

    // ── File operations (toolbar) ───────────────────────────────────
    let ao = app.clone();
    let to = texts.clone();
    let cvo = canvas.clone();
    let _ = open_btn.on_click(move || {
        if let Ok(Some(path)) = ao.open_file("Open spreadsheet") {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(mut t) = to.try_borrow_mut() {
                    for row in t.iter_mut() {
                        for cell in row.iter_mut() {
                            *cell = String::new();
                        }
                    }
                    for (i, line) in data.lines().enumerate() {
                        if i >= ROWS {
                            break;
                        }
                        for (j, val) in
                            line.split('\t').enumerate()
                        {
                            if j >= COLS {
                                break;
                            }
                            t[i][j] = val.to_string();
                        }
                    }
                }
                cvo.queue_redraw();
            }
        }
    });

    let as_ = app.clone();
    let ts = texts.clone();
    let _ = save_btn.on_click(move || {
        if let Ok(Some(path)) = as_.save_file("Save spreadsheet") {
            let mut out = String::new();
            if let Ok(t) = ts.try_borrow() {
                for row in t.iter() {
                    for (j, val) in row.iter().enumerate() {
                        if j > 0 {
                            out.push('\t');
                        }
                        out.push_str(val);
                    }
                    out.push('\n');
                }
            }
            let _ = std::fs::write(&path, &out);
        }
    });

    let _ = quit_btn.on_click(|| std::process::exit(0));

    // ── SimpleAction: wire menu items to callbacks ──────────────────
    let sa_new = app.new_simple_action("app.new")?;
    let sa_open = app.new_simple_action("app.open")?;
    let sa_save = app.new_simple_action("app.save")?;
    let sa_quit = app.new_simple_action("app.quit")?;
    let sa_find = app.new_simple_action("app.find")?;
    let sa_clear = app.new_simple_action("app.clear")?;
    let sa_about = app.new_simple_action("app.about")?;

    // Helper to connect a simple action regardless of backend.
    // GTK/NWG/WASM use connect_activate(FnMut(*mut c_void));
    // Pancurses/Zork use on_activate(FnMut()).
    macro_rules! connect_action {
        ($sa:expr, $cb:expr) => {
            #[cfg(any(
                feature = "pancurses",
                feature = "zork"
            ))]
            $sa.on_activate($cb)?;
            #[cfg(not(any(
                feature = "pancurses",
                feature = "zork"
            )))]
            {
                let cb = $cb;
                $sa.connect_activate(move |_| cb())?;
            }
        };
    }

    let tn = texts.clone();
    let cn = canvas.clone();
    let fn_ = formula_entry.clone();
    connect_action!(sa_new, {
        move || {
            if let Ok(mut t) = tn.try_borrow_mut() {
                for row in t.iter_mut() {
                    for cell in row.iter_mut() {
                        *cell = String::new();
                    }
                }
            }
            fn_.set_text("");
            cn.queue_redraw();
        }
    });

    let ao2 = app.clone();
    let to2 = texts.clone();
    let cvo2 = canvas.clone();
    connect_action!(sa_open, {
        move || {
            if let Ok(Some(path)) = ao2.open_file("Open spreadsheet")
            {
                if let Ok(data) =
                    std::fs::read_to_string(&path)
                {
                    if let Ok(mut t) = to2.try_borrow_mut() {
                        for row in t.iter_mut() {
                            for cell in row.iter_mut() {
                                *cell = String::new();
                            }
                        }
                        for (i, line) in
                            data.lines().enumerate()
                        {
                            if i >= ROWS {
                                break;
                            }
                            for (j, val) in
                                line.split('\t').enumerate()
                            {
                                if j >= COLS {
                                    break;
                                }
                                t[i][j] = val.to_string();
                            }
                        }
                    }
                    cvo2.queue_redraw();
                }
            }
        }
    });

    let as2 = app.clone();
    let ts2 = texts.clone();
    connect_action!(sa_save, {
        move || {
            if let Ok(Some(path)) =
                as2.save_file("Save spreadsheet")
            {
                let mut out = String::new();
                if let Ok(t) = ts2.try_borrow() {
                    for row in t.iter() {
                        for (j, val) in
                            row.iter().enumerate()
                        {
                            if j > 0 {
                                out.push('\t');
                            }
                            out.push_str(val);
                        }
                        out.push('\n');
                    }
                }
                let _ = std::fs::write(&path, &out);
            }
        }
    });

    connect_action!(sa_quit, { || std::process::exit(0) });

    let tc2 = texts.clone();
    let sc2 = sel.clone();
    let fc2 = formula_entry.clone();
    let cvc2 = canvas.clone();
    connect_action!(sa_clear, {
        move || {
            if let Some((r, c)) = *sc2.borrow() {
                if r < tc2.borrow().len()
                    && c < tc2.borrow()[r].len()
                {
                    tc2.borrow_mut()[r][c] = String::new();
                    fc2.set_text("");
                    cvc2.queue_redraw();
                }
            }
        }
    });

    // ── Find & Replace dialog ───────────────────────────────────────
    let af = app.clone();
    let tff = texts.clone();
    let cff = canvas.clone();
    connect_action!(sa_find, {
        move || {
            if let Ok(dialog) = af.new_dialog() {
                dialog.set_title("Find && Replace");
                dialog.set_default_size(350, 200);

                if let Ok(mut dvbox) =
                    af.new_box(Orientation::Vertical, 4)
                {
                    if let Ok(find_entry) = af.new_entry() {
                        find_entry.set_width_chars(30);
                        dvbox.append(&find_entry);

                        if let Ok(replace_entry) =
                            af.new_entry()
                        {
                            replace_entry
                                .set_width_chars(30);
                            dvbox.append(&replace_entry);

                            let tfr = tff.clone();
                            let cvr = cff.clone();
                            let fe2 = find_entry.clone();
                            let re2 = replace_entry.clone();
                            dialog.connect_response(
                                move |response| {
                                    if response == 1 {
                                        let search = fe2
                                            .get_text()
                                            .unwrap_or_default();
                                        let replace = re2
                                            .get_text()
                                            .unwrap_or_default();
                                        if !search
                                            .is_empty()
                                        {
                                            if let Ok(
                                                mut t,
                                            ) = tfr
                                                .try_borrow_mut()
                                            {
                                                for row in t
                                                    .iter_mut()
                                                {
                                                    for cell in row
                                                        .iter_mut()
                                                    {
                                                        if cell
                                                            .contains(
                                                            &search,
                                                        )
                                                        {
                                                            *cell =
                                                                cell
                                                                .replace(
                                                                &search,
                                                                &replace,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            cvr
                                            .queue_redraw();
                                        }
                                    }
                                },
                            )
                            .ok();
                        }
                    }

                    dialog.append_content_area(&dvbox);
                    dialog.add_button("Cancel", 0);
                    dialog.add_button("Replace All", 1);
                    dialog.present();
                }
            }
        }
    });

    // ── About dialog ────────────────────────────────────────────────
    let aa = app.clone();
    connect_action!(sa_about, {
        move || {
            if let Ok(dialog) = aa.new_dialog() {
                dialog.set_title("About App Demo");
                dialog.set_default_size(320, 180);

                if let Ok(mut dvbox) =
                    aa.new_box(Orientation::Vertical, 8)
                {
                    if let Ok(title_lbl) = aa.new_label(
                        "App Demo v0.1",
                    ) {
                        dvbox.append(&title_lbl);
                    }
                    if let Ok(desc_lbl) = aa.new_label(
                        "Portable demo combining\ndialog, topmenu, and x-sheet",
                    ) {
                        dvbox.append(&desc_lbl);
                    }
                    if let Ok(tech_lbl) = aa.new_label(
                        "Built with rustxwidgets\nCross-platform GUI toolkit",
                    ) {
                        dvbox.append(&tech_lbl);
                    }

                    dialog.append_content_area(&dvbox);
                    dialog.add_button("Close", 0);
                    dialog.connect_response(|_| {}).ok();
                    dialog.present();
                }
            }
        }
    });

    // ── Assemble and run ────────────────────────────────────────────
    vbox.append(&scrolled);
    vbox.set_child_vexpand(&scrolled, true);
    vbox.set_child_hexpand(&scrolled, true);
    win.set_child_box(&vbox);
    win.present();

    println!("=== App Demo: Spreadsheet with Menus and Dialogs ===");
    println!("File menu: New  Open  Save As  Quit");
    println!("Edit menu: Find & Replace  Clear Cell");
    println!("Help menu: About App Demo");
    println!("Click a cell to select and edit. Arrow keys to navigate.");

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
