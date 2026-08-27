use crate::term;
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Run a shell command in the project directory (bash -c, or cmd /C on \
                          Windows). Returns stdout and stderr (truncated to 30k chars) plus the \
                          exit code. Killed after 120s.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute" }
                },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "read_file",
            description: "Read a UTF-8 text file (truncated to 100k chars).",
            schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file",
            description: "Create or overwrite a file. Parent directories are created.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string", "description": "Full new file content" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace exactly one occurrence of old_string with new_string. \
                          old_string must be unique — include surrounding lines to disambiguate.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        ToolSpec {
            name: "list_dir",
            description: "List the entries of a directory (non-recursive).",
            schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Defaults to ." } },
                "required": []
            }),
        },
    ]
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
}

enum Decision {
    Yes,
    Always,
    No,
}

fn ask(what: &str) -> Decision {
    let answer =
        term::read_answer(&format!("Allow {what}? [y]es / [a]lways / [n]o (default no)"));
    match answer.as_str() {
        "y" | "yes" => Decision::Yes,
        "a" | "always" => Decision::Always,
        _ => Decision::No,
    }
}

pub struct ToolRunner {
    cwd: PathBuf,
    full_auto: bool,
    bash_ok: bool,
    write_ok: bool,
}

impl ToolRunner {
    pub fn new(cwd: PathBuf, full_auto: bool) -> Self {
        ToolRunner { cwd, full_auto, bash_ok: false, write_ok: false }
    }

    pub fn execute(&mut self, name: &str, input: &Value) -> Outcome {
        let result = match name {
            "bash" => self.do_bash(input),
            "read_file" => read_file(input),
            "write_file" => self.write_file(input),
            "edit_file" => self.edit_file(input),
            "list_dir" => list_dir(input, &self.cwd),
            other => Err(format!("unknown tool '{other}'")),
        };
        match result {
            Ok(content) => Outcome { content, is_error: false },
            Err(content) => Outcome { content, is_error: true },
        }
    }

    fn do_bash(&mut self, input: &Value) -> Result<String, String> {
        let command = input["command"].as_str().ok_or("bash: missing 'command'")?;
        if !self.full_auto && !self.bash_ok {
            match ask(&format!("run {}", term::yellow(&format!("`{command}`")))) {
                Decision::No => return Ok("[denied] user declined to run this command".into()),
                Decision::Always => self.bash_ok = true,
                Decision::Yes => {}
            }
        }
        run_shell(command, &self.cwd)
    }

    fn write_file(&mut self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("write_file: missing 'path'")?;
        let content = input["content"].as_str().ok_or("write_file: missing 'content'")?;
        if !self.full_auto && !self.write_ok {
            let verb = if Path::new(path).exists() { "overwrite" } else { "create" };
            match ask(&format!("{verb} {}", term::yellow(path))) {
                Decision::No => return Ok("[denied] user declined this write".into()),
                Decision::Always => self.write_ok = true,
                Decision::Yes => {}
            }
        }
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| format!("write_file {path}: {e}"))?;
            }
        }
        fs::write(path, content).map_err(|e| format!("write_file {path}: {e}"))?;
        Ok(format!("wrote {path} ({} lines, {} bytes)", content.lines().count(), content.len()))
    }

    fn edit_file(&mut self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("edit_file: missing 'path'")?;
        let old = input["old_string"].as_str().ok_or("edit_file: missing 'old_string'")?;
        let new = input["new_string"].as_str().ok_or("edit_file: missing 'new_string'")?;
        let src = fs::read_to_string(path).map_err(|e| format!("edit_file {path}: {e}"))?;
        let hits = src.matches(old).count();
        if hits == 0 {
            return Err(format!("edit_file {path}: old_string not found"));
        }
        if hits > 1 {
            return Err(format!(
                "edit_file {path}: old_string appears {hits}x — add surrounding lines to make it unique"
            ));
        }
        if !self.full_auto && !self.write_ok {
            match ask(&format!("edit {}", term::yellow(path))) {
                Decision::No => return Ok("[denied] user declined this edit".into()),
                Decision::Always => self.write_ok = true,
                Decision::Yes => {}
            }
        }
        let updated = src.replacen(old, new, 1);
        fs::write(path, updated).map_err(|e| format!("edit_file {path}: {e}"))?;
        Ok(format!("edited {path}"))
    }
}

fn read_file(input: &Value) -> Result<String, String> {
    let path = input["path"].as_str().ok_or("read_file: missing 'path'")?;
    let mut text = fs::read_to_string(path).map_err(|e| format!("read_file {path}: {e}"))?;
    truncate(&mut text, 100_000);
    Ok(text)
}

fn list_dir(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = input["path"].as_str().unwrap_or(".");
    let dir = cwd.join(path);
    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("list_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let suffix = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { "/" } else { "" };
        entries.push(format!("{name}{suffix}"));
    }
    if entries.is_empty() {
        return Ok("(empty)".into());
    }
    entries.sort();
    Ok(entries.join("\n"))
}

fn run_shell(command: &str, cwd: &Path) -> Result<String, String> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    let mut child = spawn_shell(command, cwd)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // read pipes on threads so a chatty child can't deadlock on a full pipe
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let out = t_out.join().unwrap_or_default();
    let err = t_err.join().unwrap_or_default();

    let mut text = String::from_utf8_lossy(&out).to_string();
    let err_text = String::from_utf8_lossy(&err).to_string();
    if !err_text.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&err_text);
    }
    match status {
        Some(s) if s.success() => {}
        Some(s) => text.push_str(&format!("\n[exit code {}]", s.code().unwrap_or(-1))),
        None => text.push_str(&format!("\n[pir] timed out after {}s, killed", TIMEOUT.as_secs())),
    }
    truncate(&mut text, 30_000);
    Ok(text)
}

fn spawn_shell(command: &str, cwd: &Path) -> Result<std::process::Child, String> {
    let mut build = |prog: &str, flag: &str| {
        let mut c = Command::new(prog);
        c.arg(flag).arg(command);
        c.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    };
    let (prog, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("bash", "-c") };
    match build(prog, flag).spawn() {
        Ok(c) => Ok(c),
        Err(e) if !cfg!(windows) => {
            build("sh", "-c").spawn().map_err(|_| format!("spawn {prog}: {e}"))
        }
        Err(e) => Err(format!("spawn {prog}: {e}")),
    }
}

fn truncate(s: &mut String, max_chars: usize) {
    if s.chars().count() > max_chars {
        let cut = s.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push_str("\n… [pir] output truncated");
    }
}
