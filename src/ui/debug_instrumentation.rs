use crate::grid::CellAddr;

pub fn trace_setcell_construction(addr: &CellAddr, ui_main_cols: usize, workbook_main_cols: usize) {
    #[cfg(debug_assertions)]
    {
        crate::debug_log::log(&format!(
            "DEBUG SetCell constructed: addr={:?} ui_main_cols={} workbook_main_cols={}",
            addr, ui_main_cols, workbook_main_cols
        ));
    }
}
