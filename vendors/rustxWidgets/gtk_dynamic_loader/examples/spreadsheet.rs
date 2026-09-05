#[cfg(unix)]
use gtk_dynamic_loader::{Application, Grid, Button, Entry, Loader, Orientation, Window, Label, BoxWidget, measure_text_px, connect_signal_bool, connect_signal_param};
use std::cell::RefCell;
use std::rc::Rc;
use std::fs::File;
use std::io::{Write, Read};

#[cfg(unix)]
fn update_overflow(grid: &Vec<Vec<Entry>>, loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, per_cell_px: i32) {
    use std::collections::HashMap;
    let mut measure_cache: HashMap<(usize, String), i32> = HashMap::new();

    for row in grid.iter() {
        let cols = row.len();
        let mut orig: Vec<String> = row.iter().map(|e| e.get_text().unwrap_or_default()).collect();

        let mut i = 0;
        while i < cols {
            let s = orig[i].clone();
            if s.is_empty() { i += 1; continue; }
            let widget_ptr = *row[i].as_ref();
            let w = if let Some(&v) = measure_cache.get(&(widget_ptr as usize, s.clone())) { v }
                    else {
                        let m = unsafe { measure_text_px(loader, Some(widget_ptr), &s) };
                        measure_cache.insert((widget_ptr as usize, s.clone()), m);
                        m
                    };
            if w <= per_cell_px { i += 1; continue; }
            if i + 1 < cols && !orig[i + 1].is_empty() { i += 1; continue; }

            let mut targets = Vec::new();
            let mut j = i;
            while j < cols && (j == i || orig[j].is_empty()) {
                targets.push(j);
                j += 1;
            }

            let mut remaining = s.clone();
            remaining = remaining.trim_start().to_string();

            for &t in targets.iter() {
                if remaining.is_empty() { orig[t] = String::new(); continue; }
                let widget_ptr_t = *row[t].as_ref();

                let full_key = (widget_ptr_t as usize, remaining.clone());
                if let Some(&cached_full) = measure_cache.get(&full_key) {
                    if cached_full <= per_cell_px { orig[t] = remaining.clone(); remaining.clear(); continue; }
                } else {
                    let measured_full = unsafe { measure_text_px(loader, Some(widget_ptr_t), &remaining) };
                    measure_cache.insert(full_key.clone(), measured_full);
                    if measured_full <= per_cell_px { orig[t] = remaining.clone(); remaining.clear(); continue; }
                }

                let chars: Vec<char> = remaining.chars().collect();
                let mut low: usize = 0;
                let mut high: usize = chars.len();
                while low < high {
                    let mid = (low + high + 1) / 2;
                    let prefix: String = chars.iter().take(mid).collect();
                    let key = (widget_ptr_t as usize, prefix.clone());
                    let measured = if let Some(&cached) = measure_cache.get(&key) { cached }
                                   else { let m = unsafe { measure_text_px(loader, Some(widget_ptr_t), &prefix) }; measure_cache.insert(key.clone(), m); m };
                    if measured <= per_cell_px { low = mid; } else { if mid == 0 { break; } high = mid - 1; }
                }
                let max_chars = if low == 0 { 1 } else { low };

                let mut break_pos: Option<usize> = None;
                for k in (0..max_chars).rev() {
                    if chars[k].is_whitespace() { break_pos = Some(k); break; }
                }
                let take = match break_pos {
                    Some(bp) if bp > 0 => bp,
                    _ => max_chars,
                };

                let mut chunk: String = chars.iter().take(take).collect();
                chunk = chunk.trim_end().to_string();
                orig[t] = chunk.clone();
                let rem: String = chars.iter().skip(take).collect();
                remaining = rem.trim_start().to_string();
            }

            i = j;
        }

        for (entry, txt) in row.iter().zip(orig.iter()) {
            entry.set_text(txt);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(unix))]
    {
        eprintln!("skipped (not Unix)");
        return Ok(());
    }

    #[cfg(unix)]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--prefer-gtk3" || a == "-3") {
            std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1");
        }

        let loader = Loader::new()?;

        const ROWS: usize = 6;
        const COLS: usize = 6;

        let app = Application::new(loader.clone(), Some("org.example.SpreadsheetDemo"))?;

        let win = Window::new(loader.clone())?;
        win.set_title("Spreadsheet-like overflow demo");

        let grid_widget = Grid::new(loader.clone())?;
        for c in 0..COLS {
            let header = Label::new(loader.clone(), &format!("{}", (b'A' + (c as u8)) as char))?;
            header.set_markup(&format!("<span weight=bold>{}</span>", (b'A' + (c as u8)) as char));
            grid_widget.attach(&header, (c+1) as i32, 0, 1, 1);
        }
        let mut grid_cells: Vec<Vec<Entry>> = Vec::new();
        for r in 0..ROWS {
            let mut row: Vec<Entry> = Vec::new();
            let heading = Label::new(loader.clone(), &format!("{}", r+1))?;
            heading.set_markup(&format!("<span foreground=\"#666666\">{}</span>", r+1));
            grid_widget.attach(&heading, 0, (r+1) as i32, 1, 1);
            for c in 0..COLS {
                let e = Entry::new(loader.clone())?;
                e.set_width_chars(12);
                e.set_size_request(120, 28);
                grid_widget.attach(&e, (c+1) as i32, (r+1) as i32, 1, 1);
                row.push(e);
            }
            grid_cells.push(row);
        }

        let grid_cells = std::rc::Rc::new(grid_cells);

        grid_cells[0][0].set_text("Short");
        grid_cells[1][0].set_text("VeryLongHeaderThatOverflows");
        grid_cells[2][0].set_text("CellWithAVeryLongWordThatWillSpanMultipleCells");
        grid_cells[3][0].set_text("NoOverflowHereBecauseNextIsUsed");
        grid_cells[3][1].set_text("X");

        for r in 0..grid_cells.len() {
            for c in 0..grid_cells[r].len() {
                if let Some(s) = grid_cells[r][c].get_text() {
                    if s == "TRUE" {
                        grid_cells[r][c].add_class("trueval");
                    } else if s.starts_with('-') {
                        grid_cells[r][c].add_class("negative");
                    }
                }
                let e_handle = grid_cells[r][c].clone();
                grid_cells[r][c].connect_changed(move || {
                    if let Some(s2) = e_handle.get_text() {
                        if s2 == "TRUE" { e_handle.add_class("trueval"); } else { e_handle.remove_class("trueval"); }
                        if s2.starts_with('-') { e_handle.add_class("negative"); } else { e_handle.remove_class("negative"); }
                    }
                }).ok();

                let grid_for_key = grid_cells.clone();
                let loader_for_key = loader.clone();
                let r_idx = r; let c_idx = c;
                    let instance = *grid_for_key[r_idx][c_idx].as_ref();
                    let syms_arc = loader_for_key.symbols.clone();
                    let syms_ref = syms_arc.as_ref();
                    let syms_for_closure = syms_arc.clone();
                    let left_k = syms_for_closure.gdk_keyval_from_name.map(|f| unsafe { 
                        let n = std::ffi::CString::new("Left").unwrap(); f(n.as_ptr())
                    }).unwrap_or(65361);
                    let right_k = syms_for_closure.gdk_keyval_from_name.map(|f| unsafe { 
                        let n = std::ffi::CString::new("Right").unwrap(); f(n.as_ptr())
                    }).unwrap_or(65363);
                    let up_k = syms_for_closure.gdk_keyval_from_name.map(|f| unsafe { 
                        let n = std::ffi::CString::new("Up").unwrap(); f(n.as_ptr())
                    }).unwrap_or(65362);
                    let down_k = syms_for_closure.gdk_keyval_from_name.map(|f| unsafe { 
                        let n = std::ffi::CString::new("Down").unwrap(); f(n.as_ptr())
                    }).unwrap_or(65364);
                    let _ = unsafe { connect_signal_bool(syms_ref, instance, "key-press-event", Box::new(move |ev: *mut std::os::raw::c_void| -> i32 {
                        if let Some(get_keyval) = syms_for_closure.gdk_event_get_keyval {
                            let mut kv = 0u32;
                            unsafe { get_keyval(ev, &mut kv); }
                            match kv {
                                k if k == left_k => { if c_idx > 0 { grid_for_key[r_idx][c_idx-1].grab_focus(); return 1; } }
                                k if k == right_k => { if c_idx + 1 < grid_for_key[r_idx].len() { grid_for_key[r_idx][c_idx+1].grab_focus(); return 1; } }
                                k if k == up_k => { if r_idx > 0 { grid_for_key[r_idx-1][c_idx].grab_focus(); return 1; } }
                                k if k == down_k => { if r_idx + 1 < grid_for_key.len() { grid_for_key[r_idx+1][c_idx].grab_focus(); return 1; } }
                                _ => {}
                            }
                        }
                        0
                    })) }.ok();
            }
        }

        let controls = Grid::new(loader.clone())?;
        let open_btn = Button::with_label(loader.clone(), "Open")?;
        let save_btn = Button::with_label(loader.clone(), "Save As")?;
        let quit_btn = Button::with_label(loader.clone(), "Quit")?;
        controls.attach(&open_btn, 0, 0, 1, 1);
        controls.attach(&save_btn, 1, 0, 1, 1);
        controls.attach(&quit_btn, 2, 0, 1, 1);

        update_overflow(&grid_cells, &loader, 120);

        if let (Some(provider_ctor), Some(load_from_data), Some(add_provider), Some(get_ctx_sym)) = (
            loader.symbols.gtk_css_provider_new,
            loader.symbols.gtk_css_provider_load_from_data,
            loader.symbols.gtk_style_context_add_provider,
            loader.symbols.gtk_widget_get_style_context,
        ) {
            unsafe {
                let provider = provider_ctor();
                let css = b"\n.entry { padding: 4px; }\n.cell-divider { border: 0.5px solid #e0e0e0; }\n.row-divider { border-bottom: 0.5px solid #e0e0e0; }\n.negative { color: #cc0000; }\n.trueval { font-weight: bold; }\n.header { font-weight: bold; }\n.row-marker { color: #666666; }\n";
                let _ = load_from_data(provider, css.as_ptr() as *const i8, css.len() as isize, std::ptr::null_mut());
                for r in 0..grid_cells.len() {
                    for c in 0..grid_cells[r].len() {
                        let e = &grid_cells[r][c];
                        let ctx = get_ctx_sym(*e.as_ref());
                        if !ctx.is_null() { add_provider(ctx, provider, 800); }
                    }
                }
                for _c in 0..COLS {
                    if let Ok(h) = Label::new(loader.clone(), "") {
                        let ctx = get_ctx_sym(*h.as_ref());
                        if !ctx.is_null() { add_provider(ctx, provider, 800); }
                    }
                }
            }
        }

        let toggle = Rc::new(RefCell::new(false));
        let g1 = Rc::new(grid_cells);
        let t1 = toggle.clone();
        let loader_for_rand = loader.clone();
        let rand_btn = Button::with_label(loader.clone(), "Toggle Random Long Text")?;
        controls.attach(&rand_btn, 3, 0, 1, 1);
        let g1_for_rand = g1.clone();
        rand_btn.connect_clicked(move || {
            let long1 = "RandomLongStringFooBarBazQuxQuuxCorgeGrault";
            let long2 = "AnotherSuperLongValueThatShouldWrapToNextCellIfEmpty";
            let cur = *t1.borrow();
            let pick = if cur { long1 } else { long2 };
            g1_for_rand[0][2].set_text(pick);
            g1_for_rand[1][2].set_text(pick);
            *t1.borrow_mut() = !cur;
            update_overflow(&g1_for_rand, &loader_for_rand, 120);
        })?;

        quit_btn.connect_clicked(move || {
            std::process::exit(0);
        })?;

        let gopen = g1.clone();
        let loader_for_open = loader.clone();
        open_btn.connect_clicked(move || {
            let syms = &loader_for_open.symbols;
            if let (Some(chooser_new), Some(native_run), Some(get_filename), Some(destroy_widget), Some(gfree)) = (
                syms.gtk_file_chooser_native_new,
                syms.gtk_native_dialog_run,
                syms.gtk_file_chooser_get_filename,
                syms.gtk_widget_destroy,
                syms.g_free,
            ) {
                unsafe {
                    let title = std::ffi::CString::new("Open TSV").unwrap();
                    let accept = std::ffi::CString::new("Open").unwrap();
                    let cancel = std::ffi::CString::new("Cancel").unwrap();
                    let native = chooser_new(title.as_ptr(), std::ptr::null_mut(), 0, accept.as_ptr(), cancel.as_ptr());
                    if native.is_null() { return; }
                    let res = native_run(native);
                    if res == 0 {
                        destroy_widget(native);
                    } else {
                        let fname = get_filename(native);
                        if !fname.is_null() {
                            let c = std::ffi::CStr::from_ptr(fname);
                            let path = c.to_string_lossy().into_owned();
                            gfree(fname as *mut std::os::raw::c_void);
                            destroy_widget(native);
                            if let Ok(mut f) = File::open(path) {
                                let mut s = String::new();
                                if f.read_to_string(&mut s).is_ok() {
                                    for (r, line) in s.lines().enumerate() {
                                        for (c, cell) in line.split('\t').enumerate() {
                                            if r < gopen.len() && c < gopen[r].len() {
                                                gopen[r][c].set_text(cell);
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            destroy_widget(native);
                        }
                    }
                }
            } else {
                if let Ok(mut f) = File::open("sample.tsv") {
                    let mut s = String::new();
                    if f.read_to_string(&mut s).is_ok() {
                        for (r, line) in s.lines().enumerate() {
                            for (c, cell) in line.split('\t').enumerate() {
                                if r < gopen.len() && c < gopen[r].len() {
                                    gopen[r][c].set_text(cell);
                                }
                            }
                        }
                    }
                }
            }
        })?;

        let gsave = g1.clone();
        let loader_for_save = loader.clone();
        save_btn.connect_clicked(move || {
            let syms = &loader_for_save.symbols;
            if let (Some(chooser_new), Some(native_run), Some(get_filename), Some(destroy_widget), Some(gfree)) = (
                syms.gtk_file_chooser_native_new,
                syms.gtk_native_dialog_run,
                syms.gtk_file_chooser_get_filename,
                syms.gtk_widget_destroy,
                syms.g_free,
            ) {
                unsafe {
                    let title = std::ffi::CString::new("Save TSV").unwrap();
                    let accept = std::ffi::CString::new("Save").unwrap();
                    let cancel = std::ffi::CString::new("Cancel").unwrap();
                    let native = chooser_new(title.as_ptr(), std::ptr::null_mut(), 0, accept.as_ptr(), cancel.as_ptr());
                    if native.is_null() { return; }
                    let res = native_run(native);
                    if res == 0 {
                        destroy_widget(native);
                    } else {
                        let fname = get_filename(native);
                        if !fname.is_null() {
                            let c = std::ffi::CStr::from_ptr(fname);
                            let path = c.to_string_lossy().into_owned();
                            gfree(fname as *mut std::os::raw::c_void);
                            destroy_widget(native);
                            if let Ok(mut f) = File::create(path) {
                                for r in 0..gsave.len() {
                                    let mut row_cells = Vec::new();
                                    for c in 0..gsave[r].len() {
                                        row_cells.push(gsave[r][c].get_text().unwrap_or_default());
                                    }
                                    let line = row_cells.join("\t");
                                    let _ = writeln!(f, "{}", line);
                                }
                            }
                        } else {
                            destroy_widget(native);
                        }
                    }
                }
            } else {
                if let Ok(mut f) = File::create("sample.tsv") {
                    for r in 0..gsave.len() {
                        let mut row_cells = Vec::new();
                        for c in 0..gsave[r].len() {
                            row_cells.push(gsave[r][c].get_text().unwrap_or_default());
                        }
                        let line = row_cells.join("\t");
                        let _ = writeln!(f, "{}", line);
                    }
                }
            }
        })?;

        let vbox = BoxWidget::new(loader.clone(), Orientation::Vertical, 6)?;
        vbox.append(&grid_widget);
        vbox.append(&controls);
        win.set_child(&vbox);
        win.present();

        if let (Some(gtk_application_new), Some(g_simple_action_new), Some(g_action_map_add_action), Some(g_menu_new), Some(g_menu_append), Some(g_application_set_app_menu)) = (
            loader.symbols.gtk_application_new,
            loader.symbols.g_simple_action_new,
            loader.symbols.g_action_map_add_action,
            loader.symbols.g_menu_new,
            loader.symbols.g_menu_append,
            loader.symbols.g_application_set_app_menu,
        ) {
            let id_c = std::ffi::CString::new("org.example.SpreadsheetDemo").unwrap();
            unsafe {
                let app_ptr2 = gtk_application_new(id_c.as_ptr(), 0);
                if !app_ptr2.is_null() {
                    let menu = g_menu_new();
                    let act_open = g_simple_action_new(std::ffi::CString::new("open").unwrap().as_ptr(), std::ptr::null_mut());
                    g_action_map_add_action(app_ptr2, act_open);
                    g_menu_append(menu, std::ffi::CString::new("Open").unwrap().as_ptr(), std::ffi::CString::new("app.open").unwrap().as_ptr());
                    let act_save = g_simple_action_new(std::ffi::CString::new("save").unwrap().as_ptr(), std::ptr::null_mut());
                    g_action_map_add_action(app_ptr2, act_save);
                    g_menu_append(menu, std::ffi::CString::new("Save").unwrap().as_ptr(), std::ffi::CString::new("app.save").unwrap().as_ptr());
                    let act_quit = g_simple_action_new(std::ffi::CString::new("quit").unwrap().as_ptr(), std::ptr::null_mut());
                    g_action_map_add_action(app_ptr2, act_quit);
                    g_menu_append(menu, std::ffi::CString::new("Quit").unwrap().as_ptr(), std::ffi::CString::new("app.quit").unwrap().as_ptr());
                    g_application_set_app_menu(app_ptr2, menu);

                    if !act_open.is_null() {
                        let grid_for_act = g1.clone();
                        let loader_for_act = loader.clone();
                        let syms_arc = loader_for_act.symbols.clone();
                        let syms_ref = syms_arc.as_ref();
                        let syms_closure = syms_arc.clone();
                        let _ = connect_signal_param(syms_ref, act_open, "activate", Box::new(move |_p| {
                            let syms = syms_closure.as_ref();
                            if let (Some(chooser_new), Some(native_run), Some(get_filename), Some(destroy_widget), Some(gfree)) = (
                                syms.gtk_file_chooser_native_new,
                                syms.gtk_native_dialog_run,
                                syms.gtk_file_chooser_get_filename,
                                syms.gtk_widget_destroy,
                                syms.g_free,
                            ) {
                                unsafe {
                                    let title = std::ffi::CString::new("Open TSV").unwrap();
                                    let accept = std::ffi::CString::new("Open").unwrap();
                                    let cancel = std::ffi::CString::new("Cancel").unwrap();
                                    let native = chooser_new(title.as_ptr(), std::ptr::null_mut(), 0, accept.as_ptr(), cancel.as_ptr());
                                    if native.is_null() { return; }
                                    let res = native_run(native);
                                    if res == 0 { destroy_widget(native); }
                                    else {
                                        let fname = get_filename(native);
                                        if !fname.is_null() {
                                            let c = std::ffi::CStr::from_ptr(fname);
                                            let path = c.to_string_lossy().into_owned();
                                            gfree(fname as *mut std::os::raw::c_void);
                                            destroy_widget(native);
                                            if let Ok(mut f) = File::open(path) {
                                                let mut s = String::new();
                                                if f.read_to_string(&mut s).is_ok() {
                                                    for (r, line) in s.lines().enumerate() {
                                                        for (c, cell) in line.split('\t').enumerate() {
                                                            if r < grid_for_act.len() && c < grid_for_act[r].len() {
                                                                grid_for_act[r][c].set_text(cell);
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        } else { destroy_widget(native); }
                                    }
                                }
                            } else {
                                if let Ok(mut f) = File::open("sample.tsv") {
                                    let mut s = String::new();
                                    if f.read_to_string(&mut s).is_ok() {
                                        for (r, line) in s.lines().enumerate() {
                                            for (c, cell) in line.split('\t').enumerate() {
                                                if r < grid_for_act.len() && c < grid_for_act[r].len() {
                                                    grid_for_act[r][c].set_text(cell);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }));
                    }

                    if !act_save.is_null() {
                        let grid_for_act = g1.clone();
                        let _ = connect_signal_param(loader.symbols.as_ref(), act_save, "activate", Box::new(move |_p| {
                            if let Ok(mut f) = File::create("sample.tsv") {
                                for r in 0..grid_for_act.len() {
                                    let mut row_cells = Vec::new();
                                    for c in 0..grid_for_act[r].len() {
                                        row_cells.push(grid_for_act[r][c].get_text().unwrap_or_default());
                                    }
                                    let line = row_cells.join("\t");
                                    let _ = writeln!(f, "{}", line);
                                }
                            }
                        }));
                    }

                    if !act_quit.is_null() {
                        let _ = connect_signal_param(loader.symbols.as_ref(), act_quit, "activate", Box::new(move |_p| {
                            std::process::exit(0);
                        }));
                    }
                }
            }
        }
        app.run()?;
    }
    Ok(())
}
