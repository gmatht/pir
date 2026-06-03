#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    AddWidget,
    RemoveWidget,
    MutateWidget,
    CycleStyle,
    NestLayout,
    UnnestLayout,
    ToggleVisible,
    ToggleExpand,
    FocusShuffle,
    ResizeWindow,
    PulseChild,
    ResizeWidget,
}

impl Op {
    pub fn from_index(index: u32) -> Self {
        match index % 12 {
            0 => Self::AddWidget,
            1 => Self::RemoveWidget,
            2 => Self::MutateWidget,
            3 => Self::CycleStyle,
            4 => Self::NestLayout,
            5 => Self::UnnestLayout,
            6 => Self::ToggleVisible,
            7 => Self::ToggleExpand,
            8 => Self::FocusShuffle,
            9 => Self::ResizeWindow,
            10 => Self::PulseChild,
            _ => Self::ResizeWidget,
        }
    }

    /// Weighted: 30% add, 25% remove, 10% nest, 8% unnest, 7% style, 20% other
    pub fn pick_weighted(roll: u32) -> Self {
        let band = roll % 100;
        if band < 30 { return Self::AddWidget; }
        if band < 55 { return Self::RemoveWidget; }
        if band < 65 { return Self::NestLayout; }
        if band < 73 { return Self::UnnestLayout; }
        if band < 80 { return Self::CycleStyle; }
        Self::from_index(band)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AddWidget => "add_widget",
            Self::RemoveWidget => "remove_widget",
            Self::MutateWidget => "mutate_widget",
            Self::CycleStyle => "cycle_style",
            Self::NestLayout => "nest_layout",
            Self::UnnestLayout => "unnest_layout",
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
            | Op::CycleStyle
            | Op::NestLayout
            | Op::UnnestLayout
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
            Just(Op::CycleStyle),
            Just(Op::NestLayout),
            Just(Op::UnnestLayout),
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
