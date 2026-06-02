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
        if upper <= 1 {
            return 0;
        }
        (self.next_u32() % (upper as u32)) as i32
    }
}

fn parse_config() -> Config {
    let mut cfg = Config {
        seed: 1,
        steps: 400,
        prefer_gtk3: false,
        verbose: false,
        trace: false,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => {
                if let Some(v) = args.next() {
                    cfg.seed = v.parse().unwrap_or(cfg.seed);
                }
            }
            "--steps" => {
                if let Some(v) = args.next() {
                    cfg.steps = v.parse().unwrap_or(cfg.steps);
                }
            }
            "--prefer-gtk3" | "-3" => cfg.prefer_gtk3 = true,
            "--verbose" | "-v" => cfg.verbose = true,
            "--trace" => cfg.trace = true,
            _ => {}
        }
    }

    cfg
}

// This harness is intended for two modes:
// - low step counts: reproducible smoke coverage for lifecycle operations
// - higher step counts: crash-finding fuzzing that may still reproduce known
//   GTK3 overlay/editor teardown bugs under investigation

fn schedule_idle(loader: &Arc<gtk_dynamic_loader::Loader>, cb: impl FnMut() + 'static) {
    unsafe { gtk_dynamic_loader::idle_add_once(loader, Box::new(cb)); }
}

