//! Per-language lint plan for `/autoclean`.
//!
//! `/autoclean` used to hand the model one hardcoded, Rust-shaped paragraph
//! ("run `cargo build` … `cargo test` …"). That silently did nothing useful
//! in every other language: a C/C++ project got told to run `cargo`, and the
//! agent either ignored it or invented a build command. Worse, the prompt
//! could not distinguish "the linter ran and found nothing" from "there is no
//! linter here", so a missing tool read as a clean bill of health.
//!
//! This module answers three questions *before* the turn starts, from the
//! files actually in the tree rather than from guesswork:
//!
//! 1. **Which languages is this?** Detected from build manifests and source
//!    files, not from the prompt.
//! 2. **What could vet each one?** A *list* of candidates, best first — not a
//!    single canonical tool.
//! 3. **What is actually installed here?** Probed on this machine, right now.
//!
//! A language is analysed by **whichever candidates are installed**, and by as
//! many of them as apply. This matters most for C and C++, where the good
//! tools are independent: LLVM's `scan-build`, `clang-tidy`, `cppcheck` and
//! GCC's own `-fanalyzer` each catch different bugs, and a box with only `g++`
//! on it can still be statically analysed rather than being written off.
//!
//! Only when *no* candidate is installed does the language get reported as
//! unanalysed — and then as a warning plus an offer to install, never as
//! "analysis passed". Where a fallback carried the analysis, the stronger tools
//! are still offered, as an upgrade rather than a blocker.
//!
//! Deliberately dependency-free: a `command -v` probe is the only way to
//! answer "is it installed" that cannot be wrong, and shelling out to it beats
//! guessing from `$PATH` by hand.

use std::path::Path;

/// One way of analysing a language: the tool that must exist, what it is for,
/// and the command fragment that invokes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintTool {
    /// The binary that must be on `PATH` (probed with `command -v`).
    pub tool: &'static str,
    /// What this tool catches, so the model knows why it is being run.
    pub purpose: &'static str,
    /// The command fragment, joined with `; ` when several tools are used.
    pub command: &'static str,
    /// The apt package that provides it. Empty when the tool ships with the
    /// base toolchain (e.g. GCC's own analyzer), so there is nothing to fetch.
    pub apt_package: &'static str,
}

impl LintTool {
    const fn new(tool: &'static str, purpose: &'static str, command: &'static str, apt_package: &'static str) -> LintTool {
        LintTool { tool, purpose, command, apt_package }
    }

    /// Does this tool need an install to be available?
    pub fn needs_install(&self) -> bool {
        !self.apt_package.is_empty()
    }
}

/// A detected language and every tool that could vet it, best candidate first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintPlan {
    /// The detected language (`rust`, `c`, `cpp`, `python`, …).
    pub lang: &'static str,
    /// The manifest/source pattern that identified the language.
    pub detected_by: String,
    /// Candidate tools, strongest first. The first *installed* one is the
    /// primary; every installed one is run.
    pub candidates: Vec<LintTool>,
}

impl LintPlan {
    fn new(lang: &'static str, detected_by: &str, candidates: Vec<LintTool>) -> Self {
        LintPlan { lang, detected_by: detected_by.to_string(), candidates }
    }
}

/// Is `tool` an executable on `PATH`?
///
/// This is the whole "don't silently pass" guarantee: the answer comes from
/// asking the shell, so a tool that is absent (or merely not executable) is
/// reported as absent. Never inferred from the project files.
pub fn tool_available(tool: &str) -> bool {
    // `command -v` is a shell builtin, so this needs a shell. Guard against an
    // empty/odd name so it can never be read as a command injection.
    if tool.is_empty() || !tool.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)) {
        return false;
    }
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool} >/dev/null 2>&1"))
        .status();
    matches!(out, Ok(s) if s.success())
}

/// Does `dir` contain at least one file whose name is `name`?
fn has_file(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

/// Does `dir` contain a file whose extension is `ext` (e.g. `"cpp"`) at its
/// top level?
///
/// Deliberately shallow: build manifests live at the top level, and a
/// top-level sample is enough to identify a language. Recursing the whole tree
/// would be slow on a big repo and would pick up vendored/dependency code
/// that is not the project's to lint.
fn has_ext(dir: &Path, ext: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| {
        // `Path::extension()` yields the extension *without* a leading dot, so
        // the comparison is against a bare "cpp", not ".cpp" — passing the
        // dotted form silently never matches and the language goes undetected.
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case(ext))
            .unwrap_or(false)
    })
}

/// LLVM's static analyzer. Lives in `clang-tools`, a *separate* package from
/// `clang` itself — the single most common reason `scan-build` is "installed"
/// and then not found.
const SCAN_BUILD: LintTool = LintTool::new(
    "scan-build",
    "LLVM static analysis (memory safety, null derefs, leaks, use-after-free)",
    "scan-build --status-bugs <build-command>",
    "clang-tools",
);
const CLANG_TIDY: LintTool = LintTool::new(
    "clang-tidy",
    "clang-analyzer bug patterns plus modern-C++ and style rules",
    "clang-tidy -p . <sources>",
    "clang-tools",
);
const CPPCHECK: LintTool = LintTool::new(
    "cppcheck",
    "portable static analysis, catches things the compiler's parser misses",
    "cppcheck --enable=warning,style,performance --error-exitcode=1 <sources>",
    "cppcheck",
);
/// GCC ships its own static analyzer, so a box that can compile C can analyse
/// it with nothing extra installed. Weaker than LLVM's, but *available* — which
/// is the difference between "analysed" and "not analysed at all".
///
/// NB: no `-fsyntax-only`. The analyzer is a dataflow pass that only runs
/// during code generation, so `-fsyntax-only` makes `-fanalyzer` report
/// *nothing* — a clean exit that means "not checked". Verified: a deliberate
/// null dereference yields 15 lines of `-Wanalyzer-null-dereference` without
/// it and 0 lines with it. Compiling to `/dev/null` keeps it object-file-free.
const GCC_ANALYZER_C: LintTool = LintTool::new(
    "gcc",
    "GCC static analyzer (part of the compiler, so usually already present)",
    "gcc -fanalyzer -Wall -Wextra -c <sources> -o /dev/null",
    "",
);
const GCC_ANALYZER_CPP: LintTool = LintTool::new(
    "g++",
    "GCC static analyzer (part of the compiler, so usually already present)",
    "g++ -fanalyzer -Wall -Wextra -c <sources> -o /dev/null",
    "",
);

