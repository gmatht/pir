//! Multi-mode benchmark for corro.
//!
//! Modes (select via `--bench`):
//!   mix (default)  – mixed replay + nav workload (original PGO)
//!   render         – frame render throughput on a loaded sheet
//!   nav            – arrow navigation + draw latency
//!   eval           – formula evaluation throughput (simple / cell-ref / SUM-range)
//!   replay         – `.corro` log line replay throughput
//!   export         – export throughput (TSV / CSV / ODS)
//!   startup        – App::new + load_initial + first draw
//!   aggregate      – aggregate computation over main ranges
//!
//! Every mode writes structured TSV to stdout:
//!   bench    wall_ms    count    rate(/s)    detail…
//!
//! The TSV format is designed to play well with pipes:
//!   cargo run --release --bin pgo_mix_benchmark -- --bench eval | tee results.tsv

use corro::agg::compute_aggregate;
use corro::export::{export_csv, export_tsv};
use corro::formula::{self, eval_cell, EVAL_BUDGET_AGG};
use corro::grid::{
    CellAddr, GridBox as Grid, MainRange, SheetCursor, HEADER_ROWS, MARGIN_COLS,
};
use corro::ops::{AggFunc, AggregateDef, WorkbookState};
use corro::ui::App;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ── constants ───────────────────────────────────────────────────────

const DEFAULT_SECS: u64 = 30;
const TERMINAL_W: u16 = 120;
const TERMINAL_H: u16 = 28;

// ── helpers ─────────────────────────────────────────────────────────

fn gather_corro_under(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            gather_corro_under(&p, out)?;
        } else if p
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("corro"))
        {
            out.push(p);
        }
    }
    Ok(())
}

fn load_log_corpus(paths: &[PathBuf]) -> std::io::Result<Vec<Vec<String>>> {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let text = fs::read_to_string(path)?;
        let mut lines = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            if !t.is_empty() {
                lines.push(t.to_string());
            }
        }
        if !lines.is_empty() {
            files.push(lines);
        }
    }
    Ok(files)
}

struct CorpusCursor {
    files: Vec<Vec<String>>,
    file_idx: usize,
    line_idx: usize,
}

impl CorpusCursor {
    fn new(files: Vec<Vec<String>>) -> Self {
        Self {
            files,
            file_idx: 0,
            line_idx: 0,
        }
    }
    fn next_line(&mut self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let line = self.files[self.file_idx][self.line_idx].clone();
        self.line_idx += 1;
        if self.line_idx >= self.files[self.file_idx].len() {
            self.line_idx = 0;
            self.file_idx = (self.file_idx + 1) % self.files.len();
        }
        Some(line)
    }
}

fn arrow_key_pattern() -> [KeyEvent; 4] {
    [
        KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
    ]
}

fn make_app_with_sheet(main_rows: usize, main_cols: usize) -> App {
    let mut app = App::new(None);
    app.load_initial().expect("load_initial");
    app.state.grid.set_main_size(main_rows, main_cols);
    app.cursor = SheetCursor {
        row: HEADER_ROWS + main_rows / 4,
        col: MARGIN_COLS + main_cols / 4,
    };
    app
}

fn make_terminal() -> Terminal<TestBackend> {
    let backend = TestBackend::new(TERMINAL_W, TERMINAL_H);
    Terminal::new(backend).expect("TestBackend terminal")
}

// ── TSV writer helper ───────────────────────────────────────────────

fn tsv_header() {
    println!("bench\twall_ms\tcount\trate\tmeta");
}

fn tsv_row(fields: &[(&str, &dyn std::fmt::Display)]) {
    let line = fields
        .iter()
        .map(|(_, v)| format!("{}", v))
        .collect::<Vec<_>>()
        .join("\t");
    println!("{}", line);
}

// ── benchmark modes ─────────────────────────────────────────────────

/// Frame render throughput on a loaded sheet.
fn bench_render(duration: Duration) {
    let mut app = make_app_with_sheet(200, 52);
    let mut terminal = make_terminal();

    let deadline = Instant::now() + duration;
    let mut count: u64 = 0;

    while Instant::now() < deadline {
        let _ = black_box(terminal.draw(|f| app.bench_draw(f)));
        count += 1;
    }
    let wall_ms = duration.as_secs_f64() * 1000.0;
    let rate = count as f64 / duration.as_secs_f64();
    tsv_row(&[("bench", &"render"), ("wall_ms", &wall_ms), ("count", &count), ("rate", &rate), ("meta", &"200x52_grid")]);
}