fn position_editor(loader: &Arc<gtk_dynamic_loader::Loader>, editor: &gtk::Entry, x: i32, y: i32, visible: bool) {
    unsafe {
        gtk_dynamic_loader::widget_set_margin_start(loader, *editor.as_ref(), x);
        gtk_dynamic_loader::widget_set_margin_top(loader, *editor.as_ref(), y);
        gtk_dynamic_loader::widget_set_halign(loader, *editor.as_ref(), 1);
        gtk_dynamic_loader::widget_set_valign(loader, *editor.as_ref(), 1);
        gtk_dynamic_loader::widget_set_visible(loader, *editor.as_ref(), visible);
    }
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

fn queue_widget_draw(loader: &Arc<gtk_dynamic_loader::Loader>, widget: *mut c_void) {
    unsafe { gtk_dynamic_loader::widget_queue_draw(loader, widget) }
}

fn remove_from_parent(loader: &Arc<gtk_dynamic_loader::Loader>, widget: *mut c_void) {
    unsafe { gtk_dynamic_loader::remove_from_parent(loader, widget) }
}

fn schedule_entry_focus(
    loader: &Arc<gtk_dynamic_loader::Loader>,
    entry: gtk::Entry,
    current_focus_entry_id: &Rc<Cell<u64>>,
    expected_entry_id: u64,
) {
    let loader = loader.clone();
    let current_focus_entry_id = current_focus_entry_id.clone();
    schedule_idle(&loader, move || {
        if current_focus_entry_id.get() != expected_entry_id {
            return;
        }
        entry.grab_focus();
    });
}

fn schedule_formula_focus(loader: &Arc<gtk_dynamic_loader::Loader>, formula: gtk::Entry) {
    let loader = loader.clone();
    schedule_idle(&loader, move || {
        formula.grab_focus();
    });
}

fn schedule_editor_remove(
    loader: &Arc<gtk_dynamic_loader::Loader>,
    overlay: &gtk_dynamic_loader::Overlay,
    formula: &gtk::Entry,
    entry: gtk::Entry,
    use_overlay_remove: bool,
    pending_detach: &Rc<Cell<bool>>,
    current_focus_entry_id: &Rc<Cell<u64>>,
    retired_entries: &Rc<RefCell<Vec<gtk::Entry>>>,
) {
    pending_detach.set(true);
    current_focus_entry_id.set(0);
    set_widget_visible(loader, *entry.as_ref(), false);
    let loader_for_idle = loader.clone();
    let loader_for_outer_idle = loader.clone();
    let loader_for_remove = loader.clone();
    let overlay = overlay.clone();
    let formula = formula.clone();
    let pending_detach = pending_detach.clone();
    let retired_entries = retired_entries.clone();
    schedule_idle(&loader_for_outer_idle, move || {
        let overlay = overlay.clone();
        let loader_for_remove = loader_for_remove.clone();
        let entry = entry.clone();
        let formula = formula.clone();
        let pending_detach = pending_detach.clone();
        let retired_entries = retired_entries.clone();
        schedule_idle(&loader_for_idle, move || {
            let keepalive_entry = entry.clone();
            if use_overlay_remove {
                overlay.remove(&entry);
            } else {
                remove_from_parent(&loader_for_remove, *entry.as_ref());
            }
            let loader_for_finalize = loader_for_remove.clone();
            let pending_detach_for_finalize = pending_detach.clone();
            let formula_for_finalize = formula.clone();
            let retired_entries_for_finalize = retired_entries.clone();
            schedule_idle(&loader_for_remove, move || {
                pending_detach_for_finalize.set(false);
                schedule_formula_focus(&loader_for_finalize, formula_for_finalize.clone());
                retired_entries_for_finalize.borrow_mut().push(keepalive_entry.clone());
            });
        });
    });
}

fn attach_editor(
    loader: &Arc<gtk_dynamic_loader::Loader>,
    overlay: &gtk_dynamic_loader::Overlay,
    editor: &gtk::Entry,
    text: &str,
    rng: &mut Lcg,
) {
    editor.set_text(text);
    editor.set_size_request(150 + rng.range_i32(90), 28 + rng.range_i32(18));
    position_editor(loader, editor, 12 + rng.range_i32(260), 12 + rng.range_i32(180), true);
    overlay.add_overlay(editor);
    overlay.set_overlay_pass_through(editor, false);
    set_widget_visible(loader, *editor.as_ref(), true);
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
    win.set_default_size(720, 420);

    let root = app.create_box(gtk::Orientation::Vertical, 6)?;
    let status = app.create_label("starting lifecycle stress")?;
    let summary = app.create_label("")?;
    let formula = app.create_entry()?;
    formula.set_text("formula");
    formula.set_width_chars(24);
    root.append(&status);
    root.append(&formula);

    let drawing = gtk_dynamic_loader::DrawingArea::new(loader.clone())?;
    let drawing_ptr = *drawing.as_ref();
    drawing.set_size_request(640, 260);
    if loader.symbols.gtk_container_add.is_none() {
        drawing.set_content_width(640);
        drawing.set_content_height(260);
    }

    let overlay = gtk_dynamic_loader::Overlay::new(loader.clone())?;
    overlay.set_child(&drawing);

    let scrolled = gtk_dynamic_loader::ScrolledWindow::new(loader.clone())?;
    scrolled.set_policy(0, 0);
    scrolled.set_child(&overlay);
    set_widget_expand(&loader, *scrolled.as_ref(), true, true);
    root.append(&scrolled);
    root.append(&summary);
    win.set_child(&root);
    win.present();

    let rng = Rc::new(RefCell::new(Lcg::new(cfg.seed)));
    let editor = Rc::new(RefCell::new(None::<gtk::Entry>));
    let step = Rc::new(Cell::new(0usize));
    let counts = Rc::new(RefCell::new([0usize; 8]));
    let active_entry_id = Rc::new(Cell::new(0u64));
    let current_focus_entry_id = Rc::new(Cell::new(0u64));
    let pending_detach = Rc::new(Cell::new(false));
    let retired_entries = Rc::new(RefCell::new(Vec::<gtk::Entry>::new()));

    let runner: Rc<RefCell<Option<Box<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let runner_for_init = runner.clone();
    let loader_for_run = loader.clone();
    let overlay_for_run = overlay.clone();
    let status_for_run = status.clone();
    let summary_for_run = summary.clone();
    let formula_for_run = formula.clone();
    let rng_for_run = rng.clone();
    let editor_for_run = editor.clone();
    let active_entry_id_for_run = active_entry_id.clone();
    let current_focus_entry_id_for_run = current_focus_entry_id.clone();
    let pending_detach_for_run = pending_detach.clone();
    let retired_entries_for_run = retired_entries.clone();
    let step_for_run = step.clone();
    let counts_for_run = counts.clone();

    *runner.borrow_mut() = Some(Box::new(move || {
        let current = step_for_run.get();
        if current >= cfg.steps {
            let counts = counts_for_run.borrow();
            let editor_alive = editor_for_run.borrow().is_some();
            println!(
                "completed seed={} steps={} editor_alive={} spawn={} mutate={} hide={} show={} remove={} redraw={} labels={} focus={}",
                cfg.seed,
                cfg.steps,
                editor_alive,
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4],
                counts[5],
                counts[6],
                counts[7],
            );
            let _ = gtk::quit_main_loop();
            return;
        }

        let op = {
            let mut rng = rng_for_run.borrow_mut();
            Op::from_index(rng.next_u32())
        };
        step_for_run.set(current + 1);
        counts_for_run.borrow_mut()[op as usize] += 1;

        match op {
            Op::SpawnEditor => {
                if editor_for_run.borrow().is_none() && !pending_detach_for_run.get() {
                    if let Ok(entry) = gtk::create_entry() {
                        let entry_id = active_entry_id_for_run.get() + 1;
                        active_entry_id_for_run.set(entry_id);
                        let trace = cfg.trace;
                        let _ = entry.connect_focus_in_event(move |_| {
                            if trace {
                                eprintln!("trace entry#{entry_id} focus-in");
                            }
                            0
                        });
                        let trace = cfg.trace;
                        let _ = entry.connect_focus_out_event(move |_| {
                            if trace {
                                eprintln!("trace entry#{entry_id} focus-out");
                            }
                            0
                        });
                        let _ = entry.connect_activate(|_| {});
                        let text = format!("seed:{} step:{}", cfg.seed, current);
                        let mut rng = rng_for_run.borrow_mut();
                        attach_editor(&loader_for_run, &overlay_for_run, &entry, &text, &mut rng);
                        current_focus_entry_id_for_run.set(entry_id);
                        let should_focus = rng.next_bool();
                        if cfg.trace {
                            eprintln!("trace entry#{entry_id} spawned should_focus={should_focus}");
                        }
                        if should_focus {
                            schedule_entry_focus(
                                &loader_for_run,
                                entry.clone(),
                                &current_focus_entry_id_for_run,
                                entry_id,
                            );
                        }
                        *editor_for_run.borrow_mut() = Some(entry);
                    }
                }
            }
            Op::MutateEditor => {
                if let Some(entry) = editor_for_run.borrow().as_ref() {
                    let mut rng = rng_for_run.borrow_mut();
                    let text = format!("mut:{}:{}", current, rng.next_u32() % 1000);
                    entry.set_text(&text);
                    entry.set_size_request(150 + rng.range_i32(70), 28 + rng.range_i32(18));
                    position_editor(&loader_for_run, entry, 8 + rng.range_i32(280), 8 + rng.range_i32(190), true);
                }
            }
            Op::HideEditor => {
                if let Some(entry) = editor_for_run.borrow().as_ref() {
                    set_widget_visible(&loader_for_run, *entry.as_ref(), false);
                }
            }
            Op::ShowEditor => {
                if let Some(entry) = editor_for_run.borrow().as_ref() {
                    set_widget_visible(&loader_for_run, *entry.as_ref(), true);
                }
            }
            Op::RemoveEditor => {
                if let Some(entry) = editor_for_run.borrow_mut().take() {
                    let use_overlay_remove = rng_for_run.borrow_mut().next_bool();
                    if cfg.trace {
                        let entry_id = active_entry_id_for_run.get();
                        eprintln!("trace entry#{entry_id} remove use_overlay_remove={use_overlay_remove}");
                    }
                    schedule_editor_remove(
                        &loader_for_run,
                        &overlay_for_run,
                        &formula_for_run,
                        entry,
                        use_overlay_remove,
                        &pending_detach_for_run,
                        &current_focus_entry_id_for_run,
                        &retired_entries_for_run,
                    );
                }
            }
            Op::QueueRedraw => {
                queue_widget_draw(&loader_for_run, drawing_ptr);
                set_widget_visible(&loader_for_run, drawing_ptr, rng_for_run.borrow_mut().next_bool());
                set_widget_visible(&loader_for_run, drawing_ptr, true);
            }
            Op::MutateLabels => {
                let next = rng_for_run.borrow_mut().next_u32() % 10_000;
                status_for_run.set_text(&format!("op={} step={}", op.as_str(), current));
                summary_for_run.set_text(&format!("seed={} next={}", cfg.seed, next));
                formula_for_run.set_text(&format!("={}+{}", current, next));
            }
            Op::FocusShuffle => {
                if let Some(entry) = editor_for_run.borrow().as_ref() {
                    if rng_for_run.borrow_mut().next_bool() {
                        schedule_entry_focus(
                            &loader_for_run,
                            entry.clone(),
                            &current_focus_entry_id_for_run,
                            current_focus_entry_id_for_run.get(),
                        );
                    } else {
                        schedule_formula_focus(&loader_for_run, formula_for_run.clone());
                    }
                } else {
                    schedule_formula_focus(&loader_for_run, formula_for_run.clone());
                }
            }
        }

        if cfg.verbose {
            println!("step={} op={}", current, op.as_str());
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

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
