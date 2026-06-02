#[cfg(feature = "pancurses")]
mod pancurses_backend {
    use pancurses::*;
    use std::cell::RefCell;
    use std::os::raw::c_void;

    pub type Callback = Box<dyn FnMut()>;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Rect {
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    #[derive(Clone)]
    pub enum PcWidgetKind {
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

    pub struct PcWidgetNode {
        pub id: usize,
        pub kind: PcWidgetKind,
        pub parent: Option<usize>,
        pub children: Vec<usize>,
        pub rect: Rect,
        pub visible: bool,
        pub callbacks: Vec<Callback>,
    }

    pub struct PcState {
        pub nodes: Vec<PcWidgetNode>,
        pub next_id: usize,
        pub running: bool,
        pub focus_id: Option<usize>,
    }

    impl PcState {
        pub fn new() -> Self {
            PcState {
                nodes: Vec::new(),
                next_id: 1,
                running: true,
                focus_id: None,
            }
        }

        pub fn alloc_id(&mut self) -> usize {
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        pub fn add_node(&mut self, kind: PcWidgetKind, parent: Option<usize>) -> usize {
            let id = self.alloc_id();
            self.nodes.push(PcWidgetNode {
                id,
                kind,
                parent,
                children: Vec::new(),
                rect: Rect::default(),
                visible: true,
                callbacks: Vec::new(),
            });
            if let Some(pid) = parent {
                if let Some(p) = self.nodes.iter_mut().find(|n| n.id == pid) {
                    p.children.push(id);
                }
            }
            id
        }

        pub fn node_mut(&mut self, id: usize) -> Option<&mut PcWidgetNode> {
            self.nodes.iter_mut().find(|n| n.id == id)
        }

        pub fn node(&self, id: usize) -> Option<&PcWidgetNode> {
            self.nodes.iter().find(|n| n.id == id)
        }
    }

    thread_local! {
        static PC_STATE: RefCell<PcState> = RefCell::new(PcState::new());
    }

    fn with_state<F, R>(f: F) -> R
    where
        F: FnOnce(&mut PcState) -> R,
    {
        PC_STATE.with(|s| f(&mut s.borrow_mut()))
    }

    pub struct PcApp;

    impl crate::backends::BackendApp for PcApp {
        fn run(self: Box<Self>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let root = initscr();
            raw();
            noecho();
            root.keypad(true);
            mousemask(ALL_MOUSE_EVENTS | REPORT_MOUSE_POSITION, None);
            half_delay(1);
            curs_set(0);

            if has_colors() {
                start_color();
                init_pair(1, COLOR_WHITE, COLOR_BLUE);
                init_pair(2, COLOR_BLACK, COLOR_WHITE);
                init_pair(3, COLOR_WHITE, COLOR_BLACK);
                init_pair(4, COLOR_YELLOW, COLOR_BLACK);
                init_pair(5, COLOR_GREEN, COLOR_BLACK);
                init_pair(6, COLOR_RED, COLOR_BLACK);
                init_pair(7, COLOR_CYAN, COLOR_BLACK);
            }

            with_state(|state| {
                let (my, mx) = root.get_max_yx();
                for node in &mut state.nodes {
                    match &node.kind {
                        PcWidgetKind::Window { .. } | PcWidgetKind::Dialog { .. } => {
                            node.rect = Rect { x: 0, y: 0, w: mx, h: my };
                        }
                        _ => {}
                    }
                }
                state.running = true;
            });

            while with_state(|s| s.running) {
                let (_max_y, _max_x) = root.get_max_yx();

                // layout — inherit parent size, then auto-layout containers, then clamp
                with_state(|state| {
                    // first pass: propagate sizes from parent to zero-sized children
                    let ids: Vec<usize> = state.nodes.iter().map(|n| n.id).collect();
                    for id in &ids {
                        let rect = state.node(*id).map(|n| n.rect);
                        if let Some(r) = rect {
                            if r.w > 0 && r.h > 0 { continue; }
                            if let Some(pid) = state.node(*id).and_then(|n| n.parent) {
                                let pr = state.node(pid).map(|p| p.rect).unwrap_or(Rect::default());
                                if pr.w > 0 || pr.h > 0 {
                                    if let Some(n) = state.node_mut(*id) {
                                        if n.rect.w == 0 { n.rect.w = pr.w; }
                                        if n.rect.h == 0 { n.rect.h = pr.h; }
                                    }
                                }
                            }
                        }
                    }
                    // second pass: auto-layout containers
                    for id in &ids {
                        let kind = state.node(*id).map(|n| &n.kind).cloned();
                        match kind {
                            Some(PcWidgetKind::BoxWidget { .. }) => {
                                layout_box_inner(*id, state);
                            }
                            Some(PcWidgetKind::Grid { .. }) => {
                                layout_grid_inner(*id, state);
                            }
                            _ => {}
                        }
                    }
                    // third pass: clamp to parent bounds
                    for id in &ids {
                        let node_rect = state.node(*id).map(|n| n.rect);
                        if let Some(rect) = node_rect {
                            if let Some(parent_id) = state.node(*id).and_then(|n| n.parent) {
                                if let Some(parent) = state.node(parent_id) {
                                    let pw = parent.rect.w.max(1);
                                    let ph = parent.rect.h.max(1);
                                    let mut new_rect = rect;
                                    new_rect.x = rect.x.clamp(0, pw - 1);
                                    new_rect.y = rect.y.clamp(0, ph - 1);
                                    new_rect.w = rect.w.min(pw - new_rect.x);
                                    new_rect.h = rect.h.min(ph - new_rect.y);
                                    if let Some(n) = state.node_mut(*id) {
                                        n.rect = new_rect;
                                    }
                                }
                            }
                        }
                    }
                });

                // render — erase screen, then draw all widgets
                root.erase();
                with_state(|state| {
                    for i in 0..state.nodes.len() {
                        let id = state.nodes[i].id;
                        let kind = state.nodes[i].kind.clone();
                        let rect = state.nodes[i].rect;
                        let visible = state.nodes[i].visible;
                        if !visible {
                            continue;
                        }
                        render_widget(&root, &kind, rect, id, state);
                    }
                });
                root.noutrefresh();

                // input
                match root.getch() {
                    Some(Input::KeyResize) => {
                        let (my, mx) = root.get_max_yx();
                        with_state(|state| {
                            for node in &mut state.nodes {
                                match &node.kind {
                                    PcWidgetKind::Window { .. } | PcWidgetKind::Dialog { .. } => {
                                        node.rect = Rect { x: 0, y: 0, w: mx, h: my };
                                    }
                                    _ => {}
                                }
                            }
                        });
                    }
                    Some(Input::KeyMouse) => {
                        if let Ok(mevent) = getmouse() {
                            let x = mevent.x;
                            let y = mevent.y;
                            with_state(|state| {
                                let hit = state.nodes.iter().rev().find(|n| {
                                    n.visible
                                        && n.rect.x <= x && x < n.rect.x + n.rect.w
                                        && n.rect.y <= y && y < n.rect.y + n.rect.h
                                });
                                if let Some(node) = hit {
                                    match &node.kind {
                                        PcWidgetKind::Button { .. } => {
                                            let idxs: Vec<usize> = (0..state.nodes.len())
                                                .filter(|i| state.nodes[*i].id == node.id)
                                                .collect();
                                            for idx in idxs {
                                                for cb in &mut state.nodes[idx].callbacks {
                                                    cb();
                                                }
                                            }
                                        }
                                        PcWidgetKind::CheckButton { .. } => {
                                            if let Some(n) = state.node_mut(node.id) {
                                                if let PcWidgetKind::CheckButton { ref mut checked, .. } = n.kind {
                                                    *checked = !*checked;
                                                }
                                                for cb in &mut n.callbacks {
                                                    cb();
                                                }
                                            }
                                        }
                                        PcWidgetKind::Entry { .. } => {
                                            state.focus_id = Some(node.id);
                                        }
                                        _ => {}
                                    }
                                }
                            });
                        }
                    }
                    Some(Input::Character('\n')) | Some(Input::Character('\r')) => {
                        with_state(|state| {
                            if let Some(fid) = state.focus_id {
                                let idxs: Vec<usize> = (0..state.nodes.len())
                                    .filter(|i| state.nodes[*i].id == fid)
                                    .collect();
                                for idx in idxs {
                                    match &state.nodes[idx].kind {
                                        PcWidgetKind::Button { .. } => {
                                            for cb in &mut state.nodes[idx].callbacks {
                                                cb();
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::Character(c)) => {
                        if c == '\t' {
                            with_state(|state| {
                                let focusable: Vec<usize> = state.nodes.iter()
                                    .filter(|n| matches!(n.kind, PcWidgetKind::Button { .. } | PcWidgetKind::Entry { .. } | PcWidgetKind::CheckButton { .. }))
                                    .map(|n| n.id)
                                    .collect();
                                if let Some(pos) = state.focus_id.and_then(|f| focusable.iter().position(|&x| x == f)) {
                                    let next = (pos + 1) % focusable.len();
                                    state.focus_id = Some(focusable[next]);
                                } else {
                                    state.focus_id = focusable.first().copied();
                                }
                            });
                        } else if c == '\x1b' {
                            with_state(|state| state.running = false);
                        } else {
                            with_state(|state| {
                                if let Some(fid) = state.focus_id {
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                                            buffer.insert(*cursor, c);
                                            *cursor += 1;
                                        }
                                    }
                                }
                            });
                        }
                    }
                    Some(Input::KeyBackspace) => {
                        with_state(|state| {
                            if let Some(fid) = state.focus_id {
                                if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                                        if *cursor > 0 {
                                            *cursor -= 1;
                                            buffer.remove(*cursor);
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyDC) => {
                        with_state(|state| {
                            if let Some(fid) = state.focus_id {
                                if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                                        if *cursor < buffer.len() {
                                            buffer.remove(*cursor);
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyLeft) => {
                        with_state(|state| {
                            if let Some(fid) = state.focus_id {
                                if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut cursor, .. } = n.kind {
                                        if *cursor > 0 {
                                            *cursor -= 1;
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyRight) => {
                        with_state(|state| {
                            if let Some(fid) = state.focus_id {
                                if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut cursor, ref buffer } = n.kind {
                                        if *cursor < buffer.len() {
                                            *cursor += 1;
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeySTab) => {
                        with_state(|state| {
                            let focusable: Vec<usize> = state.nodes.iter()
                                .filter(|n| matches!(n.kind, PcWidgetKind::Button { .. } | PcWidgetKind::Entry { .. } | PcWidgetKind::CheckButton { .. }))
                                .map(|n| n.id)
                                .collect();
                            if let Some(pos) = state.focus_id.and_then(|f| focusable.iter().position(|&x| x == f)) {
                                let prev = if pos == 0 { focusable.len() - 1 } else { pos - 1 };
                                state.focus_id = Some(focusable[prev]);
                            } else {
                                state.focus_id = focusable.last().copied();
                            }
                        });
                    }
                    Some(Input::KeyExit) => {
                        with_state(|state| state.running = false);
                    }
                    _ => {}
                }
            }

            endwin();
            Ok(())
        }
    }

    fn render_widget(root: &Window, kind: &PcWidgetKind, rect: Rect, id: usize, state: &PcState) {
        match kind {
            PcWidgetKind::Window { title } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(7));
                }
                for x in rect.x..rect.x + rect.w {
                    root.mvaddch(rect.y, x, '─');
                    root.mvaddch(rect.y + rect.h - 1, x, '─');
                }
                for y in rect.y..rect.y + rect.h {
                    root.mvaddch(y, rect.x, '│');
                    root.mvaddch(y, rect.x + rect.w - 1, '│');
                }
                root.mvaddch(rect.y, rect.x, '┌');
                root.mvaddch(rect.y, rect.x + rect.w - 1, '┐');
                root.mvaddch(rect.y + rect.h - 1, rect.x, '└');
                root.mvaddch(rect.y + rect.h - 1, rect.x + rect.w - 1, '┘');
                if has_colors() {
                    root.attroff(COLOR_PAIR(7));
                }
                let title_x = rect.x + (rect.w - title.len() as i32) / 2;
                if title_x > rect.x {
                    root.mvaddstr(rect.y, title_x, title);
                }
            }
            PcWidgetKind::Button { label } => {
                let focused = state.focus_id == Some(id);
                if focused && has_colors() {
                    root.attron(COLOR_PAIR(2));
                } else if has_colors() {
                    root.attron(COLOR_PAIR(1));
                }
                let inner_w = rect.w - 2;
                let display = if label.len() as i32 > inner_w {
                    let len = inner_w.max(0) as usize;
                    label[..len].to_string()
                } else {
                    label.clone()
                };
                let pad = (inner_w - display.len() as i32).max(0);
                let left_pad = pad / 2;
                let right_pad = pad - left_pad;
                root.mvaddch(rect.y, rect.x, '[');
                root.mvaddstr(rect.y, rect.x + 1 + left_pad, &display);
                root.mvaddch(rect.y, rect.x + 1 + left_pad + display.len() as i32 + right_pad, ']');
                if has_colors() {
                    root.attroff(COLOR_PAIR(1) | COLOR_PAIR(2));
                }
            }
            PcWidgetKind::Label { text } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(3));
                }
                let max_w = rect.w as usize;
                let truncated = if text.len() > max_w { &text[..max_w] } else { text.as_str() };
                root.mvaddstr(rect.y, rect.x, truncated);
                if has_colors() {
                    root.attroff(COLOR_PAIR(3));
                }
            }
            PcWidgetKind::Entry { buffer, cursor } => {
                let focused = state.focus_id == Some(id);
                if focused && has_colors() {
                    root.attron(COLOR_PAIR(1));
                } else if has_colors() {
                    root.attron(COLOR_PAIR(3));
                }
                let max_w = rect.w as usize;
                let display = if buffer.len() > max_w {
                    &buffer[buffer.len() - max_w..]
                } else {
                    buffer.as_str()
                };
                root.mvaddstr(rect.y, rect.x, display);
                let rest = max_w.saturating_sub(display.len());
                for i in 0..rest {
                    root.mvaddch(rect.y, rect.x + display.len() as i32 + i as i32, ' ');
                }
                if focused {
                    let cursor_display = cursor.saturating_sub(buffer.len().saturating_sub(max_w));
                    let ch = '_' as u32 | A_REVERSE as u32;
                    root.mvaddch(rect.y, rect.x + cursor_display as i32, ch);
                }
                if has_colors() {
                    root.attroff(COLOR_PAIR(1) | COLOR_PAIR(3));
                }
            }
            PcWidgetKind::CheckButton { label, checked } => {
                let focused = state.focus_id == Some(id);
                if focused && has_colors() {
                    root.attron(COLOR_PAIR(2));
                } else if has_colors() {
                    root.attron(COLOR_PAIR(3));
                }
                let mark = if *checked { "[x]" } else { "[ ]" };
                root.mvaddstr(rect.y, rect.x, mark);
                let max_lbl = (rect.w - 4) as usize;
                let truncated = if label.len() > max_lbl { &label[..max_lbl] } else { label.as_str() };
                root.mvaddstr(rect.y, rect.x + 4, truncated);
                if has_colors() {
                    root.attroff(COLOR_PAIR(2) | COLOR_PAIR(3));
                }
            }
            PcWidgetKind::RadioButton { label, checked, .. } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(3));
                }
                let mark = if *checked { "(•)" } else { "( )" };
                root.mvaddstr(rect.y, rect.x, mark);
                let max_lbl = (rect.w - 4) as usize;
                let truncated = if label.len() > max_lbl { &label[..max_lbl] } else { label.as_str() };
                root.mvaddstr(rect.y, rect.x + 4, truncated);
                if has_colors() {
                    root.attroff(COLOR_PAIR(3));
                }
            }
            PcWidgetKind::BoxWidget { .. } | PcWidgetKind::Grid { .. } => {}
            PcWidgetKind::Dialog { title } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(7));
                }
                for x in rect.x..rect.x + rect.w {
                    root.mvaddch(rect.y, x, '═');
                    root.mvaddch(rect.y + rect.h - 1, x, '═');
                }
                for y in rect.y..rect.y + rect.h {
                    root.mvaddch(y, rect.x, '║');
                    root.mvaddch(y, rect.x + rect.w - 1, '║');
                }
                root.mvaddch(rect.y, rect.x, '╔');
                root.mvaddch(rect.y, rect.x + rect.w - 1, '╗');
                root.mvaddch(rect.y + rect.h - 1, rect.x, '╚');
                root.mvaddch(rect.y + rect.h - 1, rect.x + rect.w - 1, '╝');
                if has_colors() {
                    root.attroff(COLOR_PAIR(7));
                }
                root.mvaddstr(rect.y, rect.x + 2, title);
            }
            PcWidgetKind::Menu | PcWidgetKind::MenuBar => {
                if has_colors() {
                    root.attron(COLOR_PAIR(4));
                }
                for x in rect.x..rect.x + rect.w {
                    root.mvaddch(rect.y, x, ' ');
                }
                if has_colors() {
                    root.attroff(COLOR_PAIR(4));
                }
            }
            PcWidgetKind::SimpleAction => {}
            PcWidgetKind::DropDown { items, selected } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(1));
                }
                let current = selected.and_then(|s| items.get(s)).map(|s| s.as_str()).unwrap_or("");
                let max_w = (rect.w - 2) as usize;
                let truncated = if current.len() > max_w { &current[..max_w] } else { current };
                root.mvaddch(rect.y, rect.x, '[');
                root.mvaddstr(rect.y, rect.x + 1, truncated);
                root.mvaddch(rect.y, rect.x + 1 + truncated.len() as i32, 'v');
                let rest = max_w.saturating_sub(truncated.len() + 1);
                for i in 0..rest {
                    root.mvaddch(rect.y, rect.x + 2 + truncated.len() as i32 + i as i32, ' ');
                }
                root.mvaddch(rect.y, rect.x + rect.w - 1, ']');
                if has_colors() {
                    root.attroff(COLOR_PAIR(1));
                }
            }
            PcWidgetKind::TextView { text } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(3));
                }
                let max_w = rect.w as usize;
                let max_h = rect.h as usize;
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate().take(max_h) {
                    let truncated = if line.len() > max_w { &line[..max_w] } else { line };
                    root.mvaddstr(rect.y + i as i32, rect.x, truncated);
                }
                if has_colors() {
                    root.attroff(COLOR_PAIR(3));
                }
            }
        }
    }

    // -- Factory functions --

    pub fn init() -> Result<Box<dyn crate::backends::BackendApp>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Box::new(PcApp))
    }

    fn find_window_id(state: &PcState) -> Option<usize> {
        state.nodes.iter().find(|n| matches!(n.kind, PcWidgetKind::Window { .. })).map(|n| n.id)
    }

    pub fn create_window() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Window { title: String::new() }, None)))
    }

