use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::os::raw::c_void;

pub type Callback = Box<dyn FnMut()>;

#[derive(Clone, Debug)]
pub struct MenuItemData {
    pub label: String,
    pub action: String,
    pub submenu: Option<Vec<MenuItemData>>,
}

#[derive(Clone, Debug)]
pub enum ZorkKind {
    Window { title: String },
    Button { label: String },
    Label { text: String },
    BoxWidget { horizontal: bool, spacing: i32 },
    Grid { cols: usize, rows: usize },
    Entry { buffer: String, cursor: usize },
    CheckButton { label: String, checked: bool },
    RadioButton { label: String, checked: bool, group_id: usize },
    Dialog { title: String },
    Menu,
    MenuBar,
    SimpleAction,
    DropDown { items: Vec<String>, selected: Option<usize> },
    TextView { text: String },
}

pub struct ZorkNode {
    pub id: usize,
    pub kind: ZorkKind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub callbacks: Vec<Callback>,
}

pub struct ZorkState {
    pub nodes: Vec<ZorkNode>,
    pub next_id: usize,
    pub running: bool,
    pub current_id: usize,
    pub prev_location: Option<usize>,
    /// Menu model items keyed by menu node id.
    pub menu_items: HashMap<usize, Vec<MenuItemData>>,
}

impl ZorkState {
    pub fn new() -> Self {
        ZorkState {
            nodes: Vec::new(),
            next_id: 1,
            running: true,
            current_id: 0,
            prev_location: None,
            menu_items: HashMap::new(),
        }
    }

    pub fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_node(&mut self, kind: ZorkKind, parent: Option<usize>) -> usize {
        let id = self.alloc_id();
        // If this is the first node (Window), set current_id
        if self.nodes.is_empty() {
            self.current_id = id;
        }
        self.nodes.push(ZorkNode {
            id,
            kind,
            parent,
            children: Vec::new(),
            callbacks: Vec::new(),
        });
        if let Some(pid) = parent {
            if let Some(p) = self.nodes.iter_mut().find(|n| n.id == pid) {
                p.children.push(id);
            }
        }
        id
    }

    pub fn node_mut(&mut self, id: usize) -> Option<&mut ZorkNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn node(&self, id: usize) -> Option<&ZorkNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

thread_local! {
    static ZORK_STATE: RefCell<ZorkState> = RefCell::new(ZorkState::new());
}

fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut ZorkState) -> R,
{
    ZORK_STATE.with(|s| f(&mut s.borrow_mut()))
}

pub struct ZorkApp;

