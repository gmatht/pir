use crate::core::action::Action;

pub fn keyval_to_action(keyval: u32, _state: &KeyState) -> Option<Action> {
    use crate::gui::edit::{KEY_LEFT, KEY_RIGHT, KEY_UP, KEY_DOWN,
        KEY_RETURN, KEY_ENTER, KEY_ESC, KEY_TAB,
        KEY_HOME, KEY_END, KEY_PAGE_UP, KEY_PAGE_DOWN,
        KEY_DELETE, KEY_F2};

    match keyval {
        KEY_LEFT => Some(Action::MoveLeft),
        KEY_RIGHT => Some(Action::MoveRight),
        KEY_UP => Some(Action::MoveUp),
        KEY_DOWN => Some(Action::MoveDown),
        KEY_HOME => Some(Action::MoveHome),
        KEY_END => Some(Action::MoveEnd),
        KEY_PAGE_UP => Some(Action::MovePageUp),
        KEY_PAGE_DOWN => Some(Action::MovePageDown),
        KEY_RETURN | KEY_ENTER => Some(Action::StartEdit),
        KEY_ESC => Some(Action::CancelEdit),
        KEY_TAB => Some(Action::MoveRight),
        KEY_DELETE => Some(Action::DeleteSelection),
        KEY_F2 => Some(Action::StartEdit),
        _ => None,
    }
}

pub struct KeyState {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl KeyState {
    pub fn new() -> Self {
        KeyState { shift: false, ctrl: false, alt: false }
    }
}
