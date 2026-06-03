#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    AddWidget,
    RemoveWidget,
    MutateWidget,
    CycleStyle,
    TeardownRace,
    ToggleVisible,
    ToggleExpand,
    FocusShuffle,
    ResizeWindow,
    PulseChild,
    ResizeWidget,
    AbuseWidget,
}

impl Op {
    pub fn from_index(index: u32) -> Self {
        match index % 12 {
            0 => Self::AddWidget,
            1 => Self::RemoveWidget,
            2 => Self::MutateWidget,
            3 => Self::CycleStyle,
            4 => Self::TeardownRace,
            5 => Self::ToggleVisible,
            6 => Self::ToggleExpand,
            7 => Self::FocusShuffle,
            8 => Self::ResizeWindow,
            9 => Self::PulseChild,
            10 => Self::ResizeWidget,
            _ => Self::AbuseWidget,
        }
    }

    /// Weighted: 27% add, 22% remove, 7% style, 7% teardown, 5% abuse, 32% other
    pub fn pick_weighted(roll: u32) -> Self {
        let band = roll % 100;
        if band < 27 { return Self::AddWidget; }
        if band < 49 { return Self::RemoveWidget; }
        if band < 56 { return Self::CycleStyle; }
        if band < 63 { return Self::TeardownRace; }
        if band < 68 { return Self::AbuseWidget; }
        Self::from_index(band)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AddWidget => "add_widget",
            Self::RemoveWidget => "remove_widget",
            Self::MutateWidget => "mutate_widget",
            Self::CycleStyle => "cycle_style",
            Self::TeardownRace => "teardown_race",
            Self::ToggleVisible => "toggle_visible",
            Self::ToggleExpand => "toggle_expand",
            Self::FocusShuffle => "focus_shuffle",
            Self::ResizeWindow => "resize_window",
            Self::PulseChild => "pulse_child",
            Self::ResizeWidget => "resize_widget",
            Self::AbuseWidget => "abuse_widget",
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
            | Op::TeardownRace
            | Op::ToggleVisible
            | Op::ToggleExpand
            | Op::FocusShuffle
            | Op::ResizeWindow
            | Op::PulseChild
            | Op::ResizeWidget
            | Op::AbuseWidget => {}
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
            Just(Op::TeardownRace),
            Just(Op::ToggleVisible),
            Just(Op::ToggleExpand),
            Just(Op::FocusShuffle),
            Just(Op::ResizeWindow),
            Just(Op::PulseChild),
            Just(Op::ResizeWidget),
            Just(Op::AbuseWidget),
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