impl crate::backends::BackendApp for ZorkApp {
    fn run(self: Box<Self>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        with_state(|s| {
            if s.nodes.is_empty() {
                return;
            }
            s.running = true;
        });

        let mut rl = rustyline::DefaultEditor::new()?;

        while with_state(|s| s.running) {
            with_state(|s| {
                if let Some(node) = s.node(s.current_id) {
                    describe_room(s, node);
                }
            });

            let prompt = "> ";
            let readline = rl.readline(prompt);
            match readline {
                Ok(line) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        rl.add_history_entry(&trimmed)?;
                        with_state(|s| execute_command(s, &trimmed));
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted)
                | Err(rustyline::error::ReadlineError::Eof) => {
                    with_state(|s| s.running = false);
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

// -- Room description --

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
            let preview = if buffer.is_empty() { "empty".into() } else if buffer.chars().count() > 40 { format!("{}...", preview) } else { buffer.clone() };
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

    // Compass directions
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
            let desc = short_desc(target);
            println!("To the {}: {}", dir, desc);
        }
    }

    // Numbered list
    println!();
    println!("You see:");
    println!("  0. (yourself) {}", short_desc(node));
    let mut idx = 1;
    for (_, target) in &dirs {
        println!("  {}. {} ({})", idx, short_desc(target), dirs.iter().find(|(_d, t)| t.id == target.id).map(|(d, _)| *d).unwrap_or("?"));
        idx += 1;
    }
    // Add any other nodes in the same parent that aren't already listed
    if let Some(pid) = node.parent {
        if let Some(p) = state.node(pid) {
            for cid in &p.children {
                if *cid == node.id { continue; }
                if dirs.iter().any(|(_, t)| t.id == *cid) { continue; }
                if let Some(other) = state.node(*cid) {
                    println!("  {}. {}", idx, short_desc(other));
                    idx += 1;
                }
            }
        }
    }
    println!();
}

fn short_desc(node: &ZorkNode) -> String {
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

// -- Command execution --

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
                            for cb in &mut node.callbacks {
                                cb();
                            }
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
                Err(_) => { println!("Usage: select <number>"); return; }
            };
            let items = state.menu_items.get(&state.current_id).cloned().unwrap_or_default();
            if num == 0 || num > items.len() {
                println!("Invalid selection.");
                return;
            }
            let item = &items[num - 1];
            println!("You selected \"{}\".", item.label);
            if item.submenu.is_some() {
                // Navigate into submenu
                println!("Opening submenu...");
            }
            if !item.action.is_empty() {
                println!("Action: {}", item.action);
                // Fire callbacks on the parent menu
                if let Some(node) = state.node_mut(state.current_id) {
                    for cb in &mut node.callbacks {
                        cb();
                    }
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
            if let Some(n) = state.node_mut(state.current_id) {
                if let ZorkKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                    buffer.clear();
                    buffer.push_str(&text);
                    *cursor = buffer.len();
                }
            }
            println!("You inscribed \"{}\" into the Entry.", text);
            // Fire changed callbacks
            if let Some(n) = state.node_mut(state.current_id) {
                for cb in &mut n.callbacks {
                    cb();
                }
            }
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
                        for cb in &mut n.callbacks {
                            cb();
                        }
                    }
                    ZorkKind::RadioButton { ref mut checked, .. } => {
                        *checked = !*checked;
                        println!("RadioButton is now {}.", if *checked { "selected" } else { "not selected" });
                        for cb in &mut n.callbacks {
                            cb();
                        }
                    }
                    _ => println!("You can't toggle that."),
                }
            }
        }
        "inventory" | "i" => {
            println!("Current path:");
            let mut path = Vec::new();
            let mut cur = Some(state.current_id);
            while let Some(id) = cur {
                if let Some(node) = state.node(id) {
                    path.push(short_desc(node));
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
            // Try interpreting as a number for navigation
            if let Ok(num) = cmd.parse::<usize>() {
                let target = resolve_number_target(state, num);
                if let Some(id) = target {
                    let desc = state.node(id).map(short_desc).unwrap_or_default();
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
        // Try matching by direction word
        let dir = arg.to_lowercase();
        navigate_find(state, &dir)
    }
}

fn resolve_number_target(state: &ZorkState, num: usize) -> Option<usize> {
    if num == 0 {
        return Some(state.current_id);
    }
    let node = state.node(state.current_id)?;
    let mut items = Vec::new();

    // Collect reachable items
    let siblings: Vec<usize> = if let Some(pid) = node.parent {
        state.node(pid).map(|p| {
            p.children.clone()
        }).unwrap_or_default()
    } else {
        Vec::new()
    };
    let children: Vec<usize> = node.children.clone();

    for id in &siblings {
        if *id == node.id { continue; }
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
    } else {
        // Try numbered navigation
        if let Ok(num) = dir.parse::<usize>() {
            if let Some(id) = resolve_number_target(state, num) {
                let desc = state.node(id).map(short_desc).unwrap_or_default();
                state.prev_location = Some(state.current_id);
                state.current_id = id;
                println!("You move to {}.", desc);
                return;
            }
        }
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

// -- Factory functions --

pub fn init() -> Result<Box<dyn crate::backends::BackendApp>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Box::new(ZorkApp))
}

fn find_window_id(state: &ZorkState) -> Option<usize> {
    state.nodes.iter().find(|n| matches!(n.kind, ZorkKind::Window { .. } | ZorkKind::Dialog { .. })).map(|n| n.id)
}

pub fn create_window() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::Window { title: String::new() }, None)))
}

