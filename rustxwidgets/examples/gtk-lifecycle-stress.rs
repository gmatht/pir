use rustxwidgets::backends_gtk_adapter as gtk;
use rustxwidgets::lifecycle_stress::Op;
use rustxwidgets::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Copy)]
struct Config {
    seed: u64,
    steps: usize,
    prefer_gtk3: bool,
    verbose: bool,
    trace: bool,
}

struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed };
        Self { state }
    }
    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.state >> 32) as u32
    }
    fn next_bool(&mut self) -> bool {
        (self.next_u32() & 1) == 0
    }
    fn range_i32(&mut self, upper: i32) -> i32 {
        if upper <= 1 { return 0; }
        (self.next_u32() % (upper as u32)) as i32
    }
}

fn parse_config() -> Config {
    let mut cfg = Config {
        seed: 1,
        steps: 200_000,
        prefer_gtk3: false,
        verbose: false,
        trace: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => {
                if let Some(v) = args.next() { cfg.seed = v.parse().unwrap_or(cfg.seed); }
            }
            "--steps" => {
                if let Some(v) = args.next() { cfg.steps = v.parse().unwrap_or(cfg.steps); }
            }
            "--prefer-gtk3" | "-3" => cfg.prefer_gtk3 = true,
            "--verbose" | "-v" => cfg.verbose = true,
            "--trace" => cfg.trace = true,
            _ => {}
        }
    }
    cfg
}

fn schedule_idle(loader: &Arc<gtk_dynamic_loader::Loader>, cb: impl FnMut() + 'static) {
    unsafe { gtk_dynamic_loader::idle_add_once(loader, Box::new(cb)); }
}

fn schedule_formula_focus(loader: &Arc<gtk_dynamic_loader::Loader>, formula: gtk::Entry) {
    let loader = loader.clone();
    schedule_idle(&loader, Box::new(move || { formula.grab_focus(); }));
}

// ── Safe methods for AnyWidget ──────────────────────────────────────────────
// These delegate to the safe adapter methods, which are safe because each
// variant holds a live typed reference to the widget.
impl AnyWidget {
    fn set_visible(&self, visible: bool) {
        match self {
            AnyWidget::Button(b) => { let _ = b.set_visible(visible); }
            AnyWidget::Label(l) => l.set_visible(visible),
            AnyWidget::Entry(e) => { let _ = e.set_visible(visible); }
            AnyWidget::CheckButton(c) => { let _ = c.set_visible(visible); }
            AnyWidget::RadioButton(r) => { let _ = r.set_visible(visible); }
            AnyWidget::DropDown(d) => { let _ = d.set_visible(visible); }
            AnyWidget::TextView(t) => { let _ = t.set_visible(visible); }
        }
    }

    fn set_hexpand(&self, expand: bool) {
        match self {
            AnyWidget::Button(b) => { let _ = b.set_hexpand(expand); }
            AnyWidget::Label(l) => l.set_hexpand(expand),
            AnyWidget::Entry(e) => e.set_hexpand(expand),
            AnyWidget::CheckButton(c) => { let _ = c.set_hexpand(expand); }
            AnyWidget::RadioButton(r) => { let _ = r.set_hexpand(expand); }
            AnyWidget::DropDown(d) => { let _ = d.set_hexpand(expand); }
            AnyWidget::TextView(t) => t.set_hexpand(expand),
        }
    }

    fn set_vexpand(&self, expand: bool) {
        match self {
            AnyWidget::Button(b) => { let _ = b.set_vexpand(expand); }
            AnyWidget::Label(l) => l.set_vexpand(expand),
            AnyWidget::Entry(e) => e.set_vexpand(expand),
            AnyWidget::CheckButton(c) => { let _ = c.set_vexpand(expand); }
            AnyWidget::RadioButton(r) => { let _ = r.set_vexpand(expand); }
            AnyWidget::DropDown(d) => { let _ = d.set_vexpand(expand); }
            AnyWidget::TextView(t) => t.set_vexpand(expand),
        }
    }