/// Detect the project's languages and, for each, the tools that could vet it.
///
/// Detection is manifest-first (a `CMakeLists.txt` is a much stronger signal
/// than a stray `.c` file) and source-file-second, so a C++ project that keeps
/// its sources in `src/` is still recognised.
pub fn detect_lints(dir: &Path) -> Vec<LintPlan> {
    let mut out: Vec<LintPlan> = Vec::new();

    if has_file(dir, "Cargo.toml") {
        out.push(LintPlan::new(
            "rust",
            "Cargo.toml",
            vec![LintTool::new(
                "cargo-clippy",
                "clippy lints, compiler warnings and the test suite",
                "cargo clippy --all-targets -- -D warnings ; cargo build 2>&1 | grep -i warning || true ; cargo test",
                "clippy",
            )],
        ));
    }

    let has_cpp_manifest = has_file(dir, "CMakeLists.txt")
        || has_file(dir, "meson.build")
        || has_file(dir, "configure.ac")
        || has_file(dir, "CMakePresets.json");
    let has_cpp_src = has_ext(dir, "cc") || has_ext(dir, "cpp") || has_ext(dir, "cxx");
    if has_cpp_manifest || has_cpp_src {
        out.push(LintPlan::new(
            "cpp",
            if has_cpp_manifest { "CMakeLists.txt" } else { "*.cpp/*.cc source" },
            vec![SCAN_BUILD, CLANG_TIDY, CPPCHECK, GCC_ANALYZER_CPP],
        ));
    } else if has_ext(dir, "c") {
        out.push(LintPlan::new(
            "c",
            "*.c source",
            vec![SCAN_BUILD, CPPCHECK, GCC_ANALYZER_C],
        ));
    }

    if has_file(dir, "pyproject.toml")
        || has_file(dir, "setup.py")
        || has_file(dir, "requirements.txt")
        || has_ext(dir, "py")
    {
        out.push(LintPlan::new(
            "python",
            if has_file(dir, "pyproject.toml") { "pyproject.toml" } else { "*.py source" },
            vec![
                LintTool::new(
                    "ruff",
                    "lint + format check + the test suite",
                    "ruff check . && ruff format --check . ; python -m pytest -q",
                    "ruff",
                ),
                LintTool::new(
                    "pyflakes",
                    "unused imports/variables and undefined names",
                    "python3 -m pyflakes <sources> ; python3 -m pytest -q",
                    "python3-pyflakes",
                ),
            ],
        ));
    }

    if has_file(dir, "package.json") {
        out.push(LintPlan::new(
            "javascript",
            "package.json",
            vec![
                LintTool::new("eslint", "lint + the test script", "eslint . ; npm test --if-present", "eslint"),
                // `npx` ships with node, so this works with no extra install.
                LintTool::new("npx", "lint via the project's own toolchain", "npx eslint . ; npm test --if-present", ""),
            ],
        ));
    }

    if has_file(dir, "go.mod") {
        out.push(LintPlan::new(
            "go",
            "go.mod",
            vec![LintTool::new(
                "go",
                "go vet, build and the test suite",
                "go vet ./... && go build ./... && go test ./...",
                "golang-go",
            )],
        ));
    }

    out
}

/// A language that *can* be analysed here: the tools that will actually run,
/// plus the stronger ones that could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedLang {
    pub lang: &'static str,
    pub detected_by: String,
    /// Installed tools, in candidate order. The first is the primary.
    pub chosen: Vec<LintTool>,
    /// Better tools that are absent — an upgrade offer, never a blocker.
    pub extras: Vec<LintTool>,
}

impl SelectedLang {
    /// The command to run: every installed tool's fragment, joined.
    pub fn command(&self) -> String {
        self.chosen.iter().map(|t| t.command).collect::<Vec<_>>().join(" ; ")
    }
}

/// A language with no analyser available at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLang {
    pub lang: &'static str,
    pub detected_by: String,
    /// Every candidate, all absent.
    pub candidates: Vec<LintTool>,
}

/// The result of matching the plan against this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintStatus {
    /// Languages with at least one installed analyser.
    pub available: Vec<SelectedLang>,
    /// Languages with none — must be surfaced, never called clean.
    pub missing: Vec<MissingLang>,
}

impl LintStatus {
    /// True when nothing at all in this project can be analysed.
    pub fn is_empty(&self) -> bool {
        self.available.is_empty() && self.missing.is_empty()
    }
}

/// Match the plan against what is installed, per language.
///
/// A language is `available` as soon as **one** candidate is installed — the
/// whole point of the candidate list. A fallback is a real analysis, not a
/// reason to declare the code unchecked.
pub fn partition(plan: Vec<LintPlan>) -> LintStatus {
    let mut available = Vec::new();
    let mut missing = Vec::new();
    for p in plan {
        let mut chosen = Vec::new();
        let mut absent = Vec::new();
        for t in p.candidates {
            if tool_available(t.tool) {
                chosen.push(t);
            } else {
                absent.push(t);
            }
        }
        if chosen.is_empty() {
            missing.push(MissingLang {
                lang: p.lang,
                detected_by: p.detected_by,
                candidates: absent,
            });
        } else {
            available.push(SelectedLang {
                lang: p.lang,
                detected_by: p.detected_by,
                chosen,
                extras: absent,
            });
        }
    }
    LintStatus { available, missing }
}