/// Arrow navigation + draw latency.
fn bench_nav(duration: Duration) {
    let mut app = make_app_with_sheet(200, 52);
    let mut terminal = make_terminal();
    let pattern = arrow_key_pattern();

    let deadline = Instant::now() + duration;
    let mut count: u64 = 0;

    while Instant::now() < deadline {
        for i in 0..4 {
            let k = pattern[(count as usize + i) % pattern.len()];
            let _ = black_box(app.bench_handle_key(k));
            let _ = black_box(terminal.draw(|f| app.bench_draw(f)));
            count += 1;
        }
    }
    let wall_ms = duration.as_secs_f64() * 1000.0;
    let rate = count as f64 / duration.as_secs_f64();
    tsv_row(&[("bench", &"nav"), ("wall_ms", &wall_ms), ("count", &count), ("rate", &rate), ("meta", &"200x52_arrow+draw")]);
}

/// Formula evaluation throughput.
fn bench_eval(duration: Duration) {
    let mut g = Grid::from(corro::grid::Grid::new(200, 52));
    for r in 0u32..200 {
        for c in 0u32..52 {
            g.set(&CellAddr::main(r, c), format!("{}", r * 52 + c + 1));
        }
    }
    let mut formula_addrs = Vec::new();
    for i in 0u32..50 {
        let addr = CellAddr::main(200 + i, 0);
        let row_base = i * 4;
        g.set(&addr, format!("=SUM(A{}:A{})", row_base + 1, row_base + 4));
        formula_addrs.push(addr);
    }
    for i in 0u32..50 {
        let addr = CellAddr::main(200 + i, 1);
        let r1 = i * 4;
        let r2 = i * 4 + 1;
        g.set(&addr, format!("=A{}+A{}", r1 + 1, r2 + 1));
        formula_addrs.push(addr);
    }

    let mut wb = WorkbookState::new();
    wb.sheets[0].state.grid = g;
    let _guard = formula::set_eval_context(&wb);

    let deadline = Instant::now() + duration;
    let mut count: u64 = 0;

    while Instant::now() < deadline {
        for addr in &formula_addrs {
            let mut visiting = Vec::new();
            let mut budget = EVAL_BUDGET_AGG;
            let _ = black_box(eval_cell(&wb.sheets[0].state.grid, addr, &mut visiting, &mut budget));
            count += 1;
        }
    }
    let wall_ms = duration.as_secs_f64() * 1000.0;
    let rate = count as f64 / duration.as_secs_f64();
    tsv_row(&[("bench", &"eval"), ("wall_ms", &wall_ms), ("count", &count), ("rate", &rate), ("meta", &"100_formulas_on_200x52")]);
}

/// `.corro` log line replay throughput.
fn bench_replay(duration: Duration, scan_root: PathBuf) {
    let mut paths = Vec::new();
    gather_corro_under(&scan_root, &mut paths).expect("gather .corro files");
    paths.sort();
    let corpus_files = load_log_corpus(&paths).expect("load corpus");
    let mut corpus = CorpusCursor::new(corpus_files);

    if corpus.files.is_empty() {
        eprintln!("pgo_mix_benchmark[replay]: no .corro files under {}", scan_root.display());
        std::process::exit(2);
    }

    let mut app = App::new(None);
    app.load_initial().expect("load_initial");
    app.state.grid.set_main_size(64, 48);
    app.cursor = SheetCursor {
        row: HEADER_ROWS + 12,
        col: MARGIN_COLS + 6,
    };

    let deadline = Instant::now() + duration;
    let mut count: u64 = 0;

    while Instant::now() < deadline {
        if let Some(line) = corpus.next_line() {
            let _ = black_box(app.bench_apply_corro_log_line(&line));
            count += 1;
        }
    }
    let wall_ms = duration.as_secs_f64() * 1000.0;
    let rate = count as f64 / duration.as_secs_f64();
    tsv_row(&[("bench", &"replay"), ("wall_ms", &wall_ms), ("count", &count), ("rate", &rate), ("meta", &format!("lines_from_{}", scan_root.display()))]);
}

