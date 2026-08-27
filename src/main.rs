mod agent;
mod config;
mod provider;
mod term;
mod tools;
mod types;

use crate::agent::Agent;
use crate::config::Provider;
use std::io::Write;

const HELP: &str = r#"pir — a featherweight pi-compatible coding agent

USAGE
  pir [options] [prompt]     prompt given => one-shot, else interactive REPL

OPTIONS
  -m, --model <selector>     e.g. -m anthropic/claude-sonnet-4-5 (fuzzy match ok)
  -y, --full-auto            no confirmation for shell/write tools
  -n, --no-color             disable ANSI colors
  -h, --help  -V, --version

CONFIG (reused from pi, never modified)
  ~/.pi/models.json          providers, models, api keys ("{env:VAR}" supported)
  ~/.pi/agent/settings.json  optional default model ("model" key)
  ~/.pi/AGENTS.md, ./AGENTS.md   appended to the system prompt
  ~/.pi/agent/sessions/      pir session transcripts (pir-*.jsonl)

COMMANDS
  /help  /model <sel>  /models  /clear  /usage  /exit
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut model_sel: Option<String> = None;
    let mut full_auto = false;
    let mut prompt: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--model" => {
                i += 1;
                match args.get(i) {
                    Some(v) => model_sel = Some(v.clone()),
                    None => die("--model needs a value"),
                }
            }
            "-y" | "--full-auto" => full_auto = true,
            "-n" | "--no-color" => term::set_color(false),
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("pir {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            x if x.starts_with('-') => die(&format!("unknown flag {x} — try --help")),
            x => prompt.push(x.to_string()),
        }
        i += 1;
    }

    let providers = match config::load_providers() {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => die("~/.pi/models.json contains no providers"),
        Err(e) => die(&e),
    };

    let explicit = model_sel.is_some();
    let selector = model_sel
        .or_else(|| std::env::var("PI_MODEL").ok())
        .or_else(|| config::default_model_setting())
        .unwrap_or_else(|| providers[0].label(&providers[0].models[0]));

    let (provider, model) = match config::select(&providers, &selector) {
        Ok(x) => x,
        Err(e) if explicit => die(&format!("{e}\n{}", list_models(&providers))),
        Err(e) => {
            let fb = providers[0].label(&providers[0].models[0]);
            eprintln!("pir: {e}; falling back to {fb}");
            match config::select(&providers, &fb) {
                Ok(x) => x,
                Err(e2) => die(&format!("{e2}\n{}", list_models(&providers))),
            }
        }
    };

    let mut agent = match Agent::new(provider.clone(), model.clone(), full_auto) {
        Ok(a) => a,
        Err(e) => die(&e),
    };

    if !prompt.is_empty() {
        agent.turn(&prompt.join(" "));
        return;
    }

    println!("{}", term::bold("pir"));
    println!(
        "{}",
        term::dim(&format!(
            "model {} · {} · config {}",
            agent.label(),
            if full_auto { "full-auto" } else { "confirm-actions" },
            config::pi_dir().display()
        ))
    );
    if let Some(p) = &agent.log_path {
        println!("{}", term::dim(&format!("session log: {}", p.display())));
    }
    println!("{}", term::dim("/help for commands · ctrl-d to quit"));

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        print!("{} ", term::cyan("❯"));
        let _ = std::io::stdout().flush();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("stdin: {e}");
                break;
            }
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if let Some(cmd) = input.strip_prefix('/') {
            handle_command(cmd, &mut agent, &providers);
        } else {
            agent.turn(input);
            println!(
                "{}",
                term::dim(&format!(
                    "· {} in / {} out tokens",
                    fmt_tok(agent.usage.input),
                    fmt_tok(agent.usage.output)
                ))
            );
        }
    }
}

fn handle_command(cmd: &str, agent: &mut Agent, providers: &[Provider]) {
    let mut parts = cmd.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    match cmd {
        "h" | "help" => print!("{HELP}"),
        "m" | "model" => {
            if rest.is_empty() {
                println!("current model: {}", agent.label());
            } else {
                match config::select(providers, &rest.join(" ")) {
                    Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => println!("→ {}", agent.label()),
                        Err(e) => eprintln!("{e}"),
                    },
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
        "models" => print!("{}", list_models(providers)),
        "clear" => {
            agent.clear();
            println!("history cleared");
        }
        "usage" => println!(
            "{} in / {} out tokens this session",
            fmt_tok(agent.usage.input),
            fmt_tok(agent.usage.output)
        ),
        "q" | "quit" | "exit" => std::process::exit(0),
        other => eprintln!("unknown command /{other} — try /help"),
    }
}

fn list_models(providers: &[Provider]) -> String {
    let mut out = String::new();
    for p in providers {
        out.push_str(&format!("{}\n", term::bold(&p.pid())));
        for m in &p.models {
            let ctx = m.context.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            out.push_str(&format!(
                "  {:<44} ctx {:>7}  {}\n",
                m.id,
                ctx,
                m.name.as_deref().unwrap_or("")
            ));
        }
    }
    out
}

fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn die(msg: &str) -> ! {
    eprintln!("pir: {msg}");
    std::process::exit(1)
}