pub fn create_button(label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::Button { label: label.to_string() }, find_window_id(s))))
}

pub fn create_label(text: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::Label { text: text.to_string() }, find_window_id(s))))
}

pub fn create_box(horizontal: bool, spacing: i32) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::BoxWidget { horizontal, spacing }, find_window_id(s))))
}

pub fn create_grid() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::Grid { cols: 0, rows: 0 }, find_window_id(s))))
}

pub fn create_entry() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::Entry { buffer: String::new(), cursor: 0 }, find_window_id(s))))
}

pub fn create_menu() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let id = with_state(|s| s.add_node(ZorkKind::Menu, find_window_id(s)));
    with_state(|s| { s.menu_items.insert(id, Vec::new()); });
    Ok(id)
}

pub fn menu_append(menu_id: usize, label: &str, action: &str) {
    with_state(|s| {
        if let Some(items) = s.menu_items.get_mut(&menu_id) {
            items.push(MenuItemData { label: label.to_string(), action: action.to_string(), submenu: None });
        }
    });
}

pub fn menu_append_submenu(menu_id: usize, label: &str, submenu_id: usize) {
    with_state(|s| {
        let sub_items = s.menu_items.get(&submenu_id).cloned().unwrap_or_default();
        if let Some(items) = s.menu_items.get_mut(&menu_id) {
            items.push(MenuItemData { label: label.to_string(), action: String::new(), submenu: Some(sub_items) });
        }
    });
}

pub fn create_simple_action(_name: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::SimpleAction, find_window_id(s))))
}

pub unsafe fn create_menubar(model_id: usize, _action_group: *mut c_void) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let bar_id = with_state(|s| s.add_node(ZorkKind::MenuBar, find_window_id(s)));
    let items = with_state(|s| s.menu_items.get(&model_id).cloned().unwrap_or_default());
    // Store the top-level item labels on the MenuBar itself
    with_state(|s| {
        s.menu_items.insert(bar_id, items.clone());
    });
    // Create submenu nodes as children
    with_state(|s| {
        for item in &items {
            if let Some(sub) = &item.submenu {
                let sub_id = s.add_node(ZorkKind::Menu, Some(bar_id));
                s.menu_items.insert(sub_id, sub.clone());
            }
        }
    });
    Ok(bar_id)
}

pub fn create_dialog() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::Dialog { title: String::new() }, find_window_id(s))))
}

pub fn create_dropdown(items: &[&str]) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let items_str: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    Ok(with_state(|s| s.add_node(ZorkKind::DropDown { items: items_str, selected: None }, find_window_id(s))))
}

pub fn create_checkbutton(label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::CheckButton { label: label.to_string(), checked: false }, find_window_id(s))))
}

pub fn create_radiobutton(group_id: Option<usize>, label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let gid = group_id.unwrap_or(0);
    Ok(with_state(|s| s.add_node(ZorkKind::RadioButton { label: label.to_string(), checked: false, group_id: gid }, find_window_id(s))))
}

pub fn create_textview() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.add_node(ZorkKind::TextView { text: String::new() }, find_window_id(s))))
}

// -- Setters/getters --

pub fn set_window_title(id: usize, title: &str) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::Window { title: ref mut t } = n.kind {
                *t = title.to_string();
            }
        }
    });
}

pub fn set_label_text(id: usize, text: &str) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::Label { text: ref mut t } = n.kind {
                *t = text.to_string();
            }
        }
    });
}

pub fn get_label_text(id: usize) -> Option<String> {
    with_state(|s| {
        s.node(id).and_then(|n| {
            if let ZorkKind::Label { ref text } = n.kind {
                Some(text.clone())
            } else {
                None
            }
        })
    })
}