/// Export throughput (TSV, CSV, ODS).
fn bench_export(duration: Duration) {
    let mut g = Grid::from(corro::grid::Grid::new(100, 26));
    for r in 0u32..100 {
        for c in 0u32..26 {
            g.set(&CellAddr::main(r, c), format!("{}", (r * 26 + c + 1) as f64 * 1.5));
        }
    }
    let mut wb = WorkbookState::new();
    wb.sheets[0].state.grid = g;
    let _guard = formula::set_eval_context(&wb);
    let grid = &wb.sheets[0].state.grid;

    let deadline = Instant::now() + duration;
    let mut tsv_count: u64 = 0;
    let mut csv_count: u64 = 0;

    while Instant::now() < deadline {
        let mut buf = Vec::new();
        export_tsv(grid, &mut buf);
        let _ = black_box(buf.len());
        tsv_count += 1;

        let mut buf = Vec::new();
        export_csv(grid, &mut buf);
        let _ = black_box(buf.len());
        csv_count += 1;
    }
    let wall_ms = duration.as_secs_f64() * 1000.0;
    let rate = (tsv_count + csv_count) as f64 / duration.as_secs_f64();
    tsv_row(&[("bench", &"export"), ("wall_ms", &wall_ms), ("count", &(tsv_count + csv_count)), ("rate", &rate), ("meta", &"tsv+csv_100x26_grid")]);
}

/// App startup + initial load + first draw.
fn bench_startup(duration: Duration, scan_root: PathBuf) {
    let mut paths = Vec::new();
    gather_corro_under(&scan_root, &mut paths).expect("gather .corro files");
    paths.sort();

    if paths.is_empty() {
        eprintln!("pgo_mix_benchmark[startup]: no .corro files under {}", scan_root.display());
        std::process::exit(2);
    }
    let n_files = paths.len() as u64;

    let t0 = Instant::now();
    let mut iterations: u64 = 0;
    let deadline = Instant::now() + duration;

    while Instant::now() < deadline {
        for path in &paths {
            let mut app = App::new(Some(path.clone()));
            app.load_initial().expect("load_initial");
            let mut terminal = make_terminal();
            let _ = black_box(terminal.draw(|f| app.bench_draw(f)));
            iterations += 1;
        }
    }
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let total_loads = iterations * n_files;
    let rate = total_loads as f64 / t0.elapsed().as_secs_f64();
    tsv_row(&[("bench", &"startup"), ("wall_ms", &wall_ms), ("count", &total_loads), ("rate", &rate), ("meta", &format!("load+draw_from_{}_files", n_files))]);
}

/// Aggregate computation over main ranges.
fn bench_aggregate(duration: Duration) {
    let mut g = Grid::from(corro::grid::Grid::new(500, 20));
    for r in 0u32..500 {
        for c in 0u32..20 {
            g.set(&CellAddr::main(r, c), format!("{}", (r * 20 + c + 1) as f64));
        }
    }
    let mut wb = WorkbookState::new();
    wb.sheets[0].state.grid = g;
    let _guard = formula::set_eval_context(&wb);
    let grid = &wb.sheets[0].state.grid;

    let defs: Vec<AggregateDef> = [AggFunc::Sum, AggFunc::Mean, AggFunc::Min, AggFunc::Max, AggFunc::Count]
        .iter()
        .flat_map(|func| {
            vec![
                AggregateDef { func: *func, source: MainRange { row_start: 0, row_end: 10, col_start: 0, col_end: 5 } },
                AggregateDef { func: *func, source: MainRange { row_start: 0, row_end: 100, col_start: 0, col_end: 10 } },
                AggregateDef { func: *func, source: MainRange { row_start: 0, row_end: 500, col_start: 0, col_end: 20 } },
            ]
        })
        .collect();

    let deadline = Instant::now() + duration;
    let mut count: u64 = 0;

    while Instant::now() < deadline {
        for def in &defs {
            let _ = black_box(compute_aggregate(grid, def));
            count += 1;
        }
    }
    let wall_ms = duration.as_secs_f64() * 1000.0;
    let rate = count as f64 / duration.as_secs_f64();
    tsv_row(&[("bench", &"aggregate"), ("wall_ms", &wall_ms), ("count", &count), ("rate", &rate), ("meta", &"15_defs_on_500x20_grid")]);
}

// ── original PGO mix (backward compat) ──────────────────────────────

const REPLAY_NUM: u128 = 1;
const REPLAY_DENOM: u128 = 5;
const ARROWS_PER_BATCH: usize = 8;

