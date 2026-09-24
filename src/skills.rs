//! Skills: file-based, progressively-disclosed instructions (Feature 4).
//!
//! A *skill* is a directory containing a `SKILL.md` whose YAML-ish frontmatter
//! carries a `name` and a `description`. The description goes into the system
//! prompt as a short catalog line; the *body* is only pulled into context when
//! the model decides the skill applies, by calling the `load_skill` tool. That
//! indirection is the whole point: an agent can carry a dozen detailed procedures
//! (a release checklist, a code-review rubric, how to drive a specific tool)
//! without paying for their full text on every single request.
//!
//! Discovery roots (earlier wins on a name collision, so a project can override
//! a user-level skill):
//!
//! ```text
//!   <cwd>/.pir/skills/<name>/SKILL.md     project-local
//!   ~/.pi/skills/<name>/SKILL.md          user-global
//!   <config>/agent/skills/<name>/SKILL.md config-dir (PI_DIR aware)
//! ```
//!
//! A plain `skills/<name>.md` file also works (no directory needed) for a
//! one-file skill.
//!
//! Frontmatter is parsed by a deliberately small, dependency-free reader —
//! `---`-delimited, `key: value` lines — matching the shape Command Code and pi
//! use, and tolerating a missing frontmatter block (in which case the first
//! heading or the first non-empty line supplies the name and description, so a
//! hand-written skill file still shows up in the catalog).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Soft cap on a skill body, so a huge file can't blow the context window when
/// the model loads it. Anything longer is truncated with an explicit marker.
pub const MAX_BODY_CHARS: usize = 40_000;

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Slug used with `load_skill` (frontmatter `name`, else the dir/file name).
    pub name: String,
    /// One-line summary shown in the system-prompt catalog.
    pub description: String,
    /// Absolute path to the `SKILL.md` (or `<name>.md`).
    pub path: PathBuf,
    /// Which root it came from — used for display and to break name ties.
    pub source: Source,
}

/// Where a skill was found, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `<cwd>/.pir/skills` — highest precedence (project overrides user).
    Project,
    /// `<config dir>/agent/skills` (e.g. `~/.pi/agent/skills`).
    Config,
    /// `~/.pi/skills` — lowest precedence.
    Global,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Project => "project",
            Source::Config => "user",
            Source::Global => "global",
        }
    }
}

/// The default roots, in precedence order (highest first).
pub fn roots(cwd: &Path) -> Vec<(Source, PathBuf)> {
    let mut v = vec![
        (Source::Project, cwd.join(".pir").join("skills")),
        (
            Source::Config,
            crate::config::pi_dir().join("agent").join("skills"),
        ),
    ];
    // `~/.pi/skills` is the *global* root. `pi_dir()` already resolves to the
    // invoking user's `~/.pi` (honouring PI_DIR / PIR_INVOKING_HOME), so this is
    // the same directory for the common case; the Config root above is where
    // pi's own skills live.
    v.push((Source::Global, crate::config::pi_dir().join("skills")));
    v
}

/// Discover every skill under the roots. Names are unique: the first root that
/// provides a name wins (project beats user beats global), which is how a repo
/// overrides a personal skill without editing it.
pub fn discover(cwd: &Path) -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    for (source, root) in roots(cwd) {
        for (name, path) in scan_root(&root) {
            let (fm_name, description) = parse_frontmatter(&path);
            let final_name = fm_name.unwrap_or(name);
            let final_name = slug(&final_name);
            if final_name.is_empty() {
                continue;
            }
            by_name.entry(final_name.clone()).or_insert(Skill {
                name: final_name,
                description,
                path,
                source,
            });
        }
    }
    by_name.into_values().collect()
}

/// Directory/file entries under one root: `<dir>/SKILL.md` and `<name>.md`.
fn scan_root(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root) else { return out };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let stem = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            let skill_md = p.join("SKILL.md");
            if skill_md.is_file() {
                out.push((stem, skill_md));
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("md")
            && !stem.eq_ignore_ascii_case("README.md")
        {
            let name = stem.trim_end_matches(".md").to_string();
            out.push((name, p));
        }
    }
    out
}