/// Analyse this project and return the matched plan.
pub fn lint_status(dir: &Path) -> LintStatus {
    partition(detect_lints(dir))
}

/// One way of measuring branch coverage: the binary that must exist, what it
/// is for, the command fragment that enforces 100%, and how to install it.
///
/// Unlike [`LintTool`], the install is a full instruction rather than an apt
/// package: coverage tools arrive via `cargo install`, `pip install` or `npx`,
/// so an apt-only field cannot name them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageTool {
    /// The binary that must be on `PATH` (probed with `command -v`).
    pub tool: &'static str,
    /// What this tool measures, including known metric limits.
    pub purpose: &'static str,
    /// The command fragment that measures branch coverage and fails below 100%.
    pub command: &'static str,
    /// How to install it. Empty when it ships with the toolchain (Go, gcov,
    /// npx), so there is nothing to fetch.
    pub install: &'static str,
}

impl CoverageTool {
    const fn new(tool: &'static str, purpose: &'static str, command: &'static str, install: &'static str) -> CoverageTool {
        CoverageTool { tool, purpose, command, install }
    }

    /// Does this tool need an install to be available?
    pub fn needs_install(&self) -> bool {
        !self.install.is_empty()
    }
}

/// A detected language and every tool that could measure its branch coverage,
/// best candidate first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoveragePlan {
    /// The detected language (`rust`, `python`, …).
    pub lang: &'static str,
    /// The manifest/source pattern that identified the language.
    pub detected_by: String,
    /// Candidate tools, strongest first.
    pub candidates: Vec<CoverageTool>,
}

impl CoveragePlan {
    fn new(lang: &'static str, detected_by: &str, candidates: Vec<CoverageTool>) -> Self {
        CoveragePlan { lang, detected_by: detected_by.to_string(), candidates }
    }
}

/// A language whose branch coverage *can* be measured here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedCoverage {
    pub lang: &'static str,
    pub detected_by: String,
    /// Installed tools, in candidate order. The first is the primary.
    pub chosen: Vec<CoverageTool>,
    /// Better tools that are absent — an upgrade offer, never a blocker.
    pub extras: Vec<CoverageTool>,
}

impl SelectedCoverage {
    /// The command to run: every installed tool's fragment, joined.
    pub fn command(&self) -> String {
        self.chosen.iter().map(|t| t.command).collect::<Vec<_>>().join(" ; ")
    }
}

/// A language with no coverage tool available at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCoverage {
    pub lang: &'static str,
    pub detected_by: String,
    /// Every candidate, all absent.
    pub candidates: Vec<CoverageTool>,
}

/// The result of matching the coverage plan against this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageStatus {
    /// Languages with at least one installed coverage tool.
    pub available: Vec<SelectedCoverage>,
    /// Languages with none — must be surfaced, never counted as covered.
    pub missing: Vec<MissingCoverage>,
}

impl CoverageStatus {
    /// True when no coverage target was recognised at all.
    pub fn is_empty(&self) -> bool {
        self.available.is_empty() && self.missing.is_empty()
    }
}

/// Detect the project's languages and, for each, the tools that could measure
/// branch coverage.
///
/// Detection mirrors [`detect_lints`] (manifest-first, then top-level source
/// files) so lint and coverage agree on which languages are present.
pub fn detect_coverage(dir: &Path) -> Vec<CoveragePlan> {
    let mut out: Vec<CoveragePlan> = Vec::new();

    if has_file(dir, "Cargo.toml") {
        out.push(CoveragePlan::new(
            "rust",
            "Cargo.toml",
            vec![
                CoverageTool::new(
                    "cargo-llvm-cov",
                    "LLVM branch coverage (requires nightly + llvm-tools-preview; stable fallback is line coverage)",
                    "cargo llvm-cov --branch --fail-under-branches 100",
                    "cargo install cargo-llvm-cov + rustup component add llvm-tools-preview --toolchain nightly",
                ),
                CoverageTool::new(
                    "cargo-tarpaulin",
                    "fallback line+branch coverage on stable Rust",
                    "cargo tarpaulin --branch --fail-under 100",
                    "cargo install cargo-tarpaulin",
                ),
            ],
        ));
    }

    if has_file(dir, "pyproject.toml")
        || has_file(dir, "setup.py")
        || has_file(dir, "requirements.txt")
        || has_ext(dir, "py")
    {
        out.push(CoveragePlan::new(
            "python",
            if has_file(dir, "pyproject.toml") { "pyproject.toml" } else { "*.py source" },
            vec![
                CoverageTool::new(
                    "coverage",
                    "branch coverage measurement + report",
                    "coverage run --branch -m pytest -q && coverage report -m --fail-under=100",
                    "pip install coverage pytest",
                ),
                CoverageTool::new(
                    "pytest",
                    "branch coverage via the pytest-cov plugin",
                    "pytest --cov=. --cov-branch --cov-report=term-missing --fail-under=100 -q",
                    "pip install pytest pytest-cov",
                ),
            ],
        ));
    }

    if has_file(dir, "package.json") {
        out.push(CoveragePlan::new(
            "javascript",
            "package.json",
            vec![CoverageTool::new(
                "npx",
                "V8 branch coverage via c8, fetched on demand (Go-style: no install needed)",
                "npx --yes c8 --100 npm test",
                "",
            )],
        ));
    }

    if has_file(dir, "go.mod") {
        out.push(CoveragePlan::new(
            "go",
            "go.mod",
            vec![CoverageTool::new(
                "go",
                "statement coverage — Go has no branch metric, this is the closest available",
                "go test -coverprofile=cover.out ./... && go tool cover -func=cover.out",
                "",
            )],
        ));
    }

    let has_cpp_manifest = has_file(dir, "CMakeLists.txt")
        || has_file(dir, "meson.build")
        || has_file(dir, "configure.ac")
        || has_file(dir, "CMakePresets.json");
    let has_cpp_src = has_ext(dir, "cc") || has_ext(dir, "cpp") || has_ext(dir, "cxx");
    if has_cpp_manifest || has_cpp_src {
        out.push(CoveragePlan::new(
            "cpp",
            if has_cpp_manifest { "CMakeLists.txt" } else { "*.cpp/*.cc source" },
            vec![
                CoverageTool::new(
                    "gcov",
                    "GCC branch coverage (some exception-handling branches may be uncoverable — ledger them)",
                    "g++ -O0 --coverage -fprofile-arcs -ftest-coverage <sources> -o /tmp/cov_test && /tmp/cov_test && gcov -b <sources>",
                    "",
                ),
                CoverageTool::new(
                    "lcov",
                    "HTML/branch report over gcov data",
                    "lcov --capture --directory . --output-file coverage.info --rc lcov_branch_coverage=1 && lcov --list coverage.info",
                    "sudo apt-get install lcov",
                ),
            ],
        ));
    } else if has_ext(dir, "c") {
        out.push(CoveragePlan::new(
            "c",
            "*.c source",
            vec![
                CoverageTool::new(
                    "gcov",
                    "GCC branch coverage",
                    "gcc -O0 --coverage -fprofile-arcs -ftest-coverage <sources> -o /tmp/cov_test && /tmp/cov_test && gcov -b <sources>",
                    "",
                ),
                CoverageTool::new(
                    "lcov",
                    "HTML/branch report over gcov data",
                    "lcov --capture --directory . --output-file coverage.info --rc lcov_branch_coverage=1 && lcov --list coverage.info",
                    "sudo apt-get install lcov",
                ),
            ],
        ));
    }

    out
}