    pub fn create_button(label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Button { label: label.to_string() }, find_window_id(s))))
    }

    pub fn create_label(text: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Label { text: text.to_string() }, find_window_id(s))))
    }

    pub fn create_box(horizontal: bool, spacing: i32) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::BoxWidget { horizontal, spacing }, find_window_id(s))))
    }

    pub fn create_grid() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Grid { cols: 0, rows: 0 }, find_window_id(s))))
    }

    pub fn create_entry() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Entry { buffer: String::new(), cursor: 0 }, find_window_id(s))))
    }

    pub fn create_menu() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Menu, find_window_id(s))))
    }

    pub fn create_simple_action(_name: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::SimpleAction, find_window_id(s))))
    }

    pub unsafe fn create_menubar(_model: usize, _action_group: *mut c_void) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::MenuBar, find_window_id(s))))
    }

    pub fn create_dialog() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::Dialog { title: String::new() }, find_window_id(s))))
    }

    pub fn create_dropdown(items: &[&str]) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let items_str: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        Ok(with_state(|s| s.add_node(PcWidgetKind::DropDown { items: items_str, selected: None }, find_window_id(s))))
    }

    pub fn create_checkbutton(label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::CheckButton { label: label.to_string(), checked: false }, find_window_id(s))))
    }

    pub fn create_radiobutton(group_id: Option<usize>, label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let gid = group_id.unwrap_or(0);
        Ok(with_state(|s| s.add_node(PcWidgetKind::RadioButton { label: label.to_string(), checked: false, group_id: gid }, find_window_id(s))))
    }

    pub fn create_textview() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Ok(with_state(|s| s.add_node(PcWidgetKind::TextView { text: String::new() }, find_window_id(s))))
    }

    pub fn set_window_title(id: usize, title: &str) {
        with_state(|s| {
            if let Some(n) = s.node_mut(id) {
                if let PcWidgetKind::Window { title: ref mut t } = n.kind {
                    *t = title.to_string();
                }
            }
        });
    }

    pub fn set_label_text(id: usize, text: &str) {
        with_state(|s| {
            if let Some(n) = s.node_mut(id) {
                if let PcWidgetKind::Label { text: ref mut t } = n.kind {
                    *t = text.to_string();
                }
            }
        });
    }

    pub fn get_label_text(id: usize) -> Option<String> {
        with_state(|s| {
            s.node(id).and_then(|n| {
                if let PcWidgetKind::Label { ref text } = n.kind {
                    Some(text.clone())
                } else {
                    None
                }
            })
        })
    }

    pub fn set_label_visible(id: usize, visible: bool) {
        with_state(|s| {
            if let Some(n) = s.node_mut(id) {
                n.visible = visible;
            }
        });
    }

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
                if let PcWidgetKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                    *buffer = text.to_string();
                    *cursor = buffer.len();
                }
            }
        });
    }

    pub fn get_entry_text(id: usize) -> Option<String> {
        with_state(|s| {
            s.node(id).and_then(|n| {
                if let PcWidgetKind::Entry { ref buffer, .. } = n.kind {
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
                if let PcWidgetKind::TextView { text: ref mut t } = n.kind {
                    *t = text.to_string();
                }
            }
        });
    }

    pub fn get_textview_text(id: usize) -> Option<String> {
        with_state(|s| {
            s.node(id).and_then(|n| {
                if let PcWidgetKind::TextView { ref text } = n.kind {
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
                if let PcWidgetKind::DropDown { items: ref mut items_vec, .. } = n.kind {
                    *items_vec = items.iter().map(|s| s.to_string()).collect();
                }
            }
        });
    }

    pub fn set_dropdown_selected(id: usize, idx: i32) {
        with_state(|s| {
            if let Some(n) = s.node_mut(id) {
                if let PcWidgetKind::DropDown { ref mut selected, ref items } = n.kind {
                    *selected = if idx >= 0 && (idx as usize) < items.len() { Some(idx as usize) } else { None };
                }
            }
        });
    }

    pub fn get_dropdown_selected(id: usize) -> i32 {
        with_state(|s| {
            s.node(id).and_then(|n| {
                if let PcWidgetKind::DropDown { ref selected, .. } = n.kind {
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
                if let PcWidgetKind::CheckButton { ref checked, .. } = n.kind {
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
                if let PcWidgetKind::RadioButton { ref checked, .. } = n.kind {
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
                if let PcWidgetKind::CheckButton { checked: ref mut c, .. } = n.kind {
                    *c = checked;
                }
            }
        });
    }

    pub fn set_radiobutton_checked(id: usize, checked: bool) {
        with_state(|s| {
            if let Some(n) = s.node_mut(id) {
                if let PcWidgetKind::RadioButton { checked: ref mut c, .. } = n.kind {
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

    fn layout_box_inner(id: usize, s: &mut PcState) -> usize {
        let (horizontal, spacing, children) = {
            let node = s.node(id);
            if node.is_none() { return 0; }
            let n = node.unwrap();
            match &n.kind {
                PcWidgetKind::BoxWidget { horizontal, spacing } => {
                    (*horizontal, *spacing, n.children.clone())
                }
                _ => return 0,
            }
        };

        if children.is_empty() { return 0; }

        let parent_rect = s.node(id).map(|n| n.rect).unwrap_or(Rect::default());
        let total_children = children.len() as i32;
        let total_spacing = spacing * (total_children - 1).max(0);

        if horizontal {
            let total_w = parent_rect.w.saturating_sub(total_spacing).max(1);
            let per_child = if total_children > 0 { (total_w / total_children).max(1) } else { 1 };
            let mut x = parent_rect.x;
            for child_id in &children {
                if let Some(n) = s.node_mut(*child_id) {
                    n.rect = Rect { x, y: parent_rect.y, w: per_child, h: parent_rect.h.max(1) };
                }
                x += per_child + spacing;
            }
        } else {
            let total_h = parent_rect.h.saturating_sub(total_spacing).max(1);
            let per_child = if total_children > 0 { (total_h / total_children).max(1) } else { 1 };
            let mut y = parent_rect.y;
            for child_id in &children {
                if let Some(n) = s.node_mut(*child_id) {
                    n.rect = Rect { x: parent_rect.x, y, w: parent_rect.w.max(1), h: per_child };
                }
                y += per_child + spacing;
            }
        }
        children.len()
    }

    pub fn layout_box(id: usize) {
        with_state(|s| { layout_box_inner(id, s); });
    }

    fn layout_grid_inner(id: usize, s: &mut PcState) -> usize {
        let (cols, rows, children) = {
            let node = s.node(id);
            if node.is_none() { return 0; }
            let n = node.unwrap();
            match &n.kind {
                PcWidgetKind::Grid { cols, rows } => (*cols.max(&1), *rows.max(&1), n.children.clone()),
                _ => return 0,
            }
        };

        if children.is_empty() { return 0; }

        let parent_rect = s.node(id).map(|n| n.rect).unwrap_or(Rect::default());
        let ncols = cols.max(1);
        let nrows = rows.max(1);
        let cell_w = (parent_rect.w / ncols as i32).max(1);
        let cell_h = (parent_rect.h / nrows as i32).max(1);

        for (i, child_id) in children.iter().enumerate() {
            let col = i as i32 % ncols as i32;
            let row = i as i32 / ncols as i32;
            if let Some(n) = s.node_mut(*child_id) {
                n.rect = Rect {
                    x: parent_rect.x + col * cell_w,
                    y: parent_rect.y + row * cell_h,
                    w: cell_w,
                    h: cell_h,
                };
            }
        }
        children.len()
    }

    pub fn layout_grid(id: usize) {
        with_state(|s| { layout_grid_inner(id, s); });
    }

    pub fn entry_set_text(id: usize, text: &str) {
        set_entry_text(id, text);
    }

    pub fn entry_text(id: usize) -> Option<String> {
        get_entry_text(id)
    }

    pub fn set_focus(id: usize) {
        with_state(|s| s.focus_id = Some(id));
    }

    pub fn quit() {
        with_state(|s| s.running = false);
    }
}

pub use pancurses_backend::*;
