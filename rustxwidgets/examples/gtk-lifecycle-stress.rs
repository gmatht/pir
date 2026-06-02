use rustxwidgets::backends_gtk_adapter as gtk;
use rustxwidgets::lifecycle_stress::Op;
use rustxwidgets::prelude::*;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
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

fn set_widget_visible(loader: &Arc<gtk_dynamic_loader::Loader>, widget: *mut c_void, visible: bool) {
    unsafe { gtk_dynamic_loader::widget_set_visible(loader, widget, visible) }
}

fn set_widget_expand(loader: &Arc<gtk_dynamic_loader::Loader>, widget: *mut c_void, hexpand: bool, vexpand: bool) {
    unsafe {
        gtk_dynamic_loader::widget_set_hexpand(loader, widget, hexpand);
        gtk_dynamic_loader::widget_set_vexpand(loader, widget, vexpand);
    }
}

fn remove_from_parent(loader: &Arc<gtk_dynamic_loader::Loader>, widget: *mut c_void) {
    unsafe { gtk_dynamic_loader::remove_from_parent(loader, widget) }
}

fn schedule_formula_focus(loader: &Arc<gtk_dynamic_loader::Loader>, formula: gtk::Entry) {
    let loader = loader.clone();
    schedule_idle(&loader, move || { formula.grab_focus(); });
}