/// Read a skill's body (frontmatter stripped), truncated to [`MAX_BODY_CHARS`].
pub fn body(skill: &Skill) -> Result<String, String> {
    let raw = fs::read_to_string(&skill.path)
        .map_err(|e| format!("cannot read {}: {e}", skill.path.display()))?;
    let body = strip_frontmatter(&raw);
    let mut text = body.trim().to_string();
    if text.is_empty() {
        text = format!("(the skill file {} has no body)", skill.path.display());
    }
    crate::plugin::truncate(&mut text, MAX_BODY_CHARS);
    Ok(text)
}

/// Render the catalog that goes into the system prompt. Empty string when there
/// are no skills — the caller appends nothing, so a skill-less session's prompt
/// is byte-identical to before this feature existed.
pub fn catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n# Skills\n\n\
         You have skills available: specialised instruction sets you can load on demand. \
         When a task matches a skill's description, call the `load_skill` tool with its name \
         *before* doing the work — the skill's full instructions will be returned for you to \
         follow. Do not load skills you do not need; they cost context.\n\n",
    );
    for sk in skills {
        s.push_str(&format!("- {} ({}): {}\n", sk.name, sk.source.label(), sk.description));
    }
    s.push_str(
        "\nTo see everything available call `load_skill` with no name. To read a skill you \
         already loaded again, call `load_skill` with its name.\n",
    );
    s
}

/// Normalize a display name into a `load_skill` slug: lowercase, spaces and
/// punctuation become `-`, runs collapsed. `"Code Review!"` -> `"code-review"`.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if (c.is_whitespace() || c == '-' || c == '_' || c == '/' || c == '.')
            && !out.is_empty()
            && !prev_dash
        {
            out.push('-');
            prev_dash = true;
        }
        // Every other character is dropped (so `SKILL.md` can't smuggle a path).
    }
    out.trim_matches('-').to_string()
}