    fn set_size_request(&self, w: i32, h: i32) {
        match self {
            AnyWidget::Button(b) => b.set_size_request(w, h),
            AnyWidget::Label(l) => l.set_size_request(w, h),
            AnyWidget::Entry(e) => e.set_size_request(w, h),
            AnyWidget::CheckButton(c) => { let _ = c.set_size_request(w, h); }
            AnyWidget::RadioButton(r) => { let _ = r.set_size_request(w, h); }
            AnyWidget::DropDown(d) => { let _ = d.set_size_request(w, h); }
            AnyWidget::TextView(t) => t.set_size_request(w, h),
        }
    }
}

enum AnyWidget {
    Button(gtk::Button),
    Label(gtk::Label),
    Entry(gtk::Entry),
    CheckButton(gtk::CheckButton),
    RadioButton(gtk::RadioButton),
    DropDown(gtk::DropDown),
    TextView(gtk::TextView),
}

impl AsRef<*mut std::ffi::c_void> for AnyWidget {
    fn as_ref(&self) -> &*mut std::ffi::c_void {
        match self {
            AnyWidget::Button(b) => b.as_ref(),
            AnyWidget::Label(l) => l.as_ref(),
            AnyWidget::Entry(e) => e.as_ref(),
            AnyWidget::CheckButton(c) => c.as_ref(),
            AnyWidget::RadioButton(r) => r.as_ref(),
            AnyWidget::DropDown(d) => d.as_ref(),
            AnyWidget::TextView(t) => t.as_ref(),
        }
    }
}

const KIND_NAMES: &[&str] = &["button","label","entry","chkbtn","radbtn","dropdown","textview"];

fn kind_index(w: &AnyWidget) -> usize {
    match w {
        AnyWidget::Button(_) => 0,
        AnyWidget::Label(_) => 1,
        AnyWidget::Entry(_) => 2,
        AnyWidget::CheckButton(_) => 3,
        AnyWidget::RadioButton(_) => 4,
        AnyWidget::DropDown(_) => 5,
        AnyWidget::TextView(_) => 6,
    }
}

struct WidgetPool {
    items: Vec<(AnyWidget, bool)>,
    kind_counts: [usize; 7],
}

impl WidgetPool {
    fn new() -> Self { WidgetPool { items: Vec::new(), kind_counts: [0; 7] } }
    fn add(&mut self, w: AnyWidget, in_overlay: bool) {
        self.kind_counts[kind_index(&w)] += 1;
        self.items.push((w, in_overlay));
    }
    fn remove_random(&mut self, rng: &mut Lcg) -> Option<(AnyWidget, bool)> {
        if self.items.is_empty() { return None; }
        let idx = rng.range_i32(self.items.len() as i32) as usize;
        let (w, in_ov) = self.items.remove(idx);
        self.kind_counts[kind_index(&w)] = self.kind_counts[kind_index(&w)].saturating_sub(1);
        Some((w, in_ov))
    }
    fn remove_first(&mut self) -> Option<(AnyWidget, bool)> {
        if self.items.is_empty() { return None; }
        let (w, in_ov) = self.items.remove(0);
        self.kind_counts[kind_index(&w)] = self.kind_counts[kind_index(&w)].saturating_sub(1);
        Some((w, in_ov))
    }
    fn len(&self) -> usize { self.items.len() }
    fn random_idx(&self, rng: &mut Lcg) -> Option<usize> {
        if self.items.is_empty() { return None; }
        Some(rng.range_i32(self.items.len() as i32) as usize)
    }
}

