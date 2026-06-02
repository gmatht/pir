/// Internal operation set shared by the GTK lifecycle stress tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    SpawnEditor,
    MutateEditor,
    HideEditor,
    ShowEditor,
    RemoveEditor,
    QueueRedraw,
    MutateLabels,
    FocusShuffle,
}

impl Op {
    pub fn from_index(index: u32) -> Self {
        match index % 8 {
            0 => Self::SpawnEditor,
            1 => Self::MutateEditor,
            2 => Self::HideEditor,
            3 => Self::ShowEditor,
            4 => Self::RemoveEditor,
            5 => Self::QueueRedraw,
            6 => Self::MutateLabels,
            _ => Self::FocusShuffle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpawnEditor => "spawn_editor",
            Self::MutateEditor => "mutate_editor",
            Self::HideEditor => "hide_editor",
            Self::ShowEditor => "show_editor",
            Self::RemoveEditor => "remove_editor",
            Self::QueueRedraw => "queue_redraw",
            Self::MutateLabels => "mutate_labels",
            Self::FocusShuffle => "focus_shuffle",
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusTarget {
    Formula,
    Editor(u64),
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
struct LifecycleModel {
    next_entry_id: u64,
    active_editor_id: Option<u64>,
    editor_visible: bool,
    pending_detach: bool,
    focused: Option<FocusTarget>,
    retired_entries: usize,
    idle: Vec<IdleAction>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
enum IdleAction {
    FocusEditor { expected_entry_id: u64 },
    FocusFormula,
    FinishDetach,
}

#[cfg(test)]
impl LifecycleModel {
    fn apply(&mut self, op: Op) {
        match op {
            Op::SpawnEditor => {
                if self.active_editor_id.is_none() && !self.pending_detach {
                    self.next_entry_id += 1;
                    let entry_id = self.next_entry_id;
                    self.active_editor_id = Some(entry_id);
                    self.editor_visible = true;
                    self.idle.push(IdleAction::FocusEditor {
                        expected_entry_id: entry_id,
                    });
                }
            }
            Op::MutateEditor => {}
            Op::HideEditor => {
                if self.active_editor_id.is_some() {
                    self.editor_visible = false;
                }
            }
            Op::ShowEditor => {
                if self.active_editor_id.is_some() {
                    self.editor_visible = true;
                }
            }
            Op::RemoveEditor => {
                if self.active_editor_id.take().is_some() {
                    self.editor_visible = false;
                    self.pending_detach = true;
                    self.focused = None;
                    self.idle.push(IdleAction::FinishDetach);
                }
            }
            Op::QueueRedraw | Op::MutateLabels => {}
            Op::FocusShuffle => {
                if let Some(entry_id) = self.active_editor_id {
                    self.idle.push(IdleAction::FocusEditor {
                        expected_entry_id: entry_id,
                    });
                } else {
                    self.idle.push(IdleAction::FocusFormula);
                }
            }
        }

        self.assert_invariants();
    }

    fn flush_one_idle(&mut self) -> bool {
        if self.idle.is_empty() {
            return false;
        }

        let action = self.idle.remove(0);
        match action {
            IdleAction::FocusEditor { expected_entry_id } => {
                if self.active_editor_id == Some(expected_entry_id) && !self.pending_detach {
                    self.focused = Some(FocusTarget::Editor(expected_entry_id));
                }
            }
            IdleAction::FocusFormula => {
                self.focused = Some(FocusTarget::Formula);
            }
            IdleAction::FinishDetach => {
                self.pending_detach = false;
                self.retired_entries += 1;
                self.idle.push(IdleAction::FocusFormula);
            }
        }

        self.assert_invariants();
        true
    }

    fn flush_all_idle(&mut self) {
        while self.flush_one_idle() {}
    }

    fn assert_invariants(&self) {
        if self.pending_detach {
            assert!(self.active_editor_id.is_none());
            assert!(!self.editor_visible);
        }

        match self.focused {
            Some(FocusTarget::Editor(entry_id)) => {
                assert_eq!(self.active_editor_id, Some(entry_id));
                assert!(!self.pending_detach);
            }
            Some(FocusTarget::Formula) | None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LifecycleModel, Op};
    use proptest::prelude::*;

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::SpawnEditor),
            Just(Op::MutateEditor),
            Just(Op::HideEditor),
            Just(Op::ShowEditor),
            Just(Op::RemoveEditor),
            Just(Op::QueueRedraw),
            Just(Op::MutateLabels),
            Just(Op::FocusShuffle),
        ]
    }

    #[test]
    fn stale_focus_is_ignored_after_remove() {
        let mut model = LifecycleModel::default();

        model.apply(Op::SpawnEditor);
        model.apply(Op::RemoveEditor);
        model.flush_one_idle();

        assert_eq!(model.focused, None);

        model.flush_one_idle();
        model.flush_one_idle();

        assert_eq!(model.focused, Some(super::FocusTarget::Formula));
        assert_eq!(model.retired_entries, 1);
        assert!(!model.pending_detach);
    }

    proptest! {
        #[test]
        fn lifecycle_sequences_preserve_invariants(ops in prop::collection::vec(op_strategy(), 1..256)) {
            let mut model = LifecycleModel::default();

            for op in ops {
                model.apply(op);
                while model.flush_one_idle() {}
            }

            model.flush_all_idle();
            model.assert_invariants();
        }
    }
}