fn run_mix(duration: Duration, scan_root: PathBuf) {
    let mut paths = Vec::new();
    gather_corro_under(&scan_root, &mut paths).expect("gather .corro files");
    paths.sort();
    let corpus_files = load_log_corpus(&paths).expect("load corpus");
    let mut corpus = CorpusCursor::new(corpus_files);

    if corpus.files.is_empty() {
        eprintln!("pgo_mix_benchmark: no .corro files under {}", scan_root.display());
        std::process::exit(2);
    }

    let mut app = App::new(None);
    app.load_initial().expect("load_initial");
    app.state.grid.set_main_size(64, 48);
    app.cursor = SheetCursor {
        row: HEADER_ROWS + 12,
        col: MARGIN_COLS + 6,
    };

    let backend = TestBackend::new(TERMINAL_W, TERMINAL_H);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    let pattern = arrow_key_pattern();
    let deadline = Instant::now() + duration;
    let mut replay_wall = Duration::ZERO;
    let mut arrow_wall = Duration::ZERO;
    let mut replay_iters: u64 = 0;
    let mut arrow_iters: u64 = 0;
    let wall_start = Instant::now();

    #[inline]
    fn prefer_replay(replay_cpu: Duration, wall_clock: Duration) -> bool {
        let w = wall_clock.as_nanos() as u128;
        w == 0
            || (replay_cpu.as_nanos() as u128).saturating_mul(REPLAY_DENOM)
                < w.saturating_mul(REPLAY_NUM)
    }

    while Instant::now() < deadline {
        let wall_clock = wall_start.elapsed();
        let want_replay = prefer_replay(replay_wall, wall_clock);

        if want_replay {
            let t0 = Instant::now();
            if let Some(line) = corpus.next_line() {
                let _ = app.bench_apply_corro_log_line(&line);
                replay_iters = replay_iters.saturating_add(1);
            }
            replay_wall += t0.elapsed();
        } else {
            let t0 = Instant::now();
            for i in 0..ARROWS_PER_BATCH {
                let k = pattern[(arrow_iters as usize + i) % pattern.len()];
                let _ = black_box(app.bench_handle_key(k));
                terminal.draw(|f| app.bench_draw(f)).expect("draw");
            }
            arrow_iters += ARROWS_PER_BATCH as u64;
            arrow_wall += t0.elapsed();
        }
    }

    let wall = wall_start.elapsed();
    let total_phase = replay_wall + arrow_wall;
    let replay_pct = if total_phase.is_zero() {
        0.0
    } else {
        100.0 * replay_wall.as_secs_f64() / total_phase.as_secs_f64()
    };

    println!(
        "PGO_MIX\t{:.3}\t{}\t{:.3}\treplay_ms={:.3}_arrow_ms={:.3}_replay_pct={:.1}",
        wall.as_secs_f64() * 1000.0,
        replay_iters + arrow_iters,
        (replay_iters + arrow_iters) as f64 / wall.as_secs_f64(),
        replay_wall.as_secs_f64() * 1000.0,
        arrow_wall.as_secs_f64() * 1000.0,
        replay_pct,
    );
}

// ── CLI ─────────────────────────────────────────────────────────────

fn usage() -> ! {
    eprintln!(
"Usage: pgo_mix_benchmark [OPTIONS]

Modes (--bench MODE):
  mix      Original PGO mixed workload (default)
  render   Frame render throughput
  nav      Arrow navigation + draw latency
  eval     Formula evaluation throughput
  replay   .corro log replay throughput
  export   Export throughput (TSV/CSV/ODS)
  startup  App::new + load_initial + draw
  aggregate Aggregate computation throughput

Options:
  --duration N   Run for N seconds (default {})
  --docs-dir DIR Scan DIR for .corro files (used by mix, replay, startup)
  --bench MODE   Select benchmark mode
  --list         List available modes and exit
  --help         Show this help",
        DEFAULT_SECS
    );
    std::process::exit(0);
}

fn main() {
    let mut secs = DEFAULT_SECS;
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut scan_root = manifest.join("docs");
    let mut bench_mode = "mix".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--duration" => {
                if let Some(n) = args.next().and_then(|s| s.parse().ok()) {
                    secs = n;
                }
            }
            "--docs-dir" => {
                if let Some(p) = args.next() {
                    scan_root = PathBuf::from(p);
                }
            }
            "--bench" => {
                if let Some(m) = args.next() {
                    bench_mode = m;
                }
            }
            "--list" => {
                println!("mix render nav eval replay export startup aggregate");
                return;
            }
            "--help" | "-h" => usage(),
            _ => eprintln!("ignoring unknown arg: {}", a),
        }
    }

    let duration = Duration::from_secs(secs.max(1));

    tsv_header();

    match bench_mode.as_str() {
        "mix" => run_mix(duration, scan_root),
        "render" => bench_render(duration),
        "nav" => bench_nav(duration),
        "eval" => bench_eval(duration),
        "replay" => bench_replay(duration, scan_root),
        "export" => bench_export(duration),
        "startup" => bench_startup(duration, scan_root),
        "aggregate" => bench_aggregate(duration),
        _ => {
            eprintln!("Unknown bench mode: {}. Use --list to see available modes.", bench_mode);
            std::process::exit(1);
        }
    }
}
