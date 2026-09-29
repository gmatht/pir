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
//! 2. **Which linter suits each?** The canonical tool for that language.
//! 3. **Is it actually installed?** Run on this machine, right now — because a
//!    plan that names a missing binary is exactly the silent-pass this fixes.
//!
//! A missing linter is reported as a *warning plus an offer to install it*
//! (via the `request_root` / `apt-install` path, which queues an auditable
//! request rather than escalating on its own). It is never presented as
//! "analysis passed".
//!
//! Deliberately dependency-free: a `command -v` probe is the only way to
//! answer "is it installed" that cannot be wrong, and shelling out to it beats
//! guessing from `$PATH` by hand.

use std::path::Path;

/// A language present in the project, with the linter that should vet it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintPlan {
    /// The detected language (`rust`, `c`, `cpp`, `python`, …).
    pub lang: &'static str,
    /// The canonical linter for that language (a binary name).
    pub tool: &'static str,
    /// How to invoke it, as a shell command line.
    pub command: String,
    /// Extra packages needed for that tool, offered to the operator.
    pub apt_package: &'static str,
    /// The manifest/source pattern that identified the language.
    pub detected_by: String,
}

impl LintPlan {
    fn new(
        lang: &'static str,
        tool: &'static str,
        command: String,
        apt_package: &'static str,
        detected_by: &str,
    ) -> Self {
        LintPlan {
            lang,
            tool,
            command,
            apt_package,
            detected_by: detected_by.to_string(),
        }
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

/// Detect the project's languages and the linter each one wants.
///
/// Detection is manifest-first (a `CMakeLists.txt` is a much stronger signal
/// than a stray `.c` file) and source-file-second, so a C++ project that keeps
/// its sources in `src/` is still recognised via its top-level sources.
pub fn detect_lints(dir: &Path) -> Vec<LintPlan> {
    let mut out: Vec<LintPlan> = Vec::new();

    // Rust. `clippy` is the linter proper; `cargo build` is what surfaces the
    // warnings, so the command runs both.
    if has_file(dir, "Cargo.toml") {
        out.push(LintPlan::new(
            "rust",
            "cargo-clippy",
            "cargo clippy --all-targets -- -D warnings && cargo build 2>&1 | grep -i warning || true; \
             cargo test"
                .to_string(),
            "clippy",
            "Cargo.toml",
        ));
    }

    // C++ / C. LLVM's static analyzer is the tool the user asked for; it comes
    // with `clang-tools`, which is a *separate* package from `clang` itself —
    // the single most common reason `scan-build` is "installed" and then not
    // found, so both are named in the offer.
    let has_cpp_manifest = has_file(dir, "CMakeLists.txt")
        || has_file(dir, "meson.build")
        || has_file(dir, "configure.ac")
        || has_file(dir, "CMakePresets.json");
    let has_cpp_src = has_ext(dir, "cc") || has_ext(dir, "cpp") || has_ext(dir, "cxx");
    let has_c_src = has_ext(dir, "c");
    if has_cpp_manifest || has_cpp_src {
        out.push(LintPlan::new(
            "cpp",
            "scan-build",
            // scan-build drives a full compile and reports every static finding;
            // clang-tidy then vets the style/clang-analyzer rules. Both are run
            // because they catch genuinely different classes of bug.
            "scan-build --status-bugs <build-command> ; clang-tidy -p . <sources>".to_string(),
            "clang-tools",
            if has_cpp_manifest { "CMakeLists.txt" } else { "*.cpp/*.cc source" },
        ));
    } else if has_c_src {
        out.push(LintPlan::new(
            "c",
            "scan-build",
            "scan-build --status-bugs <build-command> ; cppcheck --enable=warning,style --error-exitcode=1 <sources>"
                .to_string(),
            "clang-tools",
            "*.c source",
        ));
    }

    // Python.
    if has_file(dir, "pyproject.toml")
        || has_file(dir, "setup.py")
        || has_file(dir, "requirements.txt")
        || has_ext(dir, "py")
    {
        out.push(LintPlan::new(
            "python",
            "ruff",
            "ruff check . && ruff format --check . ; python -m pytest -q".to_string(),
            "ruff",
            if has_file(dir, "pyproject.toml") { "pyproject.toml" } else { "*.py source" },
        ));
    }

    // JavaScript / TypeScript.
    if has_file(dir, "package.json") {
        out.push(LintPlan::new(
            "javascript",
            "eslint",
            "npx eslint . ; npm test --if-present".to_string(),
            "eslint",
            "package.json",
        ));
    }

    // Go.
    if has_file(dir, "go.mod") {
        out.push(LintPlan::new(
            "go",
            "go",
            "go vet ./... && go build ./... && go test ./...".to_string(),
            "golang-golang-x-tools",
            "go.mod",
        ));
    }

    out
}

/// The result of checking the plan against this machine.
pub struct LintStatus {
    /// Plans whose linter is present and will run.
    pub available: Vec<LintPlan>,
    /// Plans whose linter is missing — must be surfaced, never silently
    /// treated as "clean".
    pub missing: Vec<LintPlan>,
}

/// Split the plan by what is actually installed.
pub fn partition(plan: Vec<LintPlan>) -> LintStatus {
    let mut available = Vec::new();
    let mut missing = Vec::new();
    for p in plan {
        if tool_available(p.tool) {
            available.push(p);
        } else {
            missing.push(p);
        }
    }
    LintStatus { available, missing }
}

/// Build the `/autoclean` prompt for this project.
///
/// The shape is deliberately different from the old hardcoded string:
///
/// * a numbered, per-language list of *concrete commands* to run and fix;
/// * an explicit statement of which linters are missing, with a request to
///   offer the install rather than silently skipping;
/// * an explicit "do not claim a clean bill of health you did not verify"
///   rule, so "no linter installed" can never be reported as "no findings".
pub fn autoclean_prompt(dir: &Path) -> String {
    let plan = detect_lints(dir);
    let status = partition(plan);

    if status.available.is_empty() && status.missing.is_empty() {
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
        for (i, p) in status.available.iter().enumerate() {
            s.push_str(&format!(
                "\n{}. {lang} (detected via {by}) — linter `{tool}`:\n   {cmd}\n",
                i + 1,
                lang = p.lang,
                by = p.detected_by,
                tool = p.tool,
                cmd = p.command,
            ));
        }
    } else {
        s.push_str(
            "\nNo linter for this project is installed yet, so nothing has been analysed. Deal \
with the install question below first, then analyse whatever becomes available.\n",
        );
    }

    if !status.missing.is_empty() {
        s.push_str(
            "\nIMPORTANT — a linter this project needs is NOT installed, so its code has NOT been \
analysed. Do not describe that code as clean, and do not skip it silently. Instead:\n",
        );
        for p in &status.missing {
            s.push_str(&format!(
                "  * {lang}: `{tool}` is missing — needed to analyse this project's {by}. \
Install it with `sudo apt-get install {apt}`.\n",
                lang = p.lang,
                tool = p.tool,
                by = p.detected_by,
                apt = p.apt_package,
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
    s
}

#[cfg(test)]
mod tests {
    use super::*;

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
    use std::path::PathBuf;

    /// A tool that certainly exists on any unix box, and one that certainly
    /// does not — the availability probe must tell them apart.
    #[test]
    fn availability_probe_agrees_with_the_shell() {
        assert!(tool_available("sh"), "sh is always installed");
        assert!(!tool_available("pir-definitely-not-a-real-tool-xyz"),
                "a nonexistent tool must not be reported as available");
    }

    /// The probe is handed `PATH`/shell metacharacters by no one — but it
    /// interpolates into a `sh -c` string, so it must refuse anything that is
    /// not a plain binary name rather than executing it.
    #[test]
    fn availability_probe_refuses_shell_metacharacters() {
        for bad in ["", "a; rm -rf /", "x y", "$(id)", "../../bin/sh"] {
            assert!(!tool_available(bad), "must refuse {bad:?}");
        }
    }

    /// A C++ project is recognised, and gets LLVM's static analyzer (the tool
    /// that was silently absent before).
    #[test]
    fn detects_cpp_and_names_llvm_static_analysis() {
        let t = Tmp::new("cpp");
        t.write("CMakeLists.txt").write("main.cpp");
        let plan = detect_lints(&t.0);
        let cpp = plan.iter().find(|p| p.lang == "cpp").expect("cpp detected");
        assert_eq!(cpp.tool, "scan-build", "LLVM static analysis for C++");
        assert!(cpp.command.contains("clang-tidy"), "clang-tidy vets style too");
        assert_eq!(cpp.apt_package, "clang-tools");
        assert_eq!(cpp.detected_by, "CMakeLists.txt");
    }

    /// A bare `.c` tree with no manifest is still C, not silently "unknown".
    #[test]
    fn detects_plain_c_from_source_files() {
        let t = Tmp::new("c");
        t.write("main.c");
        let plan = detect_lints(&t.0);
        let c = plan.iter().find(|p| p.lang == "c").expect("c detected");
        assert_eq!(c.tool, "scan-build");
        assert!(c.command.contains("cppcheck"), "cppcheck is the cheap second opinion");
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

    /// Every detected language's linter is probed, and a missing one lands in
    /// `missing` — this is the invariant that stops a silent pass.
    #[test]
    fn missing_linters_are_separated_from_available_ones() {
        let t = Tmp::new("part");
        t.write("Cargo.toml");
        let st = partition(detect_lints(&t.0));
        let rust = st.available.iter().find(|p| p.lang == "rust").expect("rust available");
        assert_eq!(rust.tool, "cargo-clippy");
        // Nothing in `missing` may be something we can actually run.
        for p in &st.missing {
            assert!(!tool_available(p.tool), "{} is installed but was reported missing", p.tool);
        }
    }

    /// The prompt must state the *absence* of a linter explicitly, so the model
    /// is told not to call unanalysed code clean.
    #[test]
    fn prompt_tells_the_model_not_to_claim_unanalysed_code_is_clean() {
        let t = Tmp::new("prompt");
        t.write("CMakeLists.txt").write("main.cpp");
        let p = autoclean_prompt(&t.0);
        // Either scan-build runs, or the prompt must say it is missing and
        // offer the install. Both are acceptable; silence is not.
        if !tool_available("scan-build") {
            assert!(p.contains("NOT installed"), "must flag the missing linter: {p}");
            assert!(p.contains("apt-install"), "must offer the install path");
            assert!(p.contains("ask_user_question"), "must ask before installing");
        } else {
            assert!(p.contains("scan-build"), "must name the linter it can run");
        }
    }

    /// A tree with nothing recognisable must not invent a plan.
    #[test]
    fn an_unrecognised_tree_says_so_instead_of_guessing() {
        let t = Tmp::new("empty");
        let p = autoclean_prompt(&t.0);
        assert!(p.contains("no recognised build manifest"), "got: {p}");
    }

    /// When *nothing* is installed the prompt must not still claim to have a
    /// command list, and must lead with the install question instead.
    #[test]
    fn a_fully_unanalysable_project_says_that_up_front() {
        let t = Tmp::new("none");
        t.write("CMakeLists.txt"); // cpp -> scan-build, absent on this host
        let st = super::partition(detect_lints(&t.0));
        if st.available.is_empty() {
            let p = autoclean_prompt(&t.0);
            assert!(
                p.contains("No linter for this project is installed"),
                "must not imply any analysis will run: {p}"
            );
            assert!(!p.contains("Run these, fix everything"), "must not promise a command list");
        }
    }

    /// Rust is still the first-class citizen it always was — this must not
    /// regress the case `/autoclean` originally existed for.
    #[test]
    fn rust_projects_still_get_build_test_and_clippy() {
        let t = Tmp::new("rust");
        t.write("Cargo.toml").write("main.rs");
        let p = autoclean_prompt(&t.0);
        assert!(p.contains("cargo build"), "must still build");
        assert!(p.contains("cargo test"), "must still test");
        assert!(p.contains("clippy"), "must now also lint");
    }
}

#[cfg(test)]
mod prompt_text_tests {
    use super::autoclean_prompt;

    /// The C++ prompt must both run the analysis AND admit it is missing, with
    /// an install offer — the exact combination the user asked for.
    #[test]
    fn cpp_prompt_names_the_tool_and_offers_the_install() {
        let dir = std::env::temp_dir().join("pir-lints-prompt-cpp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CMakeLists.txt"), b"").unwrap();
        std::fs::write(dir.join("main.cpp"), b"").unwrap();
        let p = autoclean_prompt(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        if super::tool_available("scan-build") {
            assert!(p.contains("scan-build"), "must run the analyzer when present");
        } else {
            assert!(p.contains("NOT installed"), "must warn it is missing:\n{p}");
            assert!(p.contains("clang-tools"), "must name the apt package");
            assert!(p.contains("ask_user_question"), "must ask before installing");
            assert!(p.contains("apt-install"), "must use the audited escalation path");
            assert!(p.contains("Never report 'no issues found'"), "must forbid a false clean bill");
        }
    }
}