/// Match the coverage plan against what is installed, per language.
///
/// Same rule as [`partition`]: a language is `available` as soon as **one**
/// candidate is installed.
pub fn partition_coverage(plan: Vec<CoveragePlan>) -> CoverageStatus {
    let mut available = Vec::new();
    let mut missing = Vec::new();
    for p in plan {
        let mut chosen = Vec::new();
        let mut absent = Vec::new();
        for t in p.candidates {
            if tool_available(t.tool) {
                chosen.push(t);
            } else {
                absent.push(t);
            }
        }
        if chosen.is_empty() {
            missing.push(MissingCoverage {
                lang: p.lang,
                detected_by: p.detected_by,
                candidates: absent,
            });
        } else {
            available.push(SelectedCoverage {
                lang: p.lang,
                detected_by: p.detected_by,
                chosen,
                extras: absent,
            });
        }
    }
    CoverageStatus { available, missing }
}

/// Measure this project's branch-coverage readiness on this machine.
pub fn coverage_status(dir: &Path) -> CoverageStatus {
    partition_coverage(detect_coverage(dir))
}

/// Build the `/autoclean` prompt for this project.
///
/// The shape is deliberately different from the old hardcoded string:
///
/// * a numbered, per-language list of *concrete commands*, built from the
///   tools that are actually installed;
/// * an explicit statement of any language that could not be analysed, with an
///   offer to install rather than a silent skip;
/// * an explicit "do not claim a clean bill of health you did not verify"
///   rule, so "no linter installed" can never be reported as "no findings".
///
/// When `with_coverage` is set (the `/autoclean coverage` opt-in), a second
/// section is appended that drives the agent to measure branch coverage and
/// write tests until it reaches 100% where possible — with an explicit
/// uncovered-branch ledger for whatever cannot be covered.
pub fn autoclean_prompt(dir: &Path, with_coverage: bool) -> String {
    let status = lint_status(dir);

    if status.is_empty() {
        return "Autoclean this project: no recognised build manifest or source file was found in \
the working directory, so there is no known lint target. Look at the tree, work out how it is \
built and tested, run that, and fix every error, warning and failing test. If you cannot \
determine the build, say so explicitly instead of guessing. Do not commit anything."
            .to_string();
    }

    let mut s = String::from(
        "Autoclean this project: fix every build error, every warning, every static-analysis \
finding and every failing test, then re-run and repeat until everything is clean. Do not \
commit anything.\n",
    );

    if !status.available.is_empty() {
        s.push_str("\nRun these, fix everything they report, and re-run until each is silent:\n");
        for (i, l) in status.available.iter().enumerate() {
            s.push_str(&format!(
                "\n{}. {lang} (detected via {by}) — analysing with {tools}:\n   {cmd}\n",
                i + 1,
                lang = l.lang,
                by = l.detected_by,
                tools = l
                    .chosen
                    .iter()
                    .map(|t| format!("`{}` ({})", t.tool, t.purpose))
                    .collect::<Vec<_>>()
                    .join(", "),
                cmd = l.command(),
            ));
        }
    } else {
        s.push_str(
            "\nNo analyser for this project is installed yet, so nothing has been analysed. Deal \
with the install question below first, then analyse whatever becomes available.\n",
        );
    }

    // Stronger tools that are missing, when a fallback already carried the
    // analysis: an upgrade worth mentioning, not a blocker.
    let upgrades: Vec<(&str, &LintTool)> = status
        .available
        .iter()
        .flat_map(|l| l.extras.iter().map(move |t| (l.lang, t)))
        .filter(|(_, t)| t.needs_install())
        .collect();
    if !upgrades.is_empty() {
        s.push_str(
            "\nThese are also applicable to this project but are not installed. They catch \
different bugs from the tool(s) above, so they are worth offering — but the code is NOT \
unanalysed, so do not block on them and do not report them as a failure:\n",
        );
        for (lang, t) in &upgrades {
            s.push_str(&format!(
                "  * {lang}: `{tool}` ({purpose}) — `sudo apt-get install {apt}`\n",
                lang = lang,
                tool = t.tool,
                purpose = t.purpose,
                apt = t.apt_package,
            ));
        }
    }

    if !status.missing.is_empty() {
        s.push_str(
            "\nIMPORTANT — a linter this project needs is NOT installed, so its code has NOT been \
analysed. Do not describe that code as clean, and do not skip it silently. Instead:\n",
        );
        for l in &status.missing {
            let list = l
                .candidates
                .iter()
                .filter(|t| t.needs_install())
                .map(|t| format!("`{}` (sudo apt-get install {})", t.tool, t.apt_package))
                .collect::<Vec<_>>()
                .join(", or ");
            s.push_str(&format!(
                "  * {lang} (detected via {by}): none of {all} are available. Install one: {list}\n",
                lang = l.lang,
                by = l.detected_by,
                all = l
                    .candidates
                    .iter()
                    .map(|t| t.tool)
                    .collect::<Vec<_>>()
                    .join(" / "),
                list = list,
            ));
        }
        s.push_str(
            "\nFor each missing linter, call `ask_user_question` and ask whether to install it \
(the apt package is named above for each). If the operator agrees, use \
`request_root` with intent `apt-install` and the named package — that queues an auditable \
request; it does not escalate by itself. If the operator declines, or the install is \
unavailable, say plainly in your final message which languages went unanalysed and why. \
Never report 'no issues found' for code that was never checked.\n",
        );
    }

    s.push_str(
        "\nWhen every listed command is silent, re-run them all once more to confirm, then give a \
short summary: what you fixed, and (if any) what you could not check and why.",
    );
    if with_coverage {
        append_coverage_section(dir, &mut s);
    }
    s
}