fn widget_ptr(w: &AnyWidget) -> *mut c_void {
    match w {
        AnyWidget::Button(b) => *b.as_ref(),
        AnyWidget::Label(l) => *l.as_ref(),
        AnyWidget::Entry(e) => *e.as_ref(),
        AnyWidget::CheckButton(c) => *c.as_ref(),
        AnyWidget::RadioButton(r) => *r.as_ref(),
        AnyWidget::DropDown(d) => *d.as_ref(),
        AnyWidget::TextView(t) => *t.as_ref(),
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

impl AsRef<*mut c_void> for AnyWidget {
    fn as_ref(&self) -> &*mut c_void {
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
    items: Vec<(AnyWidget, bool)>, // (widget, in_overlay)
    kind_counts: [usize; 7],
}

impl WidgetPool {
    fn new() -> Self {
        WidgetPool { items: Vec::new(), kind_counts: [0; 7] }
    }

    fn add(&mut self, w: AnyWidget, in_overlay: bool) {
        let ki = kind_index(&w);
        self.kind_counts[ki] += 1;
        self.items.push((w, in_overlay));
    }

    fn remove_random(&mut self, rng: &mut Lcg) -> Option<(AnyWidget, bool)> {
        if self.items.is_empty() { return None; }
        let idx = rng.range_i32(self.items.len() as i32) as usize;
        let (w, in_ov) = self.items.remove(idx);
        let ki = kind_index(&w);
        self.kind_counts[ki] = self.kind_counts[ki].saturating_sub(1);
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
        AnyWidget::Button(b) => {
            let _ = b;
        }
        AnyWidget::Label(l) => { l.set_text(label); }
        AnyWidget::Entry(e) => { e.set_text(label); }
        AnyWidget::CheckButton(c) => { c.set_active(!c.is_active()); }
        AnyWidget::RadioButton(r) => { r.set_active(!r.is_active()); }
        AnyWidget::DropDown(d) => {
            d.set_active(rng.next_u32() % 8);
        }
        AnyWidget::TextView(t) => { t.set_text(label); }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = parse_config();
    if cfg.prefer_gtk3 {
        unsafe { std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1") };
    }

    let app = App::init()?;
    let loader = rustxwidgets::backends::gtk::loader().expect("GTK loader missing after App::init");

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
    let grid = Rc::new(app.create_grid()?);

    let drawing = gtk_dynamic_loader::DrawingArea::new(loader.clone())?;
    let drawing_ptr = *drawing.as_ref();
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
    root.append(&*grid);
    root.append(&scrolled);
    root.append(&summary);
    win.set_child(&root);
    win.present();

    let rng = Rc::new(RefCell::new(Lcg::new(cfg.seed)));
    let step = Rc::new(Cell::new(0usize));
    let counts = Rc::new(RefCell::new([0usize; 9]));
    let pool = Rc::new(RefCell::new(WidgetPool::new()));
    let retired: Rc<RefCell<Vec<AnyWidget>>> = Rc::new(RefCell::new(Vec::new()));
    let pulse_count = Rc::new(Cell::new(0u32));

    let runner: Rc<RefCell<Option<Box<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let runner_for_init = runner.clone();
    let loader_for_run = loader.clone();
    let overlay_for_run = overlay.clone();
    let hbox_for_run = hbox.clone();
    let grid_for_run = grid.clone();
    let _status_for_run = status;
    let _summary_for_run = summary;
    let formula_for_run = formula.clone();
    let rng_for_run = rng.clone();
    let pool_for_run = pool.clone();
    let retired_for_run = retired.clone();
    let pulse_count_for_run = pulse_count.clone();
    let step_for_run = step.clone();
    let counts_for_run = counts.clone();
    let win_ptr = *win.as_ref();

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
            for (i, name) in ["add","remove","mutate","visible","expand","focus","resize_win","pulse","resize_wid"].iter().enumerate() {
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
        let raw_op = Op::from_index(rng.next_u32());
        let op = if raw_op == Op::RemoveWidget && rng.next_u32() % 100 < 60 {
            Op::AddWidget
        } else {
            raw_op
        };

        step_for_run.set(current + 1);
        counts_for_run.borrow_mut()[op as usize] += 1;

        match op {
            Op::AddWidget => {
                let kind = rng.next_u32() % 7;
                let label_str = format!("{}{}", KIND_NAMES[kind as usize], current);

                let w = match kind {
                    0 => gtk::create_button(&label_str).ok().map(AnyWidget::Button),
                    1 => gtk::create_label(&label_str).ok().map(AnyWidget::Label),
                    2 => {
                        gtk::create_entry().ok().map(|e| {
                            e.set_text(&label_str);
                            AnyWidget::Entry(e)
                        })
                    }
                    3 => gtk::create_checkbutton(&label_str).ok().map(AnyWidget::CheckButton),
                    4 => gtk::create_radiobutton(None, &label_str).ok().map(AnyWidget::RadioButton),
                    5 => {
                        let items: Vec<&str> = vec!["a","b","c","d","e"];
                        gtk::create_dropdown(&items).ok().map(|dd| {
                            dd.set_active(rng.range_i32(5).max(0) as u32);
                            AnyWidget::DropDown(dd)
                        })
                    }
                    _ => gtk::create_textview().ok().map(|tv| {
                        tv.set_text(&label_str);
                        AnyWidget::TextView(tv)
                    }),
                };

                if let Some(w) = w {
                    let ptr = widget_ptr(&w);
                    hbox_for_run.append(&w);
                    set_widget_visible(&loader_for_run, ptr, true);
                    pool_for_run.borrow_mut().add(w, false);
                }
            }
            Op::RemoveWidget => {
                if let Some((w, _)) = pool_for_run.borrow_mut().remove_random(&mut rng) {
                    let ptr = widget_ptr(&w);
                    set_widget_visible(&loader_for_run, ptr, false);
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
            Op::ToggleVisible => {
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    let ptr = widget_ptr(&p.items[idx].0);
                    drop(p);
                    set_widget_visible(&loader_for_run, ptr, rng.next_bool());
                }
            }
            Op::ToggleExpand => {
                set_widget_expand(&loader_for_run, drawing_ptr, rng.next_bool(), rng.next_bool());
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
                            schedule_idle(&loader, move || {
                                e.grab_focus();
                                let _ = &f;
                            });
                        }
                    }
                }
            }
            Op::ResizeWindow => {
                let w = 400 + rng.range_i32(600);
                let h = 200 + rng.range_i32(400);
                if let Some(set_size) = loader_for_run.symbols.gtk_window_set_default_size {
                    unsafe { set_size(win_ptr, w, h); }
                }
            }
            Op::PulseChild => {
                if pulse_count_for_run.get() < 1000 {
                    if let Ok(label) = gtk::create_label("pulse") {
                        let ptr = *label.as_ref();
                        hbox_for_run.append(&label);
                        set_widget_visible(&loader_for_run, ptr, true);
                        pool_for_run.borrow_mut().add(AnyWidget::Label(label), false);
                        pulse_count_for_run.set(pulse_count_for_run.get() + 1);
                    }
                }
            }
            Op::ResizeWidget => {
                let p = pool_for_run.borrow();
                if let Some(idx) = p.random_idx(&mut rng) {
                    let ptr = widget_ptr(&p.items[idx].0);
                    let w = 50 + rng.range_i32(200);
                    let h = 20 + rng.range_i32(100);
                    drop(p);
                    unsafe { gtk_dynamic_loader::widget_set_size_request(&loader_for_run, ptr, w, h); }
                }
            }
        }

        if cfg.verbose {
            if current % 1000 == 0 {
                let p = pool_for_run.borrow();
                println!("step={} live={}", current, p.len());
            }
        }

        let runner_for_next = runner_for_init.clone();
        let loader_for_next = loader_for_run.clone();
        schedule_idle(&loader_for_next, move || {
            if let Some(run) = runner_for_next.borrow_mut().as_mut() {
                run();
            }
        });
    }));

    let runner_start = runner.clone();
    schedule_idle(&loader, move || {
        if let Some(run) = runner_start.borrow_mut().as_mut() {
            run();
        }
    });

    let _ = app.run();
    Ok(())
}