pub fn set_label_visible(_id: usize, _visible: bool) {}

pub fn add_callback(id: usize, cb: Box<dyn FnMut()>) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            n.callbacks.push(cb);
        }
    });
}

pub fn set_entry_text(id: usize, text: &str) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                *buffer = text.to_string();
                *cursor = buffer.len();
            }
        }
    });
}

pub fn get_entry_text(id: usize) -> Option<String> {
    with_state(|s| {
        s.node(id).and_then(|n| {
            if let ZorkKind::Entry { ref buffer, .. } = n.kind {
                Some(buffer.clone())
            } else {
                None
            }
        })
    })
}

pub fn set_textview_text(id: usize, text: &str) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::TextView { text: ref mut t } = n.kind {
                *t = text.to_string();
            }
        }
    });
}

pub fn get_textview_text(id: usize) -> Option<String> {
    with_state(|s| {
        s.node(id).and_then(|n| {
            if let ZorkKind::TextView { ref text } = n.kind {
                Some(text.clone())
            } else {
                None
            }
        })
    })
}

pub fn set_dropdown_items(id: usize, items: &[&str]) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::DropDown { items: ref mut items_vec, .. } = n.kind {
                *items_vec = items.iter().map(|s| s.to_string()).collect();
            }
        }
    });
}

pub fn set_dropdown_selected(id: usize, idx: i32) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::DropDown { ref mut selected, ref items } = n.kind {
                *selected = if idx >= 0 && (idx as usize) < items.len() { Some(idx as usize) } else { None };
            }
        }
    });
}

pub fn get_dropdown_selected(id: usize) -> i32 {
    with_state(|s| {
        s.node(id).and_then(|n| {
            if let ZorkKind::DropDown { ref selected, .. } = n.kind {
                selected.map(|s| s as i32)
            } else {
                None
            }
        }).unwrap_or(-1)
    })
}

pub fn get_checkbutton_checked(id: usize) -> bool {
    with_state(|s| {
        s.node(id).map(|n| {
            if let ZorkKind::CheckButton { ref checked, .. } = n.kind {
                *checked
            } else {
                false
            }
        }).unwrap_or(false)
    })
}

pub fn get_radiobutton_checked(id: usize) -> bool {
    with_state(|s| {
        s.node(id).map(|n| {
            if let ZorkKind::RadioButton { ref checked, .. } = n.kind {
                *checked
            } else {
                false
            }
        }).unwrap_or(false)
    })
}

pub fn set_checkbutton_checked(id: usize, checked: bool) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::CheckButton { checked: ref mut c, .. } = n.kind {
                *c = checked;
            }
        }
    });
}

pub fn set_radiobutton_checked(id: usize, checked: bool) {
    with_state(|s| {
        if let Some(n) = s.node_mut(id) {
            if let ZorkKind::RadioButton { checked: ref mut c, .. } = n.kind {
                *c = checked;
            }
        }
    });
}

pub fn set_child(parent_id: usize, child_id: usize) {
    with_state(|s| {
        s.nodes.iter_mut().for_each(|n| n.children.retain(|c| *c != child_id));
        if let Some(parent) = s.node_mut(parent_id) {
            parent.children.push(child_id);
        }
        if let Some(child) = s.node_mut(child_id) {
            child.parent = Some(parent_id);
        }
    });
}

pub fn append_child(parent_id: usize, child_id: usize) {
    set_child(parent_id, child_id);
}

pub fn layout_box(_id: usize) {}

pub fn layout_grid(_id: usize) {}

pub fn entry_set_text(id: usize, text: &str) {
    set_entry_text(id, text);
}

pub fn entry_text(id: usize) -> Option<String> {
    get_entry_text(id)
}

pub fn set_focus(_id: usize) {}

pub fn quit() {
    with_state(|s| s.running = false);
}