/// Append the opt-in branch-coverage section to an `/autoclean` prompt.
///
/// The section mirrors the lint half: concrete per-language commands from the
/// tools actually installed, an explicit missing-tool warning with an install
/// offer, and a no-gaming rule so "100%" always means measured branches, not
/// exclusions or lowered thresholds. Known metric limits are stated inline:
/// Go measures statements (it has no branch metric), C++ exception-handling
/// branches may be uncoverable, and Rust `--branch` needs nightly.
fn append_coverage_section(dir: &Path, s: &mut String) {
    let cov = coverage_status(dir);
    if cov.is_empty() {
        s.push_str(
            "\n\nBranch coverage was requested, but no recognised coverage target was found, \
so there is nothing to measure. Say so explicitly instead of guessing.",
        );
        return;
    }

    s.push_str(
        "\n\nBranch coverage (opt-in): drive branch coverage to 100% where possible. For each \
language below, run its coverage command, read the uncovered-branch report, write new tests \
that exercise those branches, and re-run until the gate passes. Do not game the number: do \
not add exclusions, do not lower thresholds, do not delete code to make the count. Repeat \
at most a few rounds; if 100% is unreachable, keep the best result and ledger every \
uncovered branch with its reason (dead code, defensive assert, platform-specific, C++ \
exception-handling branch, Go statement-only limit). Re-run the lint commands above once \
more at the end to confirm everything is still green.\n",
    );

    if !cov.available.is_empty() {
        s.push_str("\nMeasure with these (each fails below 100%):\n");
        for l in &cov.available {
            s.push_str(&format!(
                "\n* {lang} (detected via {by}) — measuring with {tools}:\n  {cmd}\n",
                lang = l.lang,
                by = l.detected_by,
                tools = l
                    .chosen
                    .iter()
                    .map(|t| format!("`{}` ({})", t.tool, t.purpose))
                    .collect::<Vec<_>>()
                    .join(", "),
                cmd = l.command(),
            ));
        }
        s.push_str(
            "\nRust note: `--branch` needs nightly plus the llvm-tools component; without them \
fall back to line coverage and say so. Go note: the percentage is statement coverage — \
there is no branch metric to reach. C/C++ note: build the coverage binary with `-O0` and \
run it before `gcov -b`; exception-handling branches that cannot be hit go in the ledger.",
        );
    } else {
        s.push_str(
            "\nNo coverage tool for this project is installed yet, so no branch coverage has been \
measured. Deal with the install question below first, then measure whatever becomes available.\n",
        );
    }

    let upgrades: Vec<(&str, &CoverageTool)> = cov
        .available
        .iter()
        .flat_map(|l| l.extras.iter().map(move |t| (l.lang, t)))
        .filter(|(_, t)| t.needs_install())
        .collect();
    if !upgrades.is_empty() {
        s.push_str(
            "\nThese coverage tools are also applicable but are not installed — an upgrade worth \
offering, not a blocker:\n",
        );
        for (lang, t) in &upgrades {
            s.push_str(&format!(
                "  * {lang}: `{tool}` ({purpose}) — `{install}`\n",
                lang = lang,
                tool = t.tool,
                purpose = t.purpose,
                install = t.install,
            ));
        }
    }

    if !cov.missing.is_empty() {
        s.push_str(
            "\nIMPORTANT — a coverage tool this project needs is NOT installed, so its branch \
coverage has NOT been measured. Do not describe that code as fully covered, and do not skip \
it silently. Instead:\n",
        );
        for l in &cov.missing {
            let list = l
                .candidates
                .iter()
                .filter(|t| t.needs_install())
                .map(|t| format!("`{}` ({})", t.tool, t.install))
                .collect::<Vec<_>>()
                .join(", or ");
            let how = if list.is_empty() {
                "ships with the toolchain".to_string()
            } else {
                format!("Install one: {list}")
            };
            s.push_str(&format!(
                "  * {lang} (detected via {by}): none of {all} are available. {how}\n",
                lang = l.lang,
                by = l.detected_by,
                all = l
                    .candidates
                    .iter()
                    .map(|t| t.tool)
                    .collect::<Vec<_>>()
                    .join(" / "),
                how = how,
            ));
        }
        s.push_str(
            "\nFor each missing coverage tool, call `ask_user_question` and ask whether to install \
it (the install instruction is named above for each). If the operator agrees, use \
`request_root` with intent `apt-install` and the named package — that queues an auditable \
request; it does not escalate by itself. If the operator declines, or the install is \
unavailable, say plainly in your final message which languages went unmeasured and why. \
Never report '100% covered' for code that was never measured.\n",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch dir that cleans itself up, so detection can be tested against
    /// real files on disk rather than a mocked filesystem.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!("pir-lints-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("mkdir");
            Tmp(p)
        }
        fn write(&self, name: &str) -> &Tmp {
            std::fs::write(self.0.join(name), b"").expect("write");
            self
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Pick a plan entry for `lang`, failing loudly if detection missed it.
    fn plan_for(dir: &Path, lang: &str) -> LintPlan {
        detect_lints(dir)
            .into_iter()
            .find(|p| p.lang == lang)
            .unwrap_or_else(|| panic!("{lang} not detected"))
    }

    /// A tool that certainly exists on any unix box, and one that certainly
    /// does not — the availability probe must tell them apart.
    #[test]
    fn availability_probe_agrees_with_the_shell() {
        assert!(tool_available("sh"), "sh is always installed");
        assert!(!tool_available("pir-definitely-not-a-real-tool-xyz"),
                "a nonexistent tool must not be reported as available");
    }

    /// The probe interpolates into a `sh -c` string, so it must refuse
    /// anything that is not a plain binary name rather than executing it.
    #[test]
    fn availability_probe_refuses_shell_metacharacters() {
        for bad in ["", "a; rm -rf /", "x y", "$(id)", "../../bin/sh"] {
            assert!(!tool_available(bad), "must refuse {bad:?}");
        }
    }

    /// A C++ project is recognised, and LLVM's static analysis leads the
    /// candidate list.
    #[test]
    fn detects_cpp_and_offers_llvm_static_analysis_first() {
        let t = Tmp::new("cpp");
        t.write("CMakeLists.txt").write("main.cpp");
        let cpp = plan_for(&t.0, "cpp");
        assert_eq!(cpp.candidates[0].tool, "scan-build", "LLVM static analysis leads");
        assert_eq!(cpp.candidates[0].apt_package, "clang-tools");
        let tools: Vec<&str> = cpp.candidates.iter().map(|t| t.tool).collect();
        assert!(tools.contains(&"clang-tidy"));
        assert!(tools.contains(&"cppcheck"));
        assert_eq!(cpp.detected_by, "CMakeLists.txt");
    }

    /// A bare `.c` tree with no manifest is still C, not silently "unknown".
    #[test]
    fn detects_plain_c_from_source_files() {
        let t = Tmp::new("c");
        t.write("main.c");
        let c = plan_for(&t.0, "c");
        let tools: Vec<&str> = c.candidates.iter().map(|t| t.tool).collect();
        assert!(tools.contains(&"scan-build") && tools.contains(&"cppcheck"));
    }

    /// A `.cpp` source and a `.c` source together are C++, not two languages.
    #[test]
    fn cpp_wins_over_c_when_both_sources_exist() {
        let t = Tmp::new("both");
        t.write("main.c").write("main.cpp");
        let plan = detect_lints(&t.0);
        assert!(plan.iter().any(|p| p.lang == "cpp"));
        assert!(!plan.iter().any(|p| p.lang == "c"), "must not double-count");
    }

    /// The heart of the change: a language with *any* installed candidate is
    /// analysed, and the run uses every installed candidate, not just the
    /// first. This is what stops a box that only has `g++` from being written
    /// off as unanalysed.
    #[test]
    fn a_language_is_analysed_by_whichever_candidates_exist() {
        let t = Tmp::new("pick");
        t.write("CMakeLists.txt").write("main.cpp");
        let st = partition(detect_lints(&t.0));
        let cpp = st.available.iter().find(|l| l.lang == "cpp");
        let missing = st.missing.iter().find(|l| l.lang == "cpp");
        if let Some(c) = cpp {
            assert!(!c.chosen.is_empty());
            for tool in &c.chosen {
                assert!(tool_available(tool.tool), "{} chosen but not installed", tool.tool);
            }
            // The composed command must carry every chosen tool's fragment.
            let cmd = c.command();
            for tool in &c.chosen {
                assert!(cmd.contains(tool.command), "command missing {}", tool.tool);
            }
            // The primary is the strongest installed one, in candidate order:
            // it is the earliest entry of the original list that survived the
            // availability filter, so it must precede every other chosen tool.
            let order: Vec<&str> = plan_for(&t.0, "cpp")
                .candidates
                .iter()
                .map(|x| x.tool)
                .collect();
            let first = c.chosen[0].tool;
            let earliest = order
                .iter()
                .position(|x| *x == first)
                .expect("chosen tool came from the candidate list");
            for other in &c.chosen[1..] {
                assert!(
                    order.iter().position(|x| *x == other.tool).unwrap() > earliest,
                    "{} was preferred over the stronger {}",
                    other.tool,
                    first
                );
            }
        } else {
            // Nothing installed: it must be reported, not silently dropped.
            let m = missing.expect("either analysed or reported missing");
            assert!(m.candidates.iter().all(|t| !tool_available(t.tool)));
        }
    }

    /// GCC's analyzer needs no package (it is part of the compiler), so it must
    /// be offered as an analysis that works with nothing installed.
    #[test]
    fn the_compiler_builtin_fallback_needs_no_install() {
        let d = Tmp::new("gcc");
        d.write("main.c");
        let c = plan_for(&d.0, "c");
        let gcc = c.candidates.iter().find(|t| t.tool == "gcc").expect("gcc fallback offered");
        assert!(!gcc.needs_install(), "gcc ships with the compiler — nothing to apt-get");
        assert!(gcc.command.contains("-fanalyzer"));
    }

    /// Every detected language's candidates are probed, and anything reported
    /// missing really is missing — the invariant that stops a silent pass.
    #[test]
    fn missing_linters_are_separated_from_available_ones() {
        let t = Tmp::new("part");
        t.write("Cargo.toml");
        let st = partition(detect_lints(&t.0));
        for l in &st.available {
            for e in &l.extras {
                assert!(!tool_available(e.tool), "{} installed but listed as an extra", e.tool);
            }
        }
        for l in &st.missing {
            for c in &l.candidates {
                assert!(!tool_available(c.tool), "{} installed but reported missing", c.tool);
            }
        }
    }

    /// A fallback must NOT trigger the "NOT installed, unanalysed" alarm: the
    /// code *was* analysed. Only genuinely unanalysable languages do.
    #[test]
    fn a_fallback_keeps_the_language_out_of_the_unanalysed_bucket() {
        let t = Tmp::new("fallback");
        t.write("CMakeLists.txt").write("main.cpp");
        let p = autoclean_prompt(&t.0, false);
        let st = lint_status(&t.0);
        let cpp_available = st.available.iter().any(|l| l.lang == "cpp");
        if cpp_available {
            assert!(
                !p.contains("its code has NOT been analysed") || !st.missing.iter().any(|m| m.lang == "cpp"),
                "a fallback analysed the code — it must not be reported unanalysed"
            );
        }
    }

    /// The prompt must state the *absence* of any analyser explicitly, so the
    /// model is told not to call unanalysed code clean.
    #[test]
    fn prompt_tells_the_model_not_to_claim_unanalysed_code_is_clean() {
        let t = Tmp::new("prompt");
        t.write("CMakeLists.txt").write("main.cpp");
        let p = autoclean_prompt(&t.0, false);
        let cpp_unanalysed = lint_status(&t.0).missing.iter().any(|m| m.lang == "cpp");
        if cpp_unanalysed {
            assert!(p.contains("NOT installed"), "must flag it: {p}");
            assert!(p.contains("apt-install"), "must offer the install path");
            assert!(p.contains("ask_user_question"), "must ask before installing");
            assert!(p.contains("Never report 'no issues found'"), "must forbid a false clean bill");
        } else {
            // Analysed: the prompt must name a command that will actually run.
            assert!(p.contains("scan-build") || p.contains("-fanalyzer") || p.contains("cppcheck")
                    || p.contains("clang-tidy"), "must name a runnable analyser: {p}");
        }
    }

    /// A tree with nothing recognisable must not invent a plan.
    #[test]
    fn an_unrecognised_tree_says_so_instead_of_guessing() {
        let t = Tmp::new("empty");
        let p = autoclean_prompt(&t.0, false);
        assert!(p.contains("no recognised build manifest"), "got: {p}");
    }

    /// The compiler_builtin_fallback_needs_no_install test asserts the *string*.
    /// This asserts the command actually works: a deliberate null dereference
    /// must be reported.
    ///
    /// This is the test that catches `-fsyntax-only`, which suppresses GCC's
    /// dataflow pass entirely — the command then exits clean and the code is
    /// reported as analysed when it was never actually checked, which is the
    /// exact failure this module exists to prevent. Skipped where no GCC is
    /// present.
    ///
    /// **Linux-only**, despite the `tool_available("gcc")` guard: on macOS
    /// `/usr/bin/gcc` is a shim for Apple clang, which has no `-fanalyzer` at
    /// all (`clang: error: unknown argument: '-fanalyzer'`). The guard passes,
    /// the analyzer never runs, and the test fails on a clean compile — which
    /// is this module's own failure signature, reported for the wrong reason.
    /// The command being verified is GCC's; without GCC there is nothing to
    /// verify.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_gcc_fallback_command_actually_reports_a_real_bug() {
        if !tool_available("gcc") {
            eprintln!("skipping: no gcc on this host");
            return;
        }
        let d = Tmp::new("fanalyzer");
        d.write("bug.c");
        std::fs::write(
            d.0.join("bug.c"),
            b"int deref(int *p) { return *p; }\nint main(void) { return deref(0); }\n",
        )
        .unwrap();
        let plan = plan_for(&d.0, "c");
        let gcc = plan.candidates.iter().find(|t| t.tool == "gcc").expect("gcc candidate");
        // Run the real command, with `<sources>` filled in and /dev/null
        // substituted, so the test exercises the command as documented.
        let cmd = gcc
            .command
            .replace("<sources>", &format!("{}", d.0.join("bug.c").display()));
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .expect("run gcc");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(
            text.contains("analyzer-null-dereference") || text.contains("null-dereference"),
            "the fallback must actually catch a null dereference, but reported nothing.\n\
             command was: {cmd}\nstderr: {text}\n\
             (a clean run here means the analyzer was disabled, not that the code is clean)"
        );
    }

    /// Rust is still the first-class citizen it always was — this must not
    /// regress the case `/autoclean` originally existed for.
    #[test]
    fn rust_projects_still_get_build_test_and_clippy() {
        let t = Tmp::new("rust");
        t.write("Cargo.toml").write("main.rs");
        let p = autoclean_prompt(&t.0, false);
        assert!(p.contains("cargo build"), "must still build");
        assert!(p.contains("cargo test"), "must still test");
        assert!(p.contains("clippy"), "must now also lint");
    }

    /// Pick a coverage plan entry for `lang`, failing loudly if missed.
    fn cov_for(dir: &Path, lang: &str) -> CoveragePlan {
        detect_coverage(dir)
            .into_iter()
            .find(|p| p.lang == lang)
            .unwrap_or_else(|| panic!("{lang} coverage not detected"))
    }

    /// Rust coverage leads with LLVM branch coverage and names the install.
    #[test]
    fn coverage_detects_rust_and_leads_with_llvm_branch_coverage() {
        let t = Tmp::new("covrust");
        t.write("Cargo.toml").write("main.rs");
        let rust = cov_for(&t.0, "rust");
        assert_eq!(rust.candidates[0].tool, "cargo-llvm-cov");
        assert!(rust.candidates[0].command.contains("--branch"));
        assert!(rust.candidates[0].command.contains("100"));
        assert!(rust.candidates[0].needs_install());
    }

    /// Python coverage must carry the branch flag and a 100 gate.
    #[test]
    fn coverage_python_measures_branches_and_gates_at_100() {
        let t = Tmp::new("covpy");
        t.write("pyproject.toml");
        let py = cov_for(&t.0, "python");
        assert!(py.candidates.iter().any(|c| c.command.contains("--branch")),
                "at least one python candidate must pass --branch");
        assert!(py.candidates.iter().any(|c| c.command.contains("100")),
                "at least one python candidate must gate at 100");
    }

    /// JS coverage goes through npx/c8 with a 100 gate and needs no install
    /// (npx ships with node and fetches c8 on demand).
    #[test]
    fn coverage_js_uses_c8_100_via_npx_with_no_install() {
        let t = Tmp::new("covjs");
        t.write("package.json");
        let js = cov_for(&t.0, "javascript");
        let c8 = js.candidates.iter().find(|c| c.tool == "npx").expect("npx candidate");
        assert!(c8.command.contains("c8") && c8.command.contains("--100"), "got: {}", c8.command);
        assert!(!c8.needs_install(), "npx ships with node — nothing to install");
    }

    /// Go has no branch metric: the candidate must say statement-only and use
    /// the coverprofile workflow.
    #[test]
    fn coverage_go_is_statement_only_via_coverprofile() {
        let t = Tmp::new("covgo");
        t.write("go.mod");
        let go = cov_for(&t.0, "go");
        let g = &go.candidates[0];
        assert_eq!(g.tool, "go");
        assert!(g.command.contains("-coverprofile"), "got: {}", g.command);
        assert!(g.purpose.contains("no branch metric") || g.purpose.contains("statement"),
                "must disclose the metric limit: {}", g.purpose);
        assert!(!g.needs_install());
    }

    /// C coverage offers `gcov -b` with nothing to install.
    #[test]
    fn coverage_c_offers_gcov_branch_with_no_install() {
        let t = Tmp::new("covc");
        t.write("main.c");
        let c = cov_for(&t.0, "c");
        let gcov = c.candidates.iter().find(|x| x.tool == "gcov").expect("gcov candidate");
        assert!(gcov.command.contains("gcov -b"), "got: {}", gcov.command);
        assert!(!gcov.needs_install());
    }

    /// The opt-in flag must add a branch-coverage section, and the default
    /// path must stay byte-clean of it (existing behaviour unchanged).
    #[test]
    fn coverage_section_is_opt_in_only() {
        let t = Tmp::new("covopt");
        t.write("Cargo.toml").write("main.rs");
        let off = autoclean_prompt(&t.0, false);
        let on = autoclean_prompt(&t.0, true);
        assert!(!off.contains("Branch coverage"), "default prompt must not mention coverage");
        assert!(on.contains("Branch coverage"), "opt-in prompt must add the section: {on}");
        assert!(on.contains("100%"), "must state the target");
        assert!(on.contains("Do not game"), "must forbid gaming the number: {on}");
        assert!(on.contains("ledger"), "must demand an uncovered-branch ledger: {on}");
        assert!(on.contains("Never report '100% covered'"),
                "must forbid a false coverage bill: {on}");
    }

    /// Coverage partition obeys the same invariant as lint partition: chosen
    /// tools are installed, extras/missing are not.
    #[test]
    fn coverage_missing_tools_are_separated_from_available_ones() {
        let t = Tmp::new("covpart");
        t.write("Cargo.toml");
        let st = partition_coverage(detect_coverage(&t.0));
        for l in &st.available {
            for e in &l.extras {
                assert!(!tool_available(e.tool), "{} installed but listed as an extra", e.tool);
            }
            let cmd = l.command();
            for tool in &l.chosen {
                assert!(cmd.contains(tool.command), "command missing {}", tool.tool);
            }
        }
        for l in &st.missing {
            for c in &l.candidates {
                assert!(!tool_available(c.tool), "{} installed but reported missing", c.tool);
            }
        }
    }

    /// Lint and coverage agree on which languages exist: every lint language
    /// in a mixed tree has a coverage plan too.
    #[test]
    fn lint_and_coverage_detect_the_same_languages() {
        let t = Tmp::new("covagree");
        t.write("Cargo.toml").write("pyproject.toml").write("go.mod").write("package.json");
        let lints: Vec<&str> = detect_lints(&t.0).iter().map(|p| p.lang).collect();
        let covs: Vec<&str> = detect_coverage(&t.0).iter().map(|p| p.lang).collect();
        for lang in lints {
            // C/C++ detection needs source files, which this tree lacks — every
            // manifest-driven language must agree.
            if ["rust", "python", "javascript", "go"].contains(&lang) {
                assert!(covs.contains(&lang), "{lang} detected for lint but not coverage");
            }
        }
    }
}
