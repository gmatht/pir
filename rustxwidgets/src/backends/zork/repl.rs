//! Interactive text REPL driver over the [`crate::backends::zork::model`].
//!
//! This is deliberately *one* of possibly many drivers of the pure model. It is
//! kept for manual exploration and demos. For automated tests prefer
//! [`crate::backends::zork::harness::Harness`].

use std::io::{self, BufRead, Write};

use crate::backends::BackendApp;
use crate::backends::zork::model::{ZorkKind, ZorkNode, ZorkState};

pub struct ZorkApp {
    state: ZorkState,
}

impl ZorkApp {
    pub fn new() -> Self {
        ZorkApp { state: ZorkState::new() }
    }
}

impl Default for ZorkApp {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendApp for ZorkApp {
    fn run(mut self: Box<Self>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.state.nodes.is_empty() {
            return Ok(());
        }
        self.state.running = true;

        let mut rl = rustyline::DefaultEditor::new()?;

        while self.state.running {
            if let Some(node) = self.state.node(self.state.current_id) {
                describe_room(&self.state, node);
            }

            let prompt = "> ";
            let readline = rl.readline(prompt);
            match readline {
                Ok(line) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        rl.add_history_entry(&trimmed)?;
                        execute_command(&mut self.state, &trimmed);
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted)
                | Err(rustyline::error::ReadlineError::Eof) => {
                    self.state.running = false;
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }
}

fn short_desc(state: &ZorkState, node: &ZorkNode) -> String {
    match &node.kind {
        ZorkKind::Window { title } => format!("Window \"{}\"", title),
        ZorkKind::Button { label } => format!("Button \"{}\"", label),
        ZorkKind::Label { text } => {
            let preview: String = text.chars().take(30).collect();
            let preview = if text.chars().count() > 30 { format!("{}...", preview) } else { text.clone() };
            format!("Label: \"{}\"", preview)
        }
        ZorkKind::Entry { .. } => "Entry".into(),
        ZorkKind::CheckButton { label, .. } => format!("CheckButton \"{}\"", label),
        ZorkKind::RadioButton { label, .. } => format!("RadioButton \"{}\"", label),
        ZorkKind::BoxWidget { horizontal, .. } => format!("{} Box", if *horizontal { "Horizontal" } else { "Vertical" }),
        ZorkKind::Grid { .. } => "Grid".into(),
        ZorkKind::Dialog { title } => format!("Dialog \"{}\"", title),
        ZorkKind::DropDown { items, selected } => {
            let current = selected.and_then(|s| items.get(s)).map(|s| s.as_str()).unwrap_or("(none)");
            format!("DropDown [{}]", current)
        }
        ZorkKind::TextView { .. } => "TextView".into(),
        ZorkKind::Menu => "Menu".into(),
        ZorkKind::MenuBar => "MenuBar".into(),
        ZorkKind::SimpleAction => "SimpleAction".into(),
    }
}

fn dir_name(dirs: &[(&str, &ZorkNode)], id: usize) -> &str {
    dirs.iter().find(|(_, t)| t.id == id).map(|(d, _)| *d).unwrap_or("?")
}

fn describe_room(state: &ZorkState, node: &ZorkNode) {
    println!();
    match &node.kind {
        ZorkKind::Window { title } => {
            println!("You are in a Window{}", if title.is_empty() { ".".to_string() } else { format!(" titled \"{}\".", title) });
        }
        ZorkKind::Dialog { title } => {
            println!("You are in a Dialog{}", if title.is_empty() { ".".to_string() } else { format!(" titled \"{}\".", title) });
        }
        ZorkKind::Button { label } => {
            println!("You are standing on a Button labeled \"{}\".", label);
        }
        ZorkKind::Label { text } => {
            let preview: String = text.chars().take(40).collect();
            let preview = if text.chars().count() > 40 { format!("{}...", preview) } else { text.clone() };
            println!("You are looking at a Label: \"{}\".", preview);
        }
        ZorkKind::Entry { buffer, .. } => {
            let preview: String = buffer.chars().take(40).collect();
            let preview = if buffer.is_empty() {
                "empty".into()
            } else if buffer.chars().count() > 40 {
                format!("{}...", preview)
            } else {
                buffer.clone()
            };
            println!("You are at an Entry containing \"{}\".", preview);
        }
        ZorkKind::CheckButton { label, checked } => {
            println!("You are at a CheckButton labeled \"{}\" ({}).", label, if *checked { "checked" } else { "unchecked" });
        }
        ZorkKind::RadioButton { label, checked, .. } => {
            println!("You are at a RadioButton labeled \"{}\" ({}).", label, if *checked { "selected" } else { "not selected" });
        }
        ZorkKind::DropDown { items, selected } => {
            let current = selected.and_then(|s| items.get(s)).map(|s| s.as_str()).unwrap_or("(none)");
            println!("You are at a DropDown. Current selection: \"{}\".", current);
        }
        ZorkKind::TextView { text } => {
            let preview: String = text.chars().take(40).collect();
            let preview = if text.chars().count() > 40 { format!("{}...", preview) } else { text.clone() };
            println!("You are reading a TextView: \"{}\".", preview);
        }
        ZorkKind::BoxWidget { horizontal, .. } => {
            println!("You are in a {} Box.", if *horizontal { "horizontal" } else { "vertical" });
        }
        ZorkKind::Grid { .. } => {
            println!("You are in a Grid layout.");
        }
        ZorkKind::MenuBar => {
            println!("You are at a MenuBar.");
            let items = state.menu_items.get(&node.id).cloned().unwrap_or_default();
            if !items.is_empty() {
                println!("Menu items:");
                for (i, item) in items.iter().enumerate() {
                    println!("  {}. {}", i + 1, item.label);
                }
            }
        }
        ZorkKind::Menu => {
            println!("You are at a Menu.");
            let items = state.menu_items.get(&node.id).cloned().unwrap_or_default();
            if !items.is_empty() {
                println!("Items:");
                for (i, item) in items.iter().enumerate() {
                    println!("  {}. {}", i + 1, item.label);
                }
            }
        }
        _ => {
            println!("You are in an unknown widget.");
        }
    }

    let parent_dir = node.parent.and_then(|pid| state.node(pid));
    let children: Vec<&ZorkNode> = node.children.iter().filter_map(|cid| state.node(*cid)).collect();
    let siblings: Vec<&ZorkNode> = if let Some(pid) = node.parent {
        state.node(pid).map(|p| p.children.iter().filter_map(|cid| state.node(*cid)).collect()).unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut my_idx = None;
    for (i, sib) in siblings.iter().enumerate() {
        if sib.id == node.id {
            my_idx = Some(i);
            break;
        }
    }

    let mut dirs: Vec<(&str, &ZorkNode)> = Vec::new();
    if let Some(idx) = my_idx {
        if idx > 0 {
            if let Some(prev) = siblings.get(idx - 1) {
                dirs.push(("west", prev));
            }
        }
        if idx + 1 < siblings.len() {
            if let Some(next) = siblings.get(idx + 1) {
                dirs.push(("east", next));
            }
        }
    }
    if !children.is_empty() {
        if let Some(first) = children.first() {
            dirs.push(("north", first));
        }
    }
    if parent_dir.is_some() {
        dirs.push(("south", parent_dir.unwrap()));
    }

    if !dirs.is_empty() {
        println!();
        for (dir, target) in &dirs {
            let desc = short_desc(state, target);
            println!("To the {}: {}", dir, desc);
        }
    }

    println!();
    println!("You see:");
    println!("  0. (yourself) {}", short_desc(state, node));
    let mut idx = 1;
    for (_, target) in &dirs {
        println!("  {}. {} ({})", idx, short_desc(state, target), dir_name(&dirs, target.id));
        idx += 1;
    }
    if let Some(pid) = node.parent {
        if let Some(p) = state.node(pid) {
            for cid in &p.children {
                if *cid == node.id {
                    continue;
                }
                if dirs.iter().any(|(_, t)| t.id == *cid) {
                    continue;
                }
                if let Some(other) = state.node(*cid) {
                    println!("  {}. {}", idx, short_desc(state, other));
                    idx += 1;
                }
            }
        }
    }
    println!();
}

fn execute_command(state: &mut ZorkState, line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    let cmd = parts[0].to_lowercase();
    let args: Vec<&str> = parts[1..].to_vec();

    match cmd.as_str() {
        "look" | "l" => {
            if let Some(node) = state.node(state.current_id) {
                describe_room(state, node);
            }
        }
        "go" | "g" => {
            if args.is_empty() {
                println!("Go where? (north/south/east/west or a number)");
                return;
            }
            let dir = args[0].to_lowercase();
            navigate_to(state, &dir);
        }
        "north" | "n" => navigate_to(state, "north"),
        "south" | "s" => navigate_to(state, "south"),
        "east" | "e" => navigate_to(state, "east"),
        "west" | "w" => navigate_to(state, "west"),
        "click" | "press" => {
            let target_id = if !args.is_empty() {
                resolve_arg_to_id(state, args[0])
            } else {
                Some(state.current_id)
            };
            if let Some(id) = target_id {
                if let Some(node) = state.node_mut(id) {
                    match &node.kind {
                        ZorkKind::Button { .. } => {
                            println!("You press the button. It clicks!");
                            node.fire_callbacks();
                        }
                        ZorkKind::MenuBar => {
                            println!("You click the MenuBar. Use 'select <n>' to choose an item.");
                        }
                        ZorkKind::Menu => {
                            let items = state.menu_items.get(&id).cloned().unwrap_or_default();
                            if !items.is_empty() {
                                println!("Select an item:");
                                for (i, item) in items.iter().enumerate() {
                                    println!("  {}. {} ({})", i + 1, item.label, item.action);
                                }
                            } else {
                                println!("This menu has no items.");
                            }
                        }
                        _ => println!("You can't click that."),
                    }
                }
            }
        }
        "select" | "choose" => {
            if args.is_empty() {
                println!("Select what? Use 'select <number>'.");
                return;
            }
            let num: usize = match args[0].parse() {
                Ok(n) => n,
                Err(_) => {
                    println!("Usage: select <number>");
                    return;
                }
            };
            let items = state.menu_items.get(&state.current_id).cloned().unwrap_or_default();
            if num == 0 || num > items.len() {
                println!("Invalid selection.");
                return;
            }
            let item = &items[num - 1];
            println!("You selected \"{}\".", item.label);
            if item.submenu.is_some() {
                println!("Opening submenu...");
            }
            if !item.action.is_empty() {
                println!("Action: {}", item.action);
                if let Some(node) = state.node_mut(state.current_id) {
                    node.fire_callbacks();
                }
            }
        }
        "examine" | "exam" | "x" => {
            let target_id = if !args.is_empty() {
                resolve_arg_to_id(state, args[0])
            } else {
                Some(state.current_id)
            };
            if let Some(id) = target_id {
                examine(state, id);
            }
        }
        "type" | "write" => {
            if !matches!(state.node(state.current_id).map(|n| &n.kind), Some(ZorkKind::Entry { .. })) {
                println!("You are not at an Entry. Navigate to an Entry first.");
                return;
            }
            println!("(Enter text, press Enter when done)");
            print!("> ");
            io::stdout().flush().ok();
            let mut input = String::new();
            io::stdin().lock().read_line(&mut input).ok();
            let text = input.trim().to_string();
            state.set_entry_text(state.current_id, &text);
            println!("You inscribed \"{}\" into the Entry.", text);
            state.click(state.current_id);
        }
        "read" => {
            examine(state, state.current_id);
        }
        "toggle" => {
            if let Some(n) = state.node_mut(state.current_id) {
                match &mut n.kind {
                    ZorkKind::CheckButton { ref mut checked, .. } => {
                        *checked = !*checked;
                        println!("CheckButton is now {}.", if *checked { "checked" } else { "unchecked" });
                        n.fire_callbacks();
                    }
                    ZorkKind::RadioButton { ref mut checked, .. } => {
                        *checked = !*checked;
                        println!("RadioButton is now {}.", if *checked { "selected" } else { "not selected" });
                        n.fire_callbacks();
                    }
                    _ => println!("You can't toggle that."),
                }
            }
        }
        "inventory" | "i" => {
            println!("Current path:");
            let mut path: Vec<String> = Vec::new();
            let mut cur = Some(state.current_id);
            while let Some(id) = cur {
                if let Some(node) = state.node(id) {
                    path.push(short_desc(state, node));
                    cur = node.parent;
                } else {
                    break;
                }
            }
            path.reverse();
            for (i, desc) in path.iter().enumerate() {
                println!("  {}{}", "  ".repeat(i), desc);
            }
        }
        "back" => {
            if let Some(pid) = state.node(state.current_id).and_then(|n| n.parent) {
                state.current_id = pid;
                println!("You go back south.");
            } else {
                println!("You can't go back from here.");
            }
        }
        "quit" | "q" | "exit" => {
            println!("Goodbye!");
            state.running = false;
        }
        "help" | "?" => {
            println!("Commands:");
            println!("  look / l              - describe your surroundings");
            println!("  go north/south/east/west - move in a direction");
            println!("  go <number>           - go to numbered item");
            println!("  north/n, south/s, east/e, west/w - quick move");
            println!("  click / press [n]     - press a button or open menu (default: current)");
            println!("  select / choose <n>   - select a menu item from a MenuBar/Menu");
            println!("  examine / x [n]       - examine something in detail");
            println!("  type / write          - enter text into an Entry (sub-prompt)");
            println!("  read                  - read text at current location");
            println!("  toggle                - toggle a CheckButton/RadioButton");
            println!("  inventory / i         - show your path");
            println!("  back                  - go back the way you came");
            println!("  quit / q / exit       - exit the game");
            println!("  help / ?              - show this help");
        }
        _ => {
            if let Ok(num) = cmd.parse::<usize>() {
                let target = resolve_number_target(state, num);
                if let Some(id) = target {
                    let desc = state.node(id).map(|n| short_desc(state, n)).unwrap_or_default();
                    state.prev_location = Some(state.current_id);
                    state.current_id = id;
                    println!("You move to {}.", desc);
                } else {
                    println!("Invalid number.");
                }
            } else {
                println!("I don't understand \"{}\". Type 'help' for commands.", cmd);
            }
        }
    }
}

fn resolve_arg_to_id(state: &ZorkState, arg: &str) -> Option<usize> {
    if let Ok(num) = arg.parse::<usize>() {
        resolve_number_target(state, num)
    } else {
        let dir = arg.to_lowercase();
        navigate_find(state, &dir)
    }
}

fn resolve_number_target(state: &ZorkState, num: usize) -> Option<usize> {
    if num == 0 {
        return Some(state.current_id);
    }
    let node = state.node(state.current_id)?;
    let mut items: Vec<usize> = Vec::new();

    let siblings: Vec<usize> = if let Some(pid) = node.parent {
        state.node(pid).map(|p| p.children.clone()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let children: Vec<usize> = node.children.clone();

    for id in &siblings {
        if *id == node.id {
            continue;
        }
        items.push(*id);
    }
    for id in &children {
        if !items.contains(id) {
            items.push(*id);
        }
    }

    items.get(num - 1).copied()
}

fn navigate_find(state: &ZorkState, dir: &str) -> Option<usize> {
    let node = state.node(state.current_id)?;
    let siblings: Vec<&ZorkNode> = if let Some(pid) = node.parent {
        state.node(pid).map(|p| p.children.iter().filter_map(|cid| state.node(*cid)).collect()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let children: Vec<&ZorkNode> = node.children.iter().filter_map(|cid| state.node(*cid)).collect();

    let mut my_idx = None;
    for (i, sib) in siblings.iter().enumerate() {
        if sib.id == node.id {
            my_idx = Some(i);
            break;
        }
    }

    match dir {
        "north" | "n" => children.first().map(|n| n.id),
        "south" | "s" => node.parent,
        "east" | "e" => {
            if let Some(idx) = my_idx {
                siblings.get(idx + 1).map(|n| n.id)
            } else {
                None
            }
        }
        "west" | "w" => {
            if let Some(idx) = my_idx {
                if idx > 0 {
                    siblings.get(idx - 1).map(|n| n.id)
                } else {
                    node.parent
                }
            } else {
                node.parent
            }
        }
        _ => None,
    }
}

fn navigate_to(state: &mut ZorkState, dir: &str) {
    if let Some(target_id) = navigate_find(state, dir) {
        state.prev_location = Some(state.current_id);
        state.current_id = target_id;
        println!("You go {}.", dir);
    } else if let Ok(num) = dir.parse::<usize>() {
        if let Some(id) = resolve_number_target(state, num) {
            let desc = state.node(id).map(|n| short_desc(state, n)).unwrap_or_default();
            state.prev_location = Some(state.current_id);
            state.current_id = id;
            println!("You move to {}.", desc);
        } else {
            println!("You can't go that way.");
        }
    } else {
        println!("You can't go that way.");
    }
}

fn examine(state: &ZorkState, id: usize) {
    if let Some(node) = state.node(id) {
        match &node.kind {
            ZorkKind::Label { text } => {
                println!("The label reads:");
                println!("\"{}\"", text);
            }
            ZorkKind::Entry { buffer, .. } => {
                if buffer.is_empty() {
                    println!("The Entry is blank.");
                } else {
                    println!("The Entry contains:");
                    println!("\"{}\"", buffer);
                }
            }
            ZorkKind::TextView { text } => {
                println!("The TextView contains:");
                for line in text.lines() {
                    println!("  {}", line);
                }
            }
            ZorkKind::Button { label } => {
                println!("A button labeled \"{}\". It looks clickable.", label);
            }
            ZorkKind::CheckButton { label, checked } => {
                println!("CheckButton \"{}\": currently {}.", label, if *checked { "CHECKED" } else { "UNCHECKED" });
            }
            ZorkKind::RadioButton { label, checked, .. } => {
                println!("RadioButton \"{}\": currently {}.", label, if *checked { "SELECTED" } else { "NOT SELECTED" });
            }
            ZorkKind::DropDown { items, selected } => {
                println!("DropDown with {} items:", items.len());
                for (i, item) in items.iter().enumerate() {
                    let marker = if Some(i) == *selected { " <--" } else { "" };
                    println!("  {}. {}{}", i, item, marker);
                }
            }
            ZorkKind::Window { title } => {
                println!("A window titled \"{}\".", title);
            }
            ZorkKind::Dialog { title } => {
                println!("A dialog titled \"{}\".", title);
            }
            ZorkKind::BoxWidget { horizontal, spacing } => {
                println!("A {} box with spacing {}.", if *horizontal { "horizontal" } else { "vertical" }, spacing);
            }
            ZorkKind::Grid { cols, rows } => {
                println!("A grid with {} cols, {} rows.", cols, rows);
            }
            ZorkKind::MenuBar => {
                println!("A MenuBar.");
                let items = state.menu_items.get(&node.id).cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("It has no items.");
                } else {
                    println!("It contains:");
                    for (i, item) in items.iter().enumerate() {
                        println!("  {}. {} ({})", i + 1, item.label, item.action);
                    }
                }
            }
            ZorkKind::Menu => {
                println!("A Menu.");
                let items = state.menu_items.get(&node.id).cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("It has no items.");
                } else {
                    println!("It contains:");
                    for (i, item) in items.iter().enumerate() {
                        println!("  {}. {} ({})", i + 1, item.label, item.action);
                    }
                }
            }
            _ => {
                println!("There's nothing special about this.");
            }
        }
    } else {
        println!("Nothing to examine.");
    }
}