/// Look a skill up by name (exact slug, else a unique prefix, so
/// `load_skill("code")` finds `code-review` when nothing else starts with
/// "code"). Returns an explicit error listing what *is* available when nothing
/// matches — the model can then correct itself in one step.
pub fn find<'a>(skills: &'a [Skill], query: &str) -> Result<&'a Skill, String> {
    let q = slug(query);
    if q.is_empty() {
        return Err(format!(
            "load_skill: empty skill name. Available: {}",
            names_line(skills)
        ));
    }
    if let Some(sk) = skills.iter().find(|s| s.name == q) {
        return Ok(sk);
    }
    let matches: Vec<&Skill> = skills.iter().filter(|s| s.name.starts_with(&q)).collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!("load_skill: no skill named '{query}'. Available: {}", names_line(skills))),
        _ => Err(format!(
            "load_skill: '{query}' is ambiguous (matches {}). Be specific.",
            matches.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn names_line(skills: &[Skill]) -> String {
    if skills.is_empty() {
        "(none — no skills are installed; add one under .pir/skills/<name>/SKILL.md)".to_string()
    } else {
        skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// Parse `---`-delimited frontmatter for `name` and `description`. Missing
/// frontmatter falls back to the first markdown heading (name) and the first
/// non-empty, non-heading line (description), so a plain `.md` still works.
fn parse_frontmatter(path: &Path) -> (Option<String>, String) {
    let Ok(raw) = fs::read_to_string(path) else { return (None, String::new()) };
    if let Some((fm, _)) = split_frontmatter(&raw) {
        let mut name = None;
        let mut description = String::new();
        for line in fm.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim().trim_matches('"').trim_matches('\'').trim().to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "name" | "title" => {
                    if !v.is_empty() {
                        name = Some(v);
                    }
                }
                "description" | "summary" | "when" | "when_to_use" => {
                    if !v.is_empty() {
                        description = v;
                    }
                }
                _ => {}
            }
        }
        if !description.is_empty() || name.is_some() {
            return (name, description);
        }
    }
    fallback_meta(&raw)
}

/// First heading -> name, first prose line -> description.
fn fallback_meta(raw: &str) -> (Option<String>, String) {
    let mut name = None;
    let mut description = String::new();
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty() || l == "---" {
            continue;
        }
        if let Some(h) = l.strip_prefix('#') {
            if name.is_none() {
                name = Some(h.trim_matches('#').trim().to_string());
            }
            continue;
        }
        if description.is_empty() {
            description = l.to_string();
        }
        if name.is_some() && !description.is_empty() {
            break;
        }
    }
    (name, description)
}

/// Split a document into `(frontmatter, body)`. `None` when there is no
/// leading `---` block.
fn split_frontmatter(raw: &str) -> Option<(String, String)> {
    let rest = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let rest = rest.strip_prefix("---\r\n").or_else(|| rest.strip_prefix("---\n"))?;
    let mut fm = String::new();
    let mut lines = rest.lines();
    for line in lines.by_ref() {
        if line.trim() == "---" {
            let body: Vec<&str> = lines.collect();
            return Some((fm, body.join("\n")));
        }
        fm.push_str(line);
        fm.push('\n');
    }
    // Unterminated frontmatter: treat the whole thing as body (never silently
    // eat the document).
    None
}

/// The document with its frontmatter removed (the body only).
fn strip_frontmatter(raw: &str) -> String {
    match split_frontmatter(raw) {
        Some((_, body)) => body,
        None => raw.to_string(),
    }
}

/// Write the bundled `locate` skill (and any other seed skills) into the config
/// skills root the first time they are missing. Idempotent: an existing file is
/// never overwritten, so an operator's edited copy survives upgrades. Honours
/// `PIR_SKILLS_NOSEED=1` to opt out entirely.
///
/// Returns how many skills were seeded.
pub fn seed_bundled() -> usize {
    if std::env::var_os("PIR_SKILLS_NOSEED").is_some() {
        return 0;
    }
    let root = crate::config::pi_dir().join("agent").join("skills");
    let mut n = 0;
    for (name, text) in BUNDLED {
        let dir = root.join(name);
        let file = dir.join("SKILL.md");
        if file.exists() {
            continue;
        }
        if fs::create_dir_all(&dir).is_ok() && fs::write(&file, text).is_ok() {
            n += 1;
        }
    }
    n
}

/// Skills shipped with pir. Each is written once, on first run, into the
/// config skills root (never overwriting an edited copy).
const BUNDLED: &[(&str, &str)] = &[(
    "locate",
    r#"---
name: locate
description: Find the files, symbols, and call sites relevant to a task anywhere in the repo (or on the host) before changing anything — fast, greppable, no guessing.
---

# Locate

Find where something lives before you touch it. Guessing a path or editing the
wrong file is the most expensive mistake in this environment, so spend the
first minute locating precisely.

## Order of attack

1. **Cheap and broad first.** `glob` for file names, `grep` for content. Both
   are ripgrep-backed and respect `.gitignore`, so they are fast and skip noise
   (`target/`, `node_modules/`).
2. **Narrow by structure.** Once you know the directory, `list_dir` it and read
   the 2-3 most relevant files with `read_file` — not the whole tree.
3. **Only then** go wide again if the first pass came up empty.

## Recipes

Find a file by name anywhere below the project root:

```
glob(pattern="**/*parser*.rs")        # glob, not a shell command
glob(pattern="**/*.{ts,tsx}", limit=50)
```

Find a definition (anchor on the language's declaration keyword, not the bare
name — a bare name matches every *use* too):

```
grep(pattern="fn\s+build_system_prompt")         # rust
grep(pattern="(def|class)\s+Skill", type="py")   # python, type-filtered
grep(pattern="export (default )?function \w+", glob="*.ts")
```

Find *every call site* of a function (`\b` stops `foo` matching `foobar`):

```
grep(pattern="\bdescribe_call\s*\(", glob="*.rs")
```

Find configuration / registration (how a thing is wired matters more than the
thing itself):

```
grep(pattern="register_all|mod ext_")
grep(pattern="load_skill|SKILL.md")
```

Locate a binary or host file when it is not in the repo:

```
bash(command="command -v rg jq pdftotext")      # what's installed
bash(command="type -a python3; ls /usr/share/doc | head")
glob(pattern="**/Cargo.toml")                   # nested crates/workspaces
bash(command="find / -name 'libcurl.so.4' 2>/dev/null | head")   # last resort
```

## Working rules

- **Search the repo before the filesystem.** A whole-host `find /` is slow and
  almost never what you want; `glob`/`grep` already cover the project.
- **Quote regexes for the tool, not the shell.** `grep` takes a regular
  expression directly — do not escape it for `bash`, and do not wrap it in
  `/…/`.
- **Read the file that matched, plus its neighbours.** A definition is only
  half the answer; the call sites tell you the contract.
- **Say the path out loud** in your reply ("`src/agent.rs:3253`") so the operator
  can jump straight there.
- If a search returns nothing, widen in this order: (1) drop `glob`/`type`
  filters, (2) drop the leading path, (3) search for a substring of the name,
  (4) case-insensitive, (5) check you are in the right repo/root at all.
"#,
)];

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pir_skills_{}_{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }
        fn skill(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn frontmatter_supplies_name_and_description() {
        let f = Fixture::new("fm");
        let p = f.skill(
            "code-review/SKILL.md",
            "---\nname: Code Review!\ndescription: Perform thorough code reviews\n---\n# Body\nsteps\n",
        );
        let (name, desc) = parse_frontmatter(&p);
        assert_eq!(name.as_deref(), Some("Code Review!"));
        assert_eq!(desc, "Perform thorough code reviews");
        // Body excludes the frontmatter.
        let sk = Skill {
            name: "code-review".into(),
            description: desc,
            path: p,
            source: Source::Project,
        };
        let body = body(&sk).unwrap();
        assert!(body.starts_with("# Body"), "got: {body}");
        assert!(!body.contains("description:"), "frontmatter leaked into body: {body}");
    }

    #[test]
    fn missing_frontmatter_falls_back_to_heading_and_first_line() {
        let f = Fixture::new("nofm");
        let p = f.skill("plain.md", "# Deploy Runbook\n\nRun the checks, then ship.\n\nmore\n");
        let (name, desc) = parse_frontmatter(&p);
        assert_eq!(name.as_deref(), Some("Deploy Runbook"));
        assert_eq!(desc, "Run the checks, then ship.");
    }

    #[test]
    fn unterminated_frontmatter_is_treated_as_body() {
        let f = Fixture::new("unterm");
        let p = f.skill("x.md", "---\nname: x\ndescription: d\nstill going\n");
        let (name, desc) = parse_frontmatter(&p);
        // Nothing is silently eaten: the whole document falls through to the
        // prose fallback, so the first non-heading line becomes the description
        // (here the literal `name: x` line — the point is that the content is
        // still *reachable*, not that it is interpreted as metadata).
        assert_eq!(name, None, "no heading => no fallback name");
        assert!(desc.contains("name: x"), "got: {desc}");
    }

    #[test]
    fn slug_normalizes_names_and_cannot_smuggle_paths() {
        assert_eq!(slug("Code Review!"), "code-review");
        assert_eq!(slug("  My_Skill  "), "my-skill");
        assert_eq!(slug("a//b"), "a-b");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("!!!"), "");
    }

    #[test]
    fn discover_prefers_project_over_user_and_dedupes() {
        let f = Fixture::new("disc");
        // A project root and a stand-in "user" root are the two we can build in
        // a temp dir; `discover` is driven through `roots` below, so call the
        // scanner directly for the precedence rule.
        let proj = f.skill(".pir/skills/review/SKILL.md", "---\nname: review\ndescription: project copy\n---\nproj\n");
        let user = f.skill("user/review/SKILL.md", "---\nname: review\ndescription: user copy\n---\nuser\n");
        let user2 = f.skill("user/deploy.md", "---\nname: deploy\ndescription: ship it\n---\ngo\n");
        let _ = (&proj, &user, &user2);
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
        for (source, path) in [
            (Source::Project, proj.clone()),
            (Source::Global, user.clone()),
            (Source::Global, user2.clone()),
        ] {
            let (n, d) = parse_frontmatter(&path);
            by_name
                .entry(slug(n.as_deref().unwrap_or("")))
                .or_insert(Skill { name: slug(n.as_deref().unwrap_or("")), description: d, path, source });
        }
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name["review"].source, Source::Project);
        assert_eq!(by_name["review"].description, "project copy");
        assert_eq!(by_name["deploy"].source, Source::Global);
    }

    #[test]
    fn find_matches_exactly_by_prefix_and_reports_ambiguity() {
        let mk = |n: &str| Skill {
            name: n.into(),
            description: String::new(),
            path: PathBuf::from("/x"),
            source: Source::Project,
        };
        let skills = vec![mk("code-review"), mk("code-gen"), mk("deploy")];
        assert_eq!(find(&skills, "deploy").unwrap().name, "deploy");
        assert_eq!(find(&skills, "DEPLOY").unwrap().name, "deploy", "case-insensitive");
        assert!(find(&skills, "de").is_ok(), "unique prefix resolves");
        let err = find(&skills, "code").unwrap_err();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(err.contains("code-gen"), "got: {err}");
        let err = find(&skills, "nope").unwrap_err();
        assert!(err.contains("no skill named"), "got: {err}");
        assert!(err.contains("code-review"), "must list what exists: {err}");
    }

    #[test]
    fn catalog_is_empty_without_skills_and_lists_them_otherwise() {
        assert_eq!(catalog(&[]), "", "no skills => no prompt change");
        let skills = vec![Skill {
            name: "locate".into(),
            description: "Find files fast".into(),
            path: PathBuf::from("/x"),
            source: Source::Config,
        }];
        let c = catalog(&skills);
        assert!(c.contains("# Skills"), "got: {c}");
        assert!(c.contains("- locate (user): Find files fast"), "got: {c}");
        assert!(c.contains("load_skill"), "must name the tool: {c}");
    }

    #[test]
    fn bundled_locate_skill_parses_and_is_useful() {
        let (name, text) = BUNDLED[0];
        assert_eq!(name, "locate");
        let (fm_name, desc) = {
            let f = Fixture::new("bundled");
            let p = f.skill("locate/SKILL.md", text);
            parse_frontmatter(&p)
        };
        assert_eq!(fm_name.as_deref(), Some("locate"));
        assert!(desc.to_lowercase().contains("find"), "description: {desc}");
        // The body must actually teach the tools it names.
        for tool in ["glob(", "grep(", "list_dir", "read_file", "bash("] {
            assert!(text.contains(tool), "locate skill should show `{tool}` usage");
        }
    }

    #[test]
    fn seed_writes_bundled_skills_once_and_never_clobbers() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_skills_seed_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        assert_eq!(seed_bundled(), 1, "first run seeds the bundled skills");
        let locate = dir.join("agent/skills/locate/SKILL.md");
        assert!(locate.is_file(), "locate skill must land on disk");
        // An edited copy survives a re-seed.
        fs::write(&locate, "---\nname: locate\ndescription: mine\n---\nmy version\n").unwrap();
        assert_eq!(seed_bundled(), 0, "second run seeds nothing");
        assert!(fs::read_to_string(&locate).unwrap().contains("my version"));
        // Opt-out.
        unsafe { std::env::set_var("PIR_SKILLS_NOSEED", "1"); }
        fs::remove_file(&locate).unwrap();
        assert_eq!(seed_bundled(), 0);
        unsafe { std::env::remove_var("PIR_SKILLS_NOSEED"); }
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_reads_a_real_root_tree() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = std::env::temp_dir().join(format!("pir_skills_cfg_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cfg);
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &cfg); }
        let cwd = cfg.join("proj");
        fs::create_dir_all(cwd.join(".pir/skills/review")).unwrap();
        fs::write(
            cwd.join(".pir/skills/review/SKILL.md"),
            "---\nname: review\ndescription: check code\n---\nbody\n",
        )
        .unwrap();
        fs::create_dir_all(cfg.join("agent/skills")).unwrap();
        fs::write(cfg.join("agent/skills/deploy.md"), "# Deploy\n\nship it\n").unwrap();
        let skills = discover(&cwd);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["deploy", "review"], "sorted by name: {names:?}");
        assert_eq!(find(&skills, "review").unwrap().source, Source::Project);
        assert_eq!(find(&skills, "deploy").unwrap().source, Source::Config);
        assert_eq!(find(&skills, "deploy").unwrap().description, "ship it");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = fs::remove_dir_all(&cfg);
    }
}
