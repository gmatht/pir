use gtk_dynamic_loader::*;
use std::ffi::c_void;
use std::sync::Arc;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GdkRectangle {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

type GetAllocation = unsafe extern "C" fn(*mut c_void, *mut GdkRectangle);
type GMainLoopQuit = unsafe extern "C" fn(*mut c_void);

fn get_allocation(loader: &Arc<Loader>, widget: *mut c_void) -> GdkRectangle {
    let gtk_lib = match loader.libs.get("libgtk") {
        Some(l) => l,
        None => return GdkRectangle { x: 0, y: 0, width: 0, height: 0 },
    };
    unsafe {
        let sym = match gtk_lib.get::<GetAllocation>(b"gtk_widget_get_allocation") {
            Ok(s) => *s,
            Err(_) => return GdkRectangle { x: 0, y: 0, width: 0, height: 0 },
        };
        let mut rect = GdkRectangle { x: 0, y: 0, width: 0, height: 0 };
        sym(widget, &mut rect as *mut GdkRectangle);
        rect
    }
}

struct NamedWidget {
    name: &'static str,
    widget: *mut c_void,
    parent: *mut c_void,
}

struct WidgetSet {
    loader: Arc<Loader>,
    loop_ptr: *mut c_void,
    loop_quit: GMainLoopQuit,
    widgets: Vec<NamedWidget>,
}

unsafe extern "C" fn idle_cb(data: *mut c_void) -> i32 {
    let ws = &*(data as *mut WidgetSet);

    let version = match ws.loader.version {
        gtk_dynamic_loader::Version::Gtk4 => "gtk4",
        gtk_dynamic_loader::Version::Gtk3 => "gtk3",
        _ => "unknown",
    };

    eprintln!("LAYOUT_RESULT version={}", version);
    println!("{{");
    println!("  \"version\": \"{}\",", version);
    let last = ws.widgets.len() - 1;
    for (idx, nw) in ws.widgets.iter().enumerate() {
        let r = get_allocation(&ws.loader, nw.widget);
        let pr = if !nw.parent.is_null() {
            get_allocation(&ws.loader, nw.parent)
        } else {
            GdkRectangle { x: 0, y: 0, width: 0, height: 0 }
        };
        let comma = if idx < last { "," } else { "" };
        println!(
            "  \"{}\": {{ \"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}, \"parent_x\": {}, \"parent_y\": {} }}{}",
            nw.name, r.x, r.y, r.width, r.height, pr.x, pr.y, comma
        );
    }
    println!("}}");

    (ws.loop_quit)(ws.loop_ptr);
    let _ = Box::from_raw(data as *mut WidgetSet);
    0
}

#[test]
fn test_spreadsheet_layout() {
    let loader = Loader::new().expect("Loader::new failed");
    let win = Window::new(loader.clone()).expect("Window");
    let win_ptr = *win.as_ref();

    let vbox = BoxWidget::new(loader.clone(), Orientation::Vertical, 0).expect("VBox");
    let vbox_ptr = *vbox.as_ref();

    // Toolbar
    let toolbar = BoxWidget::new(loader.clone(), Orientation::Horizontal, 2).expect("Toolbar");
    let toolbar_ptr = *toolbar.as_ref();
    let open_btn = Button::with_label(loader.clone(), "Open").expect("Open");
    let open_ptr = *open_btn.as_ref();
    let save_btn = Button::with_label(loader.clone(), "Save As").expect("Save");
    let save_ptr = *save_btn.as_ref();
    toolbar.append(&open_btn);
    toolbar.append(&save_btn);
    vbox.append(&toolbar);

    // Formula bar
    let formula_box = BoxWidget::new(loader.clone(), Orientation::Horizontal, 4).expect("FormulaBox");
    let formula_ptr = *formula_box.as_ref();
    let fx_label = Label::new(loader.clone(), "  fx  ").expect("fx");
    let fx_ptr = *fx_label.as_ref();
    formula_box.append(&fx_label);
    vbox.append(&formula_box);

    // Grid with headers and cells
    let grid = Grid::new(loader.clone()).expect("Grid");
    let grid_ptr = *grid.as_ref();
    let hdr_a = Label::new(loader.clone(), "A").expect("hdrA");
    let hdr_a_ptr = *hdr_a.as_ref();
    grid.attach(&hdr_a, 1, 0, 1, 1);
    let row_marker = Label::new(loader.clone(), "1").expect("row1");
    let row_marker_ptr = *row_marker.as_ref();
    grid.attach(&row_marker, 0, 1, 1, 1);
    let cell_a1 = Label::new(loader.clone(), "Hello").expect("A1");
    let cell_a1_ptr = *cell_a1.as_ref();
    grid.attach(&cell_a1, 1, 1, 1, 1);
    let cell_b1 = Label::new(loader.clone(), "World").expect("B1");
    let cell_b1_ptr = *cell_b1.as_ref();
    grid.attach(&cell_b1, 2, 1, 1, 1);

    let symbols = &loader.symbols;
    if let Some(sr) = symbols.gtk_widget_set_size_request {
        unsafe {
            sr(open_ptr, 68, 34);
            sr(save_ptr, 86, 34);
            sr(cell_a1_ptr, 100, 28);
            sr(cell_b1_ptr, 100, 28);
            sr(hdr_a_ptr, 100, 28);
            sr(row_marker_ptr, 46, 28);
        }
    }

    vbox.append(&grid);
    win.set_child(&vbox);
    win.present();

    let loop_new = loader.symbols.g_main_loop_new.expect("g_main_loop_new");
    let loop_quit = loader.symbols.g_main_loop_quit.expect("g_main_loop_quit");
    let loop_ptr = unsafe { loop_new(std::ptr::null_mut(), 0) };
    let idle_add = loader.symbols.g_idle_add.expect("g_idle_add");

    let ws = Box::new(WidgetSet {
        loader: loader.clone(),
        loop_ptr,
        loop_quit,
        widgets: vec![
            NamedWidget { name: "win", widget: win_ptr, parent: std::ptr::null_mut() },
            NamedWidget { name: "vbox", widget: vbox_ptr, parent: win_ptr },
            NamedWidget { name: "toolbar", widget: toolbar_ptr, parent: vbox_ptr },
            NamedWidget { name: "open_btn", widget: open_ptr, parent: toolbar_ptr },
            NamedWidget { name: "save_btn", widget: save_ptr, parent: toolbar_ptr },
            NamedWidget { name: "formula_box", widget: formula_ptr, parent: vbox_ptr },
            NamedWidget { name: "fx_label", widget: fx_ptr, parent: formula_ptr },
            NamedWidget { name: "grid", widget: grid_ptr, parent: vbox_ptr },
            NamedWidget { name: "hdr_a", widget: hdr_a_ptr, parent: grid_ptr },
            NamedWidget { name: "row_marker", widget: row_marker_ptr, parent: grid_ptr },
            NamedWidget { name: "cell_a1", widget: cell_a1_ptr, parent: grid_ptr },
            NamedWidget { name: "cell_b1", widget: cell_b1_ptr, parent: grid_ptr },
        ],
    });
    let ws_ptr = Box::into_raw(ws) as *mut c_void;

    let idle_fn: Option<unsafe extern "C" fn(*mut c_void) -> i32> = Some(idle_cb);
    unsafe { idle_add(idle_fn, ws_ptr); }

    unsafe {
        let loop_run = loader.symbols.g_main_loop_run.expect("g_main_loop_run");
        loop_run(loop_ptr);
    }

    // Now that we have printed JSON, also do in-code assertions on reasonable values
    // We use the same gtk_widget_get_allocation to read the values
    // Verify that size_requested cells got their sizes
    let gtk_lib = loader.libs.get("libgtk").unwrap();
    let get_allocation = unsafe {
        let sym = gtk_lib.get::<GetAllocation>(b"gtk_widget_get_allocation").unwrap();
        *sym
    };

    let check = |name: &str, w: *mut c_void, min_w: i32, min_h: i32| {
        let mut r = GdkRectangle { x: 0, y: 0, width: 0, height: 0 };
        unsafe { get_allocation(w, &mut r as *mut GdkRectangle); }
        assert!(r.width >= min_w, "{} width {} < {}", name, r.width, min_w);
        assert!(r.height >= min_h, "{} height {} < {}", name, r.height, min_h);
    };

    check("win", win_ptr, 200, 80);
    check("vbox", vbox_ptr, 200, 80);
    check("toolbar", toolbar_ptr, 100, 10);
    check("open_btn", open_ptr, 40, 10);
    check("save_btn", save_ptr, 40, 10);
    check("formula_box", formula_ptr, 100, 10);
    check("grid", grid_ptr, 100, 40);
    check("hdr_a", hdr_a_ptr, 80, 20);
    check("cell_a1", cell_a1_ptr, 80, 20);
    check("cell_b1", cell_b1_ptr, 80, 20);
    check("row_marker", row_marker_ptr, 30, 20);
}
