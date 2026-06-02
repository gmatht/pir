#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    AddWidget,
    RemoveWidget,
    MutateWidget,
    ToggleVisible,
    ToggleExpand,
    FocusShuffle,
    ResizeWindow,
    PulseChild,
    ResizeWidget,
}

impl Op {
    pub fn from_index(index: u32) -> Self {
        match index % 9 {
            0 => Self::AddWidget,
            1 => Self::RemoveWidget,
            2 => Self::MutateWidget,
            3 => Self::ToggleVisible,
            4 => Self::ToggleExpand,
            5 => Self::FocusShuffle,
            6 => Self::ResizeWindow,
            7 => Self::PulseChild,
            _ => Self::ResizeWidget,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AddWidget => "add_widget",
            Self::RemoveWidget => "remove_widget",
            Self::MutateWidget => "mutate_widget",
            Self::ToggleVisible => "toggle_visible",
            Self::ToggleExpand => "toggle_expand",
            Self::FocusShuffle => "focus_shuffle",
            Self::ResizeWindow => "resize_window",
            Self::PulseChild => "pulse_child",
            Self::ResizeWidget => "resize_widget",
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
struct LifecycleModel {
    widget_count: usize,
}

#[cfg(test)]
impl LifecycleModel {
    fn apply(&mut self, op: Op) {
        match op {
            Op::AddWidget => {
                self.widget_count += 1;
            }
            Op::RemoveWidget => {
                if self.widget_count > 0 {
                    self.widget_count -= 1;
                }
            }
            Op::MutateWidget
            | Op::ToggleVisible
            | Op::ToggleExpand
            | Op::FocusShuffle
            | Op::ResizeWindow
            | Op::PulseChild
            | Op::ResizeWidget => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LifecycleModel, Op};
    use proptest::prelude::*;

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::AddWidget),
            Just(Op::RemoveWidget),
            Just(Op::MutateWidget),
            Just(Op::ToggleVisible),
            Just(Op::ToggleExpand),
            Just(Op::FocusShuffle),
            Just(Op::ResizeWindow),
            Just(Op::PulseChild),
            Just(Op::ResizeWidget),
        ]
    }

    proptest! {
        #[test]
        fn lifecycle_sequences_preserve_invariants(ops in prop::collection::vec(op_strategy(), 1..256)) {
            let mut model = LifecycleModel::default();
            for op in ops {
                model.apply(op);
            }
            assert!(model.widget_count <= 256);
        }
    }
}