fn mutate_widget(w: &mut AnyWidget, label: &str, rng: &mut Lcg) {
    match w {
        AnyWidget::Button(_) => {}
        AnyWidget::Label(l) => { l.set_text(label); }
        AnyWidget::Entry(e) => { e.set_text(label); }
        AnyWidget::CheckButton(c) => { c.set_active(!c.is_active()); }
        AnyWidget::RadioButton(r) => { r.set_active(!r.is_active()); }
        AnyWidget::DropDown(d) => { d.set_active(rng.next_u32() % 8); }
        AnyWidget::TextView(t) => { t.set_text(label); }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = parse_config();
    if cfg.prefer_gtk3 {
        unsafe { std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1") };
    }

    let app = App::init()?;
    let loader =
        rustxwidgets::backends::gtk::loader().expect("GTK loader missing after App::init");

    let win = app.create_window()?;
    win.set_title("GTK Lifecycle Stress");
    win.set_default_size(960, 600);

    let root = app.create_box(gtk::Orientation::Vertical, 4)?;
    let status = app.create_label("starting lifecycle stress")?;
    let summary = app.create_label("")?;
    let formula = app.create_entry()?;
    formula.set_text("formula");
    formula.set_width_chars(32);

    let hbox = Rc::new(app.create_box(gtk::Orientation::Horizontal, 4)?);
    let vbox = Rc::new(app.create_box(gtk::Orientation::Vertical, 4)?);
    let grid = Rc::new(app.create_grid()?);

    let drawing = gtk_dynamic_loader::DrawingArea::new(loader.clone())?;
    let _drawing_ptr = *drawing.as_ref();
    drawing.set_size_request(640, 200);
    if loader.symbols.gtk_container_add.is_none() {
        drawing.set_content_width(640);
        drawing.set_content_height(200);
    }

    let overlay = gtk_dynamic_loader::Overlay::new(loader.clone())?;
    overlay.set_child(&drawing);

    let scrolled = gtk_dynamic_loader::ScrolledWindow::new(loader.clone())?;
    scrolled.set_policy(0, 0);
    scrolled.set_child(&overlay);

    root.append(&status);
    root.append(&formula);
    root.append(&*hbox);
    root.append(&*vbox);
    root.append(&*grid);
    root.append(&scrolled);
    root.append(&summary);
    win.set_child(&root);
    win.present();

    let win_ptr = *win.as_ref();
    let rng = Rc::new(RefCell::new(Lcg::new(cfg.seed)));
    let step = Rc::new(Cell::new(0usize));
    let counts = Rc::new(RefCell::new([0usize; 12]));
    let pool = Rc::new(RefCell::new(WidgetPool::new()));
    let retired: Rc<RefCell<Vec<AnyWidget>>> = Rc::new(RefCell::new(Vec::new()));
    let pulse_count = Rc::new(Cell::new(0u32));

    let runner: Rc<RefCell<Option<Box<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let runner_for_init = runner.clone();
    let loader_for_run = loader.clone();
    let overlay_for_run = overlay;
    let hbox_for_run = hbox.clone();
    let vbox_for_run = vbox.clone();
    let grid_for_run = grid;
    let _status_for_run = status;
    let _summary_for_run = summary;
    let formula_for_run = formula.clone();
    let rng_for_run = rng.clone();
    let pool_for_run = pool.clone();
    let retired_for_run = retired.clone();
    let pulse_count_for_run = pulse_count.clone();
    let step_for_run = step.clone();
    let counts_for_run = counts.clone();

    *runner.borrow_mut() = Some(Box::new(move || {
        let current = step_for_run.get();
        if current >= cfg.steps {
            let counts = counts_for_run.borrow();
            let p = pool_for_run.borrow();
            println!(
                "completed seed={} steps={} live_widgets={}",
                cfg.seed, cfg.steps, p.len(),
            );
            print!("  ops:");
            for (i, name) in ["add","remove","mutate","style","teardown","visible","expand","focus","resize_win","pulse","resize_wid","abuse"].iter().enumerate() {
                print!(" {}={}", name, counts[i]);
            }
            println!();
            print!("  live:");
            for (i, name) in KIND_NAMES.iter().enumerate() {
                print!(" {}={}", name, p.kind_counts[i]);
            }
            println!();
            let _ = gtk::quit_main_loop();
            return;
        }

        let mut rng = rng_for_run.borrow_mut();
        let op = Op::pick_weighted(rng.next_u32());
        step_for_run.set(current + 1);
        counts_for_run.borrow_mut()[op as usize] += 1;

        match op {
            Op::AddWidget => {
                let kind = rng.next_u32() % 7;
                let label_str = format!("{}{}", KIND_NAMES[kind as usize], current);
                let w = match kind {
                    0 => gtk::create_button(&label_str).ok().map(AnyWidget::Button),
                    1 => gtk::create_label(&label_str).ok().map(AnyWidget::Label),
                    2 => gtk::create_entry().ok().map(|e| {
                        e.set_text(&label_str);
                        AnyWidget::Entry(e)
                    }),
                    3 => gtk::create_checkbutton(&label_str).ok().map(AnyWidget::CheckButton),
                    4 => gtk::create_radiobutton(None, &label_str).ok().map(AnyWidget::RadioButton),
                    5 => {
                        let items: Vec<&str> = vec!["a","b","c","d","e"];
                        gtk::create_dropdown(&items).ok().map(|dd| {
                            dd.set_active((rng.range_i32(5).max(0) as u32));
                            AnyWidget::DropDown(dd)
                        })
                    }
                    _ => gtk::create_textview().ok().map(|tv| {
                        tv.set_text(&label_str);
                        AnyWidget::TextView(tv)
                    }),
                };
                if let Some(w) = w {
                    let mut p = pool_for_run.borrow_mut();
                    if p.len() >= 100 {
                        if let Some((old, _)) = p.remove_first() {
                            old.set_visible(false);
                            retired_for_run.borrow_mut().push(old);
                        }
                    }
                    let layout = rng.next_u32() % 3;
                    match layout {
                        0 => hbox_for_run.append(&w),
                        1 => vbox_for_run.append(&w),
                        _ => { grid_for_run.attach(&w, rng.range_i32(4), rng.range_i32(8), 1, 1); }
                    }
                    w.set_visible(true);
                    p.add(w, false);
                }
            }
            Op::RemoveWidget => {
                if let Some((w, _)) = pool_for_run.borrow_mut().remove_random(&mut rng) {
                    w.set_visible(false);
                    retired_for_run.borrow_mut().push(w);
                }
            }
            Op::MutateWidget => {
                let mut p = pool_for_run.borrow_mut();
                if let Some(idx) = p.random_idx(&mut rng) {
                    let label = format!("mut:{}:{}", current, rng.next_u32() % 1000);
                    mutate_widget(&mut p.items[idx].0, &label, &mut rng);
                }
            }
            Op::CycleStyle => {
                // This operation exercises CSS and style APIs that inherently
                // require unsafe FFI — kept as a minimal unsafe block.
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    let ptr = *p.items[idx].0.as_ref();
                    drop(p);
                    unsafe {
                        if let (Some(get_ctx), Some(add_cls)) = (
                            loader_for_run.symbols.gtk_widget_get_style_context,
                            loader_for_run.symbols.gtk_style_context_add_class,
                        ) {
                            let c = std::ffi::CString::new("test-fuzz").unwrap();
                            let ctx = get_ctx(ptr);
                            if !ctx.is_null() { add_cls(ctx, c.as_ptr()); }
                        }
                    }
                }
            }
            Op::TeardownRace => {
                // Creates an entry, focuses it, then schedules a deferred remove.
                // The remove is inherently unsafe (calls into GTK's destroy cascade).
                if let Ok(entry) = gtk::create_entry() {
                    let entry_clone = std::mem::ManuallyDrop::new(entry.clone());
                    let _ = entry.connect_focus_in_event(move |_| { 0 });
                    let entry_ptr = *(*entry_clone).as_ref();
                    overlay_for_run.add_overlay(&entry);
                    overlay_for_run.set_overlay_pass_through(&entry, false);
                    entry.set_visible(true);
                    entry.grab_focus();
                    let loader2 = loader_for_run.clone();
                    schedule_idle(&loader_for_run, Box::new(move || {
                        unsafe { gtk_dynamic_loader::remove_from_parent(&loader2, entry_ptr); }
                    }));
                }
            }
            Op::ToggleVisible => {
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    let visible = rng.next_bool();
                    p.items[idx].0.set_visible(visible);
                }
            }
            Op::ToggleExpand => {
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    p.items[idx].0.set_hexpand(rng.next_bool());
                    p.items[idx].0.set_vexpand(rng.next_bool());
                }
            }
            Op::FocusShuffle => {
                if rng.next_bool() {
                    schedule_formula_focus(&loader_for_run, formula_for_run.clone());
                } else {
                    let p = pool_for_run.borrow();
                    if let Some(idx) = p.random_idx(&mut rng) {
                        if let AnyWidget::Entry(e) = &p.items[idx].0 {
                            let e = e.clone();
                            drop(p);
                            let f = formula_for_run.clone();
                            let loader = loader_for_run.clone();
                            schedule_idle(&loader, Box::new(move || {
                                e.grab_focus();
                                let _ = &f;
                            }));
                        }
                    }
                }
            }
            Op::ResizeWindow => {
                unsafe {
                    if let Some(set_size) = loader_for_run.symbols.gtk_window_set_default_size {
                        set_size(win_ptr, 400 + rng.range_i32(300), 200 + rng.range_i32(200));
                    }
                }
            }
            Op::PulseChild => {
                if pulse_count_for_run.get() < 1000 {
                    if let Ok(lbl) = gtk::create_label("pulse") {
                        hbox_for_run.append(&lbl);
                        let any = AnyWidget::Label(lbl);
                        any.set_visible(true);
                        pool_for_run.borrow_mut().add(any, false);
                        pulse_count_for_run.set(pulse_count_for_run.get() + 1);
                    }
                }
            }
            Op::ResizeWidget => {
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    p.items[idx].0.set_size_request(
                        50 + rng.range_i32(200), 20 + rng.range_i32(100),
                    );
                }
            }
            Op::AbuseWidget => {
                // Exercise stupid API misuse patterns that should be handled gracefully.
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    let abuse = rng.next_u32() % 13;
                    match abuse {
                        // 0-1: Set size to degenerate values
                        0 => p.items[idx].0.set_size_request(0, 0),
                        1 => p.items[idx].0.set_size_request(i32::MAX, i32::MAX),
                        // 2-3: Set empty / huge text on text-bearing widgets
                        2 => match &p.items[idx].0 {
                            AnyWidget::Label(l) => l.set_text(""),
                            AnyWidget::Entry(e) => e.set_text(""),
                            AnyWidget::TextView(t) => t.set_text(""),
                            _ => {}
                        },
                        3 => {
                            let huge = "X".repeat(65536);
                            match &p.items[idx].0 {
                                AnyWidget::Label(l) => l.set_text(&huge),
                                AnyWidget::Entry(e) => e.set_text(&huge),
                                AnyWidget::TextView(t) => t.set_text(&huge),
                                _ => {}
                            }
                        },
                        // 4: Set negative / invalid size request
                        4 => p.items[idx].0.set_size_request(-1, -1),
                        // 5: Rapid toggle visible 10 times
                        5 => {
                            for _ in 0..10 {
                                p.items[idx].0.set_visible(true);
                                p.items[idx].0.set_visible(false);
                            }
                            p.items[idx].0.set_visible(true);
                        },
                        // 6: Set dropdown to out-of-range index
                        6 => if let AnyWidget::DropDown(d) = &p.items[idx].0 {
                            d.set_active(9999);
                        },
                        // 7: Focus on a hidden widget
                        7 => {
                            p.items[idx].0.set_visible(false);
                            if let AnyWidget::Entry(e) = &p.items[idx].0 {
                                e.grab_focus();
                            }
                            p.items[idx].0.set_visible(true);
                        },
                        // 8: Rapidly toggle expand flags
                        8 => {
                            for _ in 0..5 {
                                p.items[idx].0.set_hexpand(true);
                                p.items[idx].0.set_vexpand(false);
                                p.items[idx].0.set_hexpand(false);
                                p.items[idx].0.set_vexpand(true);
                            }
                        },
                        // 9: Add same widget to another container (multi-parent)
                        9 => {
                            let l = rng.next_u32() % 3;
                            match l {
                                0 => { p.items[idx].0.set_visible(true); hbox_for_run.append(&p.items[idx].0); },
                                1 => { p.items[idx].0.set_visible(true); vbox_for_run.append(&p.items[idx].0); },
                                _ => { p.items[idx].0.set_visible(true); grid_for_run.attach(&p.items[idx].0, 0, 0, 1, 1); },
                            }
                        },
                        // 10: Overlay remove pattern — wrong vs correct.
                        // The g-spreadsheet-canvas crash happened because
                        // remove_from_parent() doesn't clean GtkOverlay's
                        // internal children list, causing use-after-free
                        // on later show_all().
                        10 => {
                            let ptr = *p.items[idx].0.as_ref();
                            overlay_for_run.add_overlay(&p.items[idx].0);
                            overlay_for_run.set_overlay_pass_through(&p.items[idx].0, false);
                            if rng.next_bool() {
                                // Wrong removal (the spreadsheet bug)
                                unsafe { gtk_dynamic_loader::remove_from_parent(&loader_for_run, ptr); }
                            } else {
                                // Correct removal
                                match &p.items[idx].0 {
                                    AnyWidget::Button(b) => overlay_for_run.remove(b),
                                    AnyWidget::Label(l) => overlay_for_run.remove(l),
                                    AnyWidget::Entry(e) => overlay_for_run.remove(e),
                                    AnyWidget::CheckButton(c) => overlay_for_run.remove(c),
                                    AnyWidget::RadioButton(r) => overlay_for_run.remove(r),
                                    AnyWidget::DropDown(d) => overlay_for_run.remove(d),
                                    AnyWidget::TextView(t) => overlay_for_run.remove(t),
                                }
                            }
                            p.items[idx].0.set_visible(true);
                        },
                        // 11: Create event controllers without storing them
                        // (gesture click, event controller key).  If GTK4's
                        // gtk_widget_add_controller doesn't add a ref, the
                        // controllers get freed immediately and later GTK
                        // iteration over widget controllers hits freed memory.
                        11 => {
                            if let Ok(gc) = gtk_dynamic_loader::GestureClick::new(loader_for_run.clone()) {
                                gc.add_to_widget(&p.items[idx].0);
                                // gc drops here — if GTK4 didn't ref it, this
                                // puts a dangling pointer in the widget's
                                // controller list that will crash on next
                                // show/event-dispatch.
                            }
                        },
                        // 12: Connect signal on entry, remove from parent, then fire
                        // the signal (e.g. activate).  If the entry/focus drops
                        // before the signal fires, the closure's user_data is
                        // corrupted and the trampoline crashes.
                        12 => {
                            if let AnyWidget::Entry(e) = &p.items[idx].0 {
                                let _ = e.connect_activate(|_param| {
                                    // Signal fires on Enter; if entry
                                    // is destroyed first, user_data is 0x1.
                                });
                                e.grab_focus();
                                // Remove and re-add to exercise signal
                                // handler lifetime across parent changes.
                                let ptr = *e.as_ref();
                                unsafe { gtk_dynamic_loader::remove_from_parent(&loader_for_run, ptr); }
                                hbox_for_run.append(&p.items[idx].0);
                                p.items[idx].0.set_visible(true);
                            }
                        },
                        _ => {}
                    }
                }
                drop(p);
            }
        }

        if cfg.verbose && current % 1000 == 0 {
            let p = pool_for_run.borrow();
            println!("step={} live={}", current, p.len());
        }

        let runner_for_next = runner_for_init.clone();
        let loader_for_next = loader_for_run.clone();
        schedule_idle(&loader_for_next, Box::new(move || {
            if let Some(run) = runner_for_next.borrow_mut().as_mut() {
                run();
            }
        }));
    }));

    let runner_start = runner.clone();
    schedule_idle(&loader, Box::new(move || {
        if let Some(run) = runner_start.borrow_mut().as_mut() {
            run();
        }
    }));

    let _ = app.run();
    std::process::exit(0);
}
