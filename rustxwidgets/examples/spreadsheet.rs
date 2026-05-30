use rustxwidgets::prelude::*;
use rustxwidgets::backends_gtk_adapter as gtk;
use std::rc::Rc;
use std::cell::RefCell;

fn compute_spans(grid: &Vec<Vec<gtk::Entry>>, loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, per_cell_px: i32) -> Vec<(usize, usize, usize, String)> {
    // Build a pure model and delegate to overflow::compute_spans_from_model so we can test it outside gtk
    let mut rows: Vec<Vec<(usize, String)>> = Vec::new();
    for row in grid.iter() {
        let mut r: Vec<(usize, String)> = Vec::new();
        for e in row.iter() {
            let key = *e.as_ref() as usize;
            let txt = e.get_text().unwrap_or_default();
            r.push((key, txt));
        }
        rows.push(r);
    }

    rustxwidgets::overflow::compute_spans_from_model(&rows, per_cell_px, |widget_key, s| {
    // Use gtk_dynamic_loader measurement when available
        // widget_key is the pointer value encoded as usize; convert back to pointer
        let ptr = widget_key as *mut std::os::raw::c_void;
        gtk_dynamic_loader::measure_text_px(loader, Some(ptr), s)
    })
}

/// Rebuild overlay labels for the given grid. This is a free function so signal handlers can call it
/// without needing complex Fn trait object gymnastics.
fn rebuild_overlays(overlay: &gtk_dynamic_loader::Overlay, overlay_labels: &Rc<RefCell<Vec<gtk_dynamic_loader::Label>>>, grid: &Rc<Vec<Vec<gtk::Entry>>>, loader: &std::sync::Arc<gtk_dynamic_loader::Loader>, per_cell_px: i32) {
    // Try to take mutable borrow to remove existing overlay labels. If unavailable (re-entrant), skip.
    if let Ok(mut existing) = overlay_labels.try_borrow_mut() {
        let drained = existing.drain(..).collect::<Vec<_>>();
        drop(existing);
        for lbl in drained.into_iter() {
            gtk_dynamic_loader::destroy_widget(loader, *lbl.as_ref());
        }
    } else {
        // Another borrow is active; don't try to mutate now to avoid panic.
        return;
    }

    // Build new labels locally first so we don't hold the RefCell across GTK calls.
    let spans = compute_spans(&*grid, loader, per_cell_px);
    let mut new_labels: Vec<gtk_dynamic_loader::Label> = Vec::new();
    for (r, start_col, len, text) in spans.into_iter() {
        if let Ok(lbl) = gtk_dynamic_loader::Label::new(loader.clone(), &text) {
            lbl.add_class("rwx-overlay");
            overlay.add_overlay(&lbl);
            overlay.set_overlay_pass_through(&lbl, true);

            // Position/size calculation using fixed cell sizes (match entries set_size_request above)
            let cell_w = 120; let cell_h = 28;
            let left = (start_col as i32) * cell_w + cell_w; // account for header column
            let top = (r as i32 + 1) * cell_h; // account for header row
            let width = (len as i32) * cell_w;
            gtk_dynamic_loader::widget_set_size_request(loader, *lbl.as_ref(), width, cell_h);
            gtk_dynamic_loader::widget_set_margin_start(loader, *lbl.as_ref(), left);
            gtk_dynamic_loader::widget_set_margin_top(loader, *lbl.as_ref(), top);

            new_labels.push(lbl);
        }
    }

    // Now attach new labels to the shared vector; if we can't borrow_mut, destroy and give up.
    if let Ok(mut existing) = overlay_labels.try_borrow_mut() {
        existing.extend(new_labels.into_iter());
    } else {
        for lbl in new_labels.into_iter() { gtk_dynamic_loader::destroy_widget(loader, *lbl.as_ref()); }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--prefer-gtk3" || a == "-3") { std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1"); }
    // Use the App-owned loader (backend loader) so symbols are shared.
    let loader = match rustxwidgets::backends::gtk::loader() {
        Some(l) => l,
        None => {
            // If backend not initialized yet, initialize via App::init which sets the loader.
            let _ = App::init()?;
            rustxwidgets::backends::gtk::loader().expect("GTK loader not initialized after App::init")
        }
    };

    const ROWS: usize = 6; const COLS: usize = 6;
    let app = App::init()?;
    let win = app.create_window()?; win.set_title("Spreadsheet-like overflow demo");

    let grid_widget = gtk::create_grid()?;
    // Wrap grid in an overlay so we can draw an overlay label that spans merged areas.
    let overlay = gtk_dynamic_loader::Overlay::new(rustxwidgets::backends::gtk::loader().expect("loader"))?;
    overlay.add_main_child(&grid_widget);
    for c in 0..COLS { let header = app.create_label(&format!("{}", (b'A' + (c as u8)) as char))?; header.set_text(&format!("{}", (b'A' + (c as u8)) as char)); grid_widget.attach(&header, (c+1) as i32, 0, 1, 1); }

    let mut grid_cells: Vec<Vec<gtk::Entry>> = Vec::new();
    for r in 0..ROWS {
        let mut row: Vec<gtk::Entry> = Vec::new();
        let heading = app.create_label(&format!("{}", r+1))?; heading.set_text(&format!("{}", r+1)); grid_widget.attach(&heading, 0, (r+1) as i32, 1, 1);
        for _c in 0..COLS { let e = gtk::create_entry()?; e.set_width_chars(12); e.set_size_request(120, 28); grid_widget.attach(&e, (_c+1) as i32, (r+1) as i32, 1, 1); row.push(e); }
        grid_cells.push(row);
    }

    let grid_cells = Rc::new(grid_cells);

    // Create overlay labels container early so signal handlers can capture it
    let overlay_labels: Rc<RefCell<Vec<gtk_dynamic_loader::Label>>> = Rc::new(RefCell::new(Vec::new()));
    grid_cells[0][0].set_text("Short");
    grid_cells[1][0].set_text("VeryLongHeaderThatOverflows");
    grid_cells[2][0].set_text("CellWithAVeryLongWordThatWillSpanMultipleCells");
    grid_cells[3][0].set_text("NoOverflowHereBecauseNextIsUsed"); grid_cells[3][1].set_text("X");

    for r in 0..grid_cells.len() {
        for c in 0..grid_cells[r].len() {
            if let Some(s) = grid_cells[r][c].get_text() {
                if s == "TRUE" { grid_cells[r][c].add_class("trueval"); }
                else if s.starts_with('-') { grid_cells[r][c].add_class("negative"); }
            }

            // Keep the simple class-updating on change (do not recompute overflow while editing).
            let e_handle = grid_cells[r][c].clone();
            let e_handle_for_focus = e_handle.clone();
            grid_cells[r][c].connect_changed(move || {
                if let Some(s2) = e_handle.get_text() {
                    if s2 == "TRUE" { e_handle.add_class("trueval"); } else { e_handle.remove_class("trueval"); }
                    if s2.starts_with('-') { e_handle.add_class("negative"); } else { e_handle.remove_class("negative"); }
                }
            }).ok();

            // When the user finishes editing (focus-out), recompute overflow for the grid.
            let grid_for_update = grid_cells.clone();
            let loader_for_update = loader.clone();
            // connect focus-out-event via low-level signal helper; clone symbols Arc first so
            // we don't borrow loader_for_update while moving it into the closure below
            let syms_arc = loader_for_update.symbols.clone();
            let syms_ref = syms_arc.as_ref();
            // connect the handler; closure will own loader_for_update
            let instance = *e_handle_for_focus.as_ref();
            let overlay_for_rebuild = overlay.clone();
            let overlay_labels_for_rebuild = overlay_labels.clone();
            let grid_for_rebuild = grid_for_update.clone();
            let loader_for_rebuild = loader_for_update.clone();
            let _ = unsafe { gtk_dynamic_loader::connect_signal_bool(syms_ref, instance, "focus-out-event", Box::new(move |_ev: *mut std::os::raw::c_void| -> i32 {
                // rebuild overlays when editing is finished
                rebuild_overlays(&overlay_for_rebuild, &overlay_labels_for_rebuild, &grid_for_rebuild, &loader_for_rebuild, 120);
                // show overlays again (collect first to avoid re-borrowing while iterating if rebuild mutates)
                let visible_now = overlay_labels_for_rebuild.borrow().iter().map(|l| l.clone()).collect::<Vec<_>>();
                for lbl in visible_now.iter() { lbl.set_visible(true); }
                0
            })) }.ok();

            // Also hide overlays when editing begins (focus-in)
            let overlay_labels_for_hide = overlay_labels.clone();
            let _loader_for_hide = loader.clone();
            let instance2 = instance;
            let _ = unsafe { gtk_dynamic_loader::connect_signal_bool(syms_ref, instance2, "focus-in-event", Box::new(move |_ev: *mut std::os::raw::c_void| -> i32 {
                // hide all overlay labels
                for lbl in overlay_labels_for_hide.borrow().iter() {
                    lbl.set_visible(false);
                }
                0
            })) }.ok();
        }
    }

    // Create overlay labels for spans and manage visibility when editing.
    // (overlay_labels already created above)

    // Apply base CSS for overlay labels
    if let Some(loader2) = rustxwidgets::backends::gtk::loader() {
        let css = r#"
        label.rwx-overlay { background-color: transparent; padding: 2px; }
        "#;
        if let Some(provider) = gtk_dynamic_loader::create_css_provider(&loader2, css) {
            gtk_dynamic_loader::add_provider_to_widget(&loader2, *overlay.as_ref(), provider, 600);
        }
    }

    // Helper to refresh overlays from computed spans
    let overlay_clone = overlay.clone();
    let grid_clone = grid_cells.clone();
    let loader_clone = loader.clone();
    let overlay_labels_ref = overlay_labels.clone();
    let refresh_overlays = move || {
        // destroy existing overlay labels (drain while holding borrow then drop it)
        let drained = {
            let mut old = overlay_labels_ref.borrow_mut();
            old.drain(..).collect::<Vec<_>>()
        };
        for lbl in drained.into_iter() {
            gtk_dynamic_loader::destroy_widget(&loader_clone, *lbl.as_ref());
        }

        let spans = compute_spans(&grid_clone, &loader_clone, 120);
        for (r, start_col, len, text) in spans.into_iter() {
            // create a label and position it by setting margins/size on the overlay
            if let Ok(lbl) = gtk_dynamic_loader::Label::new(loader_clone.clone(), &text) {
                lbl.add_class("rwx-overlay");
                // Add as overlay child and mark pass-through so clicks reach entries
                overlay_clone.add_overlay(&lbl);
                overlay_clone.set_overlay_pass_through(&lbl, true);

                // Position/size calculation using fixed cell sizes (match entries set_size_request above)
                let cell_w = 120; let cell_h = 28;
                let left = (start_col as i32) * cell_w + cell_w; // account for header column
                let top = (r as i32 + 1) * cell_h; // account for header row
                let width = (len as i32) * cell_w;
                // set size and margins on the label
                gtk_dynamic_loader::widget_set_size_request(&loader_clone, *lbl.as_ref(), width, cell_h);
                gtk_dynamic_loader::widget_set_margin_start(&loader_clone, *lbl.as_ref(), left);
                gtk_dynamic_loader::widget_set_margin_top(&loader_clone, *lbl.as_ref(), top);

                overlay_labels_ref.borrow_mut().push(lbl);
            }
        }
    };

    let controls = gtk::create_grid()?;
    let open_btn = app.create_button("Open")?; let save_btn = app.create_button("Save As")?; let quit_btn = app.create_button("Quit")?;
    controls.attach(&open_btn, 0, 0, 1, 1); controls.attach(&save_btn, 1, 0, 1, 1); controls.attach(&quit_btn, 2, 0, 1, 1);

    // Initial overlay build
    refresh_overlays();

    let vbox = gtk::create_box(gtk::Orientation::Vertical, 6)?;
    vbox.append(&overlay);
    vbox.append(&controls);
    win.set_child(&vbox);
    win.present();

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
