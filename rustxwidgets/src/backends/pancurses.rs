#[cfg(feature = "pancurses")]
mod pancurses_backend {
    use pancurses::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::raw::c_void;
    use std::rc::Rc;

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
        MenuBar { labels: Vec<String>, submenu_items: Vec<(String, Vec<(String, String)>)> },
        SimpleAction,
        DropDown { items: Vec<String>, selected: Option<usize> },
        TextView { text: String },
        Spreadsheet {
            cells: Rc<RefCell<HashMap<(u32, u32), String>>>,
            total_rows: u32,
            total_cols: u32,
            top_row: u32,
            left_col: u32,
            cursor_row: u32,
            cursor_col: u32,
            editing: bool,
            edit_buf: String,
            edit_pos: usize,
            col_width: u32,
        },
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
        pub menu_bar_id: Option<usize>,
        pub menu_open: bool,
        pub active_submenu: usize,
        pub active_item: usize,
    }

    impl PcState {
        pub fn new() -> Self {
            PcState {
                nodes: Vec::new(),
                next_id: 1,
                running: true,
                focus_id: None,
                menu_bar_id: None,
                menu_open: false,
                active_submenu: 0,
                active_item: 0,
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
            // Use `root.timeout(100)` so we can temporarily change it for Alt+key detection
            root.timeout(100);
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
                        PcWidgetKind::MenuBar { .. } => {
                            state.menu_bar_id = Some(node.id);
                        }
                        _ => {}
                    }
                }
                state.running = true;
            });

            while with_state(|s| s.running) {
                // layout pass
                with_state(|state| {
                    let ids: Vec<usize> = state.nodes.iter().map(|n| n.id).collect();
                    for id in &ids {
                        let info = state.node(*id).and_then(|n| {
                            let pid = n.parent?;
                            let pn = state.node(pid)?;
                            Some((n.rect, pn.rect, matches!(pn.kind, PcWidgetKind::Dialog { .. })))
                        });
                        if let Some((r, pr, is_dialog)) = info {
                            if (r.w == 0 || r.h == 0) && (pr.w > 0 || pr.h > 0) {
                                if let Some(n) = state.node_mut(*id) {
                                    let ins = if is_dialog { 1 } else { 0 };
                                    if n.rect.w == 0 { n.rect.w = (pr.w - 2 * ins).max(1); }
                                    if n.rect.h == 0 { n.rect.h = (pr.h - 2 * ins).max(1); }
                                    if is_dialog {
                                        if n.rect.x == 0 { n.rect.x = pr.x + 1; }
                                        if n.rect.y == 0 { n.rect.y = pr.y + 1; }
                                    }
                                }
                            }
                        }
                    }
                    for id in &ids {
                        let kind = state.node(*id).map(|n| &n.kind).cloned();
                        match kind {
                            Some(PcWidgetKind::BoxWidget { .. }) => { layout_box_inner(*id, state); }
                            Some(PcWidgetKind::Grid { .. }) => { layout_grid_inner(*id, state); }
                            _ => {}
                        }
                    }
                    for id in &ids {
                        if let Some(rect) = state.node(*id).map(|n| n.rect) {
                            if let Some(pid) = state.node(*id).and_then(|n| n.parent) {
                                let pw = state.node(pid).map(|p| p.rect.w).unwrap_or(1).max(1);
                                let ph = state.node(pid).map(|p| p.rect.h).unwrap_or(1).max(1);
                                let mut nr = rect;
                                nr.x = rect.x.clamp(0, pw - 1);
                                nr.y = rect.y.clamp(0, ph - 1);
                                nr.w = rect.w.min(pw - nr.x);
                                nr.h = rect.h.min(ph - nr.y);
                                if let Some(n) = state.node_mut(*id) { n.rect = nr; }
                            }
                        }
                    }
                });

                // render
                for i in 0..with_state(|s| s.nodes.len()) {
                    let (kind, rect, visible, id, focus_id) = with_state(|s| {
                        let n = &s.nodes[i];
                        (n.kind.clone(), n.rect, n.visible, n.id, s.focus_id)
                    });
                    if !visible { continue; }
                    render_widget(&root, &kind, rect, id, focus_id);
                }
                // render active menu dropdown
                with_state(|state| {
                    if state.menu_open {
                        let mid = state.menu_bar_id.unwrap_or(0);
                        let (sub_idx, item_idx) = (state.active_submenu, state.active_item);
                        if let Some(n) = state.node(mid) {
                            if let PcWidgetKind::MenuBar { labels, submenu_items } = &n.kind {
                                if sub_idx < submenu_items.len() {
                                    let (_, items) = &submenu_items[sub_idx];
                                    let dy = n.rect.y + 1;
                                    let dx = n.rect.x + 1;
                                    // find horizontal position of this submenu label
                                    let mut mx = n.rect.x + 1;
                                    for i in 0..sub_idx {
                                        mx += labels[i].len() as i32 + 2;
                                    }
                                    let max_w = items.iter().map(|(l,_)| l.len()).max().unwrap_or(0).max(4) as i32;
                                    if has_colors() { root.attron(COLOR_PAIR(4)); }
                                    for row in 0..items.len() as i32 {
                                        for col in 0..max_w + 2 {
                                            root.mvaddch(dy + row, mx + col, ' ');
                                        }
                                    }
                                    if has_colors() { root.attroff(COLOR_PAIR(4)); }
                                    for (i, (lbl, _)) in items.iter().enumerate() {
                                        let bg = if i == item_idx && has_colors() { COLOR_PAIR(2) } else { 0 };
                                        if bg != 0 { root.attron(bg); }
                                        root.mvaddstr(dy + i as i32, mx + 1, lbl);
                                        if bg != 0 { root.attroff(bg); }
                                    }
                                }
                            }
                        }
                    }
                });

                root.refresh();
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
                            let callbacks = with_state(|state| {
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
                                            let mut all_cbs = vec![];
                                            for idx in idxs {
                                                all_cbs.append(&mut std::mem::take(&mut state.nodes[idx].callbacks));
                                            }
                                            all_cbs
                                        }
                                        PcWidgetKind::CheckButton { .. } => {
                                            if let Some(n) = state.node_mut(node.id) {
                                                if let PcWidgetKind::CheckButton { ref mut checked, .. } = n.kind {
                                                    *checked = !*checked;
                                                }
                                                std::mem::take(&mut n.callbacks)
                                            } else {
                                                vec![]
                                            }
                                        }
                                        PcWidgetKind::Entry { .. } => {
                                            state.focus_id = Some(node.id);
                                            vec![]
                                        }
                                        _ => vec![],
                                    }
                                } else {
                                    vec![]
                                }
                            });
                            fire_callbacks(callbacks);
                        }
                    }
                    Some(Input::Character('\n')) | Some(Input::Character('\r')) => {
                        let callbacks = with_state(|state| {
                            if state.menu_open {
                                state.menu_open = false;
                                vec![]
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_enter(state, fid);
                                    spreadsheet_scroll_to_cursor(state, fid);
                                    vec![]
                                } else {
                                    toggle_focused(state, fid).1
                                }
                            } else {
                                vec![]
                            }
                        });
                        fire_callbacks(callbacks);
                    }
                    Some(Input::Character(c)) => {
                        if c == '\t' {
                            with_state(|state| {
                                if state.menu_open {
                                    state.menu_open = false;
                                } else if let Some(fid) = state.focus_id {
                                    if is_spreadsheet_focused(state, fid) {
                                        spreadsheet_commit_edit(state, fid);
                                        if let Some(n) = state.node_mut(fid) {
                                            if let PcWidgetKind::Spreadsheet { ref mut cursor_col, total_cols, .. } = n.kind {
                                                if *cursor_col + 1 < total_cols { *cursor_col += 1; }
                                            }
                                        }
                                        spreadsheet_scroll_to_cursor(state, fid);
                                    } else {
                                        let focusable: Vec<usize> = state.nodes.iter()
                                            .filter(|n| matches!(n.kind, PcWidgetKind::Button { .. } | PcWidgetKind::Entry { .. } | PcWidgetKind::CheckButton { .. } | PcWidgetKind::Spreadsheet { .. }))
                                            .map(|n| n.id)
                                            .collect();
                                        if let Some(pos) = state.focus_id.and_then(|f| focusable.iter().position(|&x| x == f)) {
                                            let next = (pos + 1) % focusable.len();
                                            state.focus_id = Some(focusable[next]);
                                        } else {
                                            state.focus_id = focusable.first().copied();
                                        }
                                    }
                                }
                            });
                        } else if c == '\x1b' {
                            // Distinguish bare Escape from Alt+key (terminal sends ESC + char)
                            root.timeout(300);
                            let alt_key = root.getch();
                            root.timeout(100);
                            match alt_key {
                                Some(Input::Character(ac)) => {
                                    // Alt+key — activate matching submenu
                                    with_state(|state| {
                                        if let Some(mid) = state.menu_bar_id {
                                            if let Some(n) = state.node(mid) {
                                                if let PcWidgetKind::MenuBar { labels, .. } = &n.kind {
                                                    let lower = ac.to_ascii_lowercase();
                                                    if let Some(pos) = labels.iter().position(|l| l.to_ascii_lowercase().starts_with(&lower.to_string())) {
                                                        state.active_submenu = pos;
                                                        state.active_item = 0;
                                                        state.menu_open = true;
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }
                                _ => {
                                    // No following char within 300ms → bare Escape → close edit/menu or quit
                                    with_state(|state| {
                                        if let Some(fid) = state.focus_id {
                                            if is_spreadsheet_focused(state, fid) {
                                                if let Some(n) = state.node_mut(fid) {
                                                    if let PcWidgetKind::Spreadsheet { ref mut editing, .. } = n.kind {
                                                        if *editing {
                                                            *editing = false; // cancel edit
                                                            return;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if state.menu_open {
                                            state.menu_open = false;
                                        } else {
                                            state.running = false;
                                        }
                                    });
                                }
                            }
                        } else if c == '\x03' {
                            // Ctrl+C — quit
                            with_state(|state| state.running = false);
                        } else if c == ' ' {
                            let (_, callbacks) = with_state(|state| {
                                if let Some(fid) = state.focus_id {
                                    let (toggled, cbs) = toggle_focused(state, fid);
                                    if !toggled {
                                        // Not a toggleable widget — insert space as text
                                        let idxs: Vec<usize> = (0..state.nodes.len())
                                            .filter(|i| state.nodes[*i].id == fid)
                                            .collect();
                                        for idx in idxs {
                                            if let PcWidgetKind::Entry { ref mut buffer, ref mut cursor } = &mut state.nodes[idx].kind {
                                                buffer.insert(*cursor, ' ');
                                                *cursor += 1;
                                            }
                                        }
                                    }
                                    (toggled, cbs)
                                } else {
                                    (false, vec![])
                                }
                            });
                            fire_callbacks(callbacks);
                        } else {
                            with_state(|state| {
                                if let Some(fid) = state.focus_id {
                                    if is_spreadsheet_focused(state, fid) {
                                        if let Some(n) = state.node_mut(fid) {
                                            if let PcWidgetKind::Spreadsheet { ref mut editing, ref mut edit_buf, ref mut edit_pos, .. } = n.kind {
                                                if !*editing {
                                                    *editing = true;
                                                    *edit_buf = c.to_string();
                                                    *edit_pos = 1;
                                                } else {
                                                    edit_buf.insert(*edit_pos, c);
                                                    *edit_pos += 1;
                                                }
                                            }
                                        }
                                    } else if let Some(n) = state.node_mut(fid) {
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
                                if is_spreadsheet_focused(state, fid) {
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut editing, ref mut edit_buf, ref mut edit_pos, .. } = n.kind {
                                            if *editing && *edit_pos > 0 {
                                                *edit_pos -= 1;
                                                edit_buf.remove(*edit_pos);
                                            }
                                        }
                                    }
                                } else if let Some(n) = state.node_mut(fid) {
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
                                if is_spreadsheet_focused(state, fid) {
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut editing, ref mut edit_buf, ref mut edit_pos, .. } = n.kind {
                                            if *editing && *edit_pos < edit_buf.len() {
                                                edit_buf.remove(*edit_pos);
                                            }
                                        }
                                    }
                                } else if let Some(n) = state.node_mut(fid) {
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
                            if state.menu_open {
                                if state.active_submenu > 0 {
                                    state.active_submenu -= 1;
                                    state.active_item = 0;
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_col, .. } = n.kind {
                                            if *cursor_col > 0 { *cursor_col -= 1; }
                                        }
                                    }
                                    spreadsheet_scroll_to_cursor(state, fid);
                                } else if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut cursor, .. } = n.kind {
                                        if *cursor > 0 { *cursor -= 1; }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyRight) => {
                        with_state(|state| {
                            if state.menu_open {
                                if let Some(mid) = state.menu_bar_id {
                                    if let Some(n) = state.node(mid) {
                                        if let PcWidgetKind::MenuBar { labels, .. } = &n.kind {
                                            if state.active_submenu + 1 < labels.len() {
                                                state.active_submenu += 1;
                                                state.active_item = 0;
                                            }
                                        }
                                    }
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_col, total_cols, .. } = n.kind {
                                            if *cursor_col + 1 < total_cols { *cursor_col += 1; }
                                        }
                                    }
                                    spreadsheet_scroll_to_cursor(state, fid);
                                } else if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut cursor, ref buffer } = n.kind {
                                        if *cursor < buffer.len() { *cursor += 1; }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyUp) => {
                        with_state(|state| {
                            if state.menu_open {
                                if let Some(mid) = state.menu_bar_id {
                                    if let Some(n) = state.node(mid) {
                                        if let PcWidgetKind::MenuBar { submenu_items, .. } = &n.kind {
                                            let si = state.active_submenu;
                                            if si < submenu_items.len() && state.active_item > 0 {
                                                state.active_item -= 1;
                                            }
                                        }
                                    }
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_row, .. } = n.kind {
                                            if *cursor_row > 0 { *cursor_row -= 1; }
                                        }
                                    }
                                    spreadsheet_scroll_to_cursor(state, fid);
                                }
                            }
                        });
                    }
                    Some(Input::KeyDown) => {
                        with_state(|state| {
                            if state.menu_open {
                                if let Some(mid) = state.menu_bar_id {
                                    if let Some(n) = state.node(mid) {
                                        if let PcWidgetKind::MenuBar { submenu_items, .. } = &n.kind {
                                            let si = state.active_submenu;
                                            if si < submenu_items.len() && state.active_item + 1 < submenu_items[si].1.len() {
                                                state.active_item += 1;
                                            }
                                        }
                                    }
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_row, total_rows, .. } = n.kind {
                                            if *cursor_row + 1 < total_rows { *cursor_row += 1; }
                                        }
                                    }
                                    spreadsheet_scroll_to_cursor(state, fid);
                                }
                            }
                        });
                    }
                    Some(Input::KeyLeft) => {
                        with_state(|state| {
                            if state.menu_open {
                                if state.active_submenu > 0 {
                                    state.active_submenu -= 1;
                                    state.active_item = 0;
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_col, .. } = n.kind {
                                            if *cursor_col > 0 { *cursor_col -= 1; }
                                        }
                                    }
                                    spreadsheet_scroll_to_cursor(state, fid);
                                } else if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut cursor, .. } = n.kind {
                                        if *cursor > 0 { *cursor -= 1; }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyRight) => {
                        with_state(|state| {
                            if state.menu_open {
                                if let Some(mid) = state.menu_bar_id {
                                    if let Some(n) = state.node(mid) {
                                        if let PcWidgetKind::MenuBar { labels, .. } = &n.kind {
                                            if state.active_submenu + 1 < labels.len() {
                                                state.active_submenu += 1;
                                                state.active_item = 0;
                                            }
                                        }
                                    }
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_col, total_cols, .. } = n.kind {
                                            if *cursor_col + 1 < total_cols { *cursor_col += 1; }
                                        }
                                    }
                                } else if let Some(n) = state.node_mut(fid) {
                                    if let PcWidgetKind::Entry { ref mut cursor, ref buffer } = n.kind {
                                        if *cursor < buffer.len() { *cursor += 1; }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyUp) => {
                        with_state(|state| {
                            if state.menu_open {
                                if let Some(mid) = state.menu_bar_id {
                                    if let Some(n) = state.node(mid) {
                                        if let PcWidgetKind::MenuBar { submenu_items, .. } = &n.kind {
                                            let si = state.active_submenu;
                                            if si < submenu_items.len() && state.active_item > 0 {
                                                state.active_item -= 1;
                                            }
                                        }
                                    }
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_row, .. } = n.kind {
                                            if *cursor_row > 0 { *cursor_row -= 1; }
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Some(Input::KeyDown) => {
                        with_state(|state| {
                            if state.menu_open {
                                if let Some(mid) = state.menu_bar_id {
                                    if let Some(n) = state.node(mid) {
                                        if let PcWidgetKind::MenuBar { submenu_items, .. } = &n.kind {
                                            let si = state.active_submenu;
                                            if si < submenu_items.len() && state.active_item + 1 < submenu_items[si].1.len() {
                                                state.active_item += 1;
                                            }
                                        }
                                    }
                                }
                            } else if let Some(fid) = state.focus_id {
                                if is_spreadsheet_focused(state, fid) {
                                    spreadsheet_commit_edit(state, fid);
                                    if let Some(n) = state.node_mut(fid) {
                                        if let PcWidgetKind::Spreadsheet { ref mut cursor_row, total_rows, .. } = n.kind {
                                            if *cursor_row + 1 < total_rows { *cursor_row += 1; }
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

    fn render_widget(root: &Window, kind: &PcWidgetKind, rect: Rect, id: usize, focus_id: Option<usize>) {
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
                let focused = focus_id == Some(id);
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
                let focused = focus_id == Some(id);
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
                let focused = focus_id == Some(id);
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
            PcWidgetKind::Menu => {
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
            PcWidgetKind::MenuBar { labels, .. } => {
                if has_colors() {
                    root.attron(COLOR_PAIR(4));
                }
                for x in rect.x..rect.x + rect.w {
                    root.mvaddch(rect.y, x, ' ');
                }
                let mut cx = rect.x + 1;
                for label in labels {
                    if cx + label.len() as i32 + 2 > rect.x + rect.w { break; }
                    root.mvaddstr(rect.y, cx, label);
                    cx += label.len() as i32 + 2;
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
            PcWidgetKind::Spreadsheet { ref cells, total_rows: _, total_cols: _, ref top_row, ref left_col, ref cursor_row, ref cursor_col, ref editing, ref edit_buf, ref edit_pos, ref col_width } => {
                let cw = *col_width as i32;
                let rh_w = 4i32; // row header width
                let sep: char = '║';
                // clear widget area
                for dy in 0..rect.h {
                    for dx in 0..rect.w {
                        root.mvaddch(rect.y + dy, rect.x + dx, ' ');
                    }
                }
                let max_data_cols = ((rect.w - rh_w - 1) / (cw + 1)).max(1);
                let max_data_rows = (rect.h - 2).max(1) as u32;
                // draw column headers
                if has_colors() { root.attron(COLOR_PAIR(4)); }
                root.mvaddstr(rect.y, rect.x, "    "); // row header blank
                root.mvaddch(rect.y, rect.x + rh_w, sep);
                for vc in 0..max_data_cols {
                    let col_idx = *left_col + vc as u32;
                    let label = col_label(col_idx);
                    let dx = rect.x + rh_w + 1 + vc * (cw + 1);
                    root.mvaddstr(rect.y, dx, &label);
                }
                if has_colors() { root.attroff(COLOR_PAIR(4)); }
                // draw header separator
                root.mvaddstr(rect.y + 1, rect.x, "═══╬");
                for vc in 0..max_data_cols {
                    let dx = rect.x + rh_w + 1 + vc * (cw + 1);
                    for i in 0..cw as usize {
                        root.mvaddch(rect.y + 1, dx + i as i32, '═');
                    }
                    root.mvaddch(rect.y + 1, dx + cw, sep);
                }
                // draw data rows
                for vr in 0..max_data_rows {
                    let row_idx = *top_row + vr as u32;
                    let ry = rect.y + 2 + vr as i32;
                    // row header
                    if has_colors() { root.attron(COLOR_PAIR(4)); }
                    let rh_text = format!("{:>3} ", row_idx + 1);
                    root.mvaddstr(ry, rect.x, &rh_text);
                    root.mvaddch(ry, rect.x + rh_w, sep);
                    if has_colors() { root.attroff(COLOR_PAIR(4)); }
                    // data cells
                    let cells_ref = cells.borrow();
                    let mut vc = 0i32;
                    while vc < max_data_cols {
                        let col_idx = *left_col + vc as u32;
                        let cell_text = cells_ref.get(&(row_idx, col_idx)).map(|s| s.as_str()).unwrap_or("");
                        let is_cursor = row_idx == *cursor_row && col_idx == *cursor_col;
                        let is_editing = is_cursor && *editing;
                        let dx = rect.x + rh_w + 1 + vc * (cw + 1);
                        if cell_text.is_empty() && !is_editing {
                            // draw empty cell
                            if is_cursor {
                                if has_colors() { root.attron(COLOR_PAIR(2)); }
                                for i in 0..cw { root.mvaddch(ry, dx + i, ' '); }
                                if has_colors() { root.attroff(COLOR_PAIR(2)); }
                            }
                            root.mvaddch(ry, dx + cw, sep);
                            vc += 1;
                            continue;
                        }
                        // determine display text with overflow
                        let text = if is_editing { &edit_buf } else { cell_text };
                        let text_len = text.len() as i32;
                        // scan rightward for overflow into empty cells
                        let mut overflow_cols = 0i32;
                        if !is_editing && text_len > cw {
                            let mut scan = vc + 1;
                            while scan < max_data_cols {
                                let sc = *left_col + scan as u32;
                                if cells_ref.get(&(row_idx, sc)).map_or(true, |s| s.is_empty()) {
                                    overflow_cols += 1;
                                    scan += 1;
                                } else { break; }
                            }
                        }
                        let available = (overflow_cols + 1) * cw;
                        let display = if text_len > available {
                            let trunc = (available - 1).max(1) as usize;
                            let mut s: String = text.chars().take(trunc).collect();
                            s.push('…');
                            s
                        } else {
                            text.to_string()
                        };
                        // draw cell background
                        if is_cursor && has_colors() { root.attron(COLOR_PAIR(2)); }
                        for i in 0..(overflow_cols + 1) * cw {
                            let cx = dx + i;
                            if cx < rect.x + rect.w {
                                root.mvaddch(ry, cx, ' ');
                            }
                        }
                        if is_cursor && has_colors() { root.attroff(COLOR_PAIR(2)); }
                        // draw text
                        if is_editing {
                            // show edit buffer: scroll to show cursor position
                            let buf_len = edit_buf.len();
                            let scroll = if *edit_pos > ((cw - 1) as usize) { *edit_pos - (cw as usize) + 1 } else { 0 };
                            let start = scroll.min(buf_len);
                            let visible: String = edit_buf.chars().skip(start).take(cw as usize).collect();
                            if is_cursor && has_colors() { root.attron(COLOR_PAIR(2)); }
                            root.mvaddstr(ry, dx, &visible);
                            // draw cursor
                            let cursor_col = (*edit_pos - scroll) as i32;
                            if cursor_col < cw && cursor_col >= 0 {
                                let ch = '_' as u32 | A_REVERSE as u32;
                                let cpos = dx + cursor_col;
                                if cpos < rect.x + rect.w {
                                    root.mvaddch(ry, cpos, ch);
                                }
                            }
                            if is_cursor && has_colors() { root.attroff(COLOR_PAIR(2)); }
                        } else {
                            if is_cursor && has_colors() { root.attron(COLOR_PAIR(2)); }
                            root.mvaddstr(ry, dx, &display);
                            if is_cursor && has_colors() { root.attroff(COLOR_PAIR(2)); }
                        }
                        // draw column separators
                        for i in 0..=overflow_cols {
                            let sx = dx + (i + 1) * cw;
                            if sx < rect.x + rect.w {
                                root.mvaddch(ry, sx, sep);
                            }
                        }
                        vc += 1 + overflow_cols;
                    }
                }
            }
        }
    }

    fn is_spreadsheet_focused(state: &PcState, fid: usize) -> bool {
        state.node(fid).map_or(false, |n| matches!(n.kind, PcWidgetKind::Spreadsheet { .. }))
    }

    fn spreadsheet_enter(state: &mut PcState, fid: usize) {
        if let Some(n) = state.node_mut(fid) {
            if let PcWidgetKind::Spreadsheet { ref cells, ref mut cursor_row, ref mut cursor_col, ref mut editing, ref mut edit_buf, ref mut edit_pos, .. } = n.kind {
                if *editing {
                    cells.borrow_mut().insert((*cursor_row, *cursor_col), edit_buf.clone());
                    *editing = false;
                    if *cursor_row + 1 < u32::MAX { *cursor_row += 1; }
                } else {
                    *editing = true;
                    let existing = cells.borrow().get(&(*cursor_row, *cursor_col)).cloned().unwrap_or_default();
                    *edit_buf = existing;
                    *edit_pos = edit_buf.len();
                }
            }
        }
    }

    fn spreadsheet_scroll_to_cursor(state: &mut PcState, fid: usize) {
        if let Some(n) = state.node_mut(fid) {
            if let PcWidgetKind::Spreadsheet { ref mut top_row, ref mut left_col, ref cursor_row, ref cursor_col, ref col_width, .. } = n.kind {
                let cw = *col_width as i32;
                let rh_w = 4i32;
                let max_data_cols = ((n.rect.w - rh_w - 1) / (cw + 1)).max(1);
                let max_data_rows = (n.rect.h - 2).max(1) as u32;
                if *cursor_row < *top_row {
                    *top_row = *cursor_row;
                } else if *cursor_row >= *top_row + max_data_rows {
                    *top_row = *cursor_row - max_data_rows + 1;
                }
                if *cursor_col < *left_col {
                    *left_col = *cursor_col;
                } else if *cursor_col >= *left_col + max_data_cols as u32 {
                    *left_col = *cursor_col - max_data_cols as u32 + 1;
                }
            }
        }
    }

    fn spreadsheet_commit_edit(state: &mut PcState, fid: usize) {
        if let Some(n) = state.node_mut(fid) {
            if let PcWidgetKind::Spreadsheet { ref cells, cursor_row, cursor_col, ref edit_buf, ref mut editing, .. } = n.kind {
                if *editing {
                    cells.borrow_mut().insert((cursor_row, cursor_col), edit_buf.clone());
                    *editing = false;
                }
            }
        }
    }

    fn col_label(idx: u32) -> String {
        if idx < 26 {
            let c = (b'A' + idx as u8) as char;
            c.to_string()
        } else {
            let prefix = (idx / 26 - 1) as u8;
            let suffix = (idx % 26) as u8;
            let mut s = String::new();
            s.push((b'A' + prefix) as char);
            s.push((b'A' + suffix) as char);
            s
        }
    }

    /// Activate the focused widget: toggle CheckButton/RadioButton or fire Button callbacks.
    /// Returns `(was_toggleable, callbacks_to_fire)`. Callbacks must be fired *outside* `with_state`.
    fn toggle_focused(state: &mut PcState, fid: usize) -> (bool, Vec<Callback>) {
        let idx = match state.nodes.iter().position(|n| n.id == fid) {
            Some(i) => i,
            None => return (false, vec![]),
        };
        match state.nodes[idx].kind {
            PcWidgetKind::Button { .. } => {
                let cbs = std::mem::take(&mut state.nodes[idx].callbacks);
                (true, cbs)
            }
            PcWidgetKind::CheckButton { ref mut checked, .. } => {
                *checked = !*checked;
                let cbs = std::mem::take(&mut state.nodes[idx].callbacks);
                (true, cbs)
            }
            PcWidgetKind::RadioButton { group_id, .. } => {
                let gid = group_id;
                // Uncheck all radio buttons in the same group
                for node in &mut state.nodes {
                    if let PcWidgetKind::RadioButton { checked: ref mut oc, group_id: og, .. } = &mut node.kind {
                        if *og == gid { *oc = false; }
                    }
                }
                // Check the focused one
                if let PcWidgetKind::RadioButton { checked: ref mut c, .. } = state.nodes[idx].kind {
                    *c = true;
                }
                let cbs = std::mem::take(&mut state.nodes[idx].callbacks);
                (true, cbs)
            }
            _ => (false, vec![]),
        }
    }

    /// Fire a list of callbacks outside of any `with_state` borrow.
    fn fire_callbacks(callbacks: Vec<Callback>) {
        for mut cb in callbacks { cb(); }
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

    pub unsafe fn create_menubar(submenu_items: Vec<(String, Vec<(String, String)>)>, _action_group: *mut c_void) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let labels: Vec<String> = submenu_items.iter().map(|(l, _)| l.clone()).collect();
        Ok(with_state(|s| s.add_node(PcWidgetKind::MenuBar { labels, submenu_items }, find_window_id(s))))
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

    pub fn create_spreadsheet(rows: u32, cols: u32) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let cells = Rc::new(RefCell::new(HashMap::new()));
        let id = with_state(|s| s.add_node(PcWidgetKind::Spreadsheet {
            cells,
            total_rows: rows,
            total_cols: cols,
            top_row: 0,
            left_col: 0,
            cursor_row: 0,
            cursor_col: 0,
            editing: false,
            edit_buf: String::new(),
            edit_pos: 0,
            col_width: 12,
        }, find_window_id(s)));
        Ok(id)
    }

    pub fn spreadsheet_set_cell(id: usize, r: u32, c: u32, text: &str) {
        with_state(|s| {
            if let Some(n) = s.node_mut(id) {
                if let PcWidgetKind::Spreadsheet { ref cells, .. } = n.kind {
                    cells.borrow_mut().insert((r, c), text.to_string());
                }
            }
        });
    }

    pub fn spreadsheet_get_cell(id: usize, r: u32, c: u32) -> Option<String> {
        with_state(|s| {
            s.node(id).and_then(|n| {
                if let PcWidgetKind::Spreadsheet { ref cells, .. } = n.kind {
                    cells.borrow().get(&(r, c)).cloned()
                } else {
                    None
                }
            })
        })
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
            // Natural-height vertical layout: each child gets 1 row by default,
            // last child stretches to fill any remaining space.
            let mut y = parent_rect.y;
            let natural_h = 1i32;
            let total_used = natural_h * total_children + total_spacing;
            let remaining = parent_rect.h.saturating_sub(total_used);
            for (i, child_id) in children.iter().enumerate() {
                let h = if i as i32 == total_children - 1 {
                    (natural_h + remaining).max(1)
                } else {
                    natural_h
                };
                if let Some(n) = s.node_mut(*child_id) {
                    n.rect = Rect { x: parent_rect.x, y, w: parent_rect.w.max(1), h };
                }
                y += h + spacing;
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
