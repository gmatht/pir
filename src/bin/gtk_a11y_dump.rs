//! Dump the GTK accessibility (AT-SPI2) tree of a running corro instance.
//!
//! Usage:
//!   cargo run --bin gtk_a11y_dump --features gui
//!
//! This tool launches corro with GTK accessibility enabled, then uses a
//! Python helper script to walk the AT-SPI2 tree and print all accessible
//! object names, roles, and relationships.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Check prerequisites
    let python_check = Command::new("python3")
        .args(["-c", "import dbus; print('ok')"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if python_check.is_err() || python_check.unwrap().code() != Some(0) {
        eprintln!("This tool requires Python 3 with the 'dbus' module.");
        eprintln!("Install: apt install python3-dbus");
        std::process::exit(1);
    }

    // Enable accessibility and launch corro
    let mut child = launch_corro()?;

    // Wait for the window to appear
    eprintln!("Waiting for GTK window...");
    std::thread::sleep(Duration::from_secs(2));

    // Run the Python a11y tree walker
    let labels = walk_a11y_tree()?;

    // Print results
    println!("=== Accessible object labels ({}) ===", labels.len());
    for label in &labels {
        println!("{}", label);
    }

    println!();
    println!("=== Alpha-only words (len >= 3) ===");
    let words: BTreeSet<&str> = labels
        .iter()
        .flat_map(|s| s.split(|c: char| !c.is_ascii_alphabetic()))
        .filter(|w| w.len() >= 3)
        .collect();
    for w in &words {
        println!("{}", w);
    }

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn launch_corro() -> Result<Child, Box<dyn std::error::Error>> {
    // Determine the corro binary path: look next to our own binary
    let self_path = std::env::current_exe()?;
    let corro_path = self_path.parent()
        .map(|p| p.join("corro"))
        .unwrap_or_else(|| PathBuf::from("./corro"));
    let mut cmd = Command::new(&corro_path);
    cmd.arg("--gui");
    cmd.env("GTK_A11Y", "yes");
    cmd.env("GSETTINGS_BACKEND", "memory");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    let child = cmd.spawn().map_err(|e| format!("Failed to launch corro: {e}"))?;
    eprintln!("Launched corro (PID {}) from {:?}", child.id(), corro_path);
    Ok(child)
}

fn walk_a11y_tree() -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let script = r##"import dbus, sys, time

bus = dbus.SessionBus()

# Enable accessibility via registry
registry_obj = bus.get_object('org.a11y.atspi.Registry', '/org/a11y/atspi/registry')
registry = dbus.Interface(registry_obj, 'org.a11y.atspi.Registry')
try:
    registry.Set('')
except:
    pass

time.sleep(1.0)

try:
    root_path, root_iface = registry.GetRoot(dbus_interface='org.a11y.atspi.Registry')
except Exception as e:
    print("Error getting root:", e, file=sys.stderr)
    sys.exit(1)

visited = set()
labels = set()

def get_name(obj_path):
    try:
        obj = bus.get_object('org.a11y.atspi.Registry', obj_path)
        iface = dbus.Interface(obj, 'org.a11y.atspi.Accessible')
        name = iface.GetName()
        if name and name.strip():
            return name.strip()
    except:
        pass
    return ''

def get_role_name(obj_path):
    try:
        obj = bus.get_object('org.a11y.atspi.Registry', obj_path)
        iface = dbus.Interface(obj, 'org.a11y.atspi.Accessible')
        role = iface.GetRole()
        roles = ['invalid', 'accelerator_label', 'alert', 'animation', 'arrow', 'calendar', 'canvas', 'check_box', 'check_menu_item', 'color_chooser', 'column_header', 'combo_box', 'date_editor', 'desktop_icon', 'desktop_frame', 'dial', 'dialog', 'directory_pane', 'drawing_area', 'file_chooser', 'filler', 'focus_traversable', 'font_chooser', 'frame', 'glass_pane', 'html_container', 'icon', 'image', 'internal_frame', 'label', 'layered_pane', 'list', 'list_item', 'menu', 'menu_bar', 'menu_item', 'option_pane', 'page_tab', 'page_tab_list', 'panel', 'password_text', 'popup_menu', 'progress_bar', 'push_button', 'radio_button', 'radio_menu_item', 'root_pane', 'row_header', 'scroll_bar', 'scroll_pane', 'separator', 'slider', 'spin_button', 'split_pane', 'status_bar', 'table', 'table_cell', 'table_column_header', 'table_row_header', 'tear_off_menu_item', 'terminal', 'text', 'toggle_button', 'tool_bar', 'tool_tip', 'tree', 'tree_table', 'unknown', 'viewport', 'window', 'extended', 'header', 'footer', 'paragraph', 'heading', 'page', 'section', 'list_box', 'description', 'application']
        if 0 <= role < len(roles):
            return roles[role]
        return str(role)
    except:
        return '?'

def get_child_count(obj_path):
    try:
        obj = bus.get_object('org.a11y.atspi.Registry', obj_path)
        iface = dbus.Interface(obj, 'org.a11y.atspi.Accessible')
        return iface.GetChildCount()
    except:
        return 0

def get_child_at(obj_path, idx):
    try:
        obj = bus.get_object('org.a11y.atspi.Registry', obj_path)
        iface = dbus.Interface(obj, 'org.a11y.atspi.Accessible')
        return iface.GetChildAtIndex(idx)
    except:
        return ('/', '')

def walk(path, depth=0):
    if path in visited:
        return
    visited.add(path)

    name = get_name(path)
    role_name = get_role_name(path)
    indent = '  ' * depth

    if name:
        labels.add(name)
        print(f"{indent}{name} (role: {role_name})")

    count = get_child_count(path)
    if count > 200:
        count = 200

    for i in range(count):
        try:
            child_path, _ = get_child_at(path, i)
            if child_path and child_path != '/':
                walk(child_path, depth + 1)
        except:
            pass

walk(root_path)

print("---LABELS---")
for label in sorted(labels):
    print(label)
"##;

    let tmp_dir = std::env::temp_dir();
    let py_path = tmp_dir.join("gtk_a11y_dump.py");
    {
        let mut f = std::fs::File::create(&py_path)?;
        f.write_all(script.as_bytes())?;
    }

    let output = Command::new("python3")
        .arg(&py_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("Failed to run Python a11y walker: {e}"))?;

    let _ = std::fs::remove_file(&py_path);

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse the labels from the output
    let mut labels = BTreeSet::new();
    let mut in_labels = false;
    for line in stdout.lines() {
        if line == "---LABELS---" {
            in_labels = true;
            continue;
        }
        if in_labels && !line.is_empty() {
            labels.insert(line.to_string().to_lowercase());
        }
    }

    Ok(labels)
}