use crate::gui::sheet::SharedState;

pub const KEY_RETURN: u32 = 0xFF0D;
pub const KEY_ENTER: u32 = 0xFF8D;
pub const KEY_ESC: u32 = 0xFF1B;
pub const KEY_BACKSPACE: u32 = 0xFF08;
pub const KEY_DELETE: u32 = 0xFFFF;
pub const KEY_LEFT: u32 = 0xFF51;
pub const KEY_UP: u32 = 0xFF52;
pub const KEY_RIGHT: u32 = 0xFF53;
pub const KEY_DOWN: u32 = 0xFF54;
pub const KEY_TAB: u32 = 0xFF09;
pub const KEY_HOME: u32 = 0xFF50;
pub const KEY_END: u32 = 0xFF57;
pub const KEY_PAGE_UP: u32 = 0xFF55;
pub const KEY_PAGE_DOWN: u32 = 0xFF56;
pub const KEY_F1: u32 = 0xFFBE;
pub const KEY_F2: u32 = 0xFFBF;

pub enum EditAction {
    Commit(String),
    Cancel,
    Continue,
}

pub fn handle_edit_input(keyval: u32, shared: &SharedState, redraw: &dyn Fn()) -> EditAction {
    match keyval {
        KEY_RETURN | KEY_ENTER => {
            let text = shared.edit_buf.borrow().clone();
            shared.editing.set(false);
            shared.edit_buf.borrow_mut().clear();
            EditAction::Commit(text)
        }
        KEY_ESC => {
            shared.editing.set(false);
            shared.edit_buf.borrow_mut().clear();
            EditAction::Cancel
        }
        KEY_BACKSPACE => {
            shared.edit_buf.borrow_mut().pop();
            redraw();
            EditAction::Continue
        }
        KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN | KEY_TAB | KEY_HOME | KEY_END |
        KEY_PAGE_UP | KEY_PAGE_DOWN | KEY_DELETE | KEY_F2 => EditAction::Continue,
        _ if keyval >= 32 && keyval <= 126 => {
            shared.edit_buf.borrow_mut().push(char::from_u32(keyval).unwrap_or('?'));
            redraw();
            EditAction::Continue
        }
        _ => EditAction::Continue,
    }
}
