fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    return nwg_main();
    println!("skipped (requires Windows)");
    Ok(())
}

#[cfg(windows)]
fn nwg_main() -> Result<(), Box<dyn std::error::Error>> {
    use rustxwidgets::prelude::*;
    use std::os::raw::c_void;

    fn check_widget_nonzero(child: &impl AsRef<*mut c_void>, name: &str) {
        unsafe {
            let hwnd = *child.as_ref();
            let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
            winapi::um::winuser::GetWindowRect(hwnd as _, &mut rect);
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            assert!(w > 0 && h > 0,
                "{} has zero area: {}x{} at ({},{})", name, w, h, rect.left, rect.top);
            println!("  {}: rect=({},{},{},{}) size={}x{}", name, rect.left, rect.top, rect.right, rect.bottom, w, h);
        }
    }

    let app = App::init()?;
    let win = app.create_window()?;
    win.set_title("Layout Position Test");
    win.set_default_size(800, 600);

    let label1 = app.create_label("Widget 1")?;
    let label2 = app.create_label("Widget 2")?;
    let label3 = app.create_label("Widget 3")?;
    let btn = app.create_button("Click")?;

    // Test flat layout
    let mut vbox = app.create_box(Orientation::Vertical, 4)?;
    vbox.append(&label1);
    vbox.append(&label2);
    vbox.append(&btn);
    vbox.append(&label3);

    win.set_child_box(&vbox);
    win.present();

    // Check that every widget has non-zero area after layout
    check_widget_nonzero(&label1, "label1");
    check_widget_nonzero(&label2, "label2");
    check_widget_nonzero(&btn, "btn");
    check_widget_nonzero(&label3, "label3");

    // Verify vertical ordering
    unsafe fn get_child_top(child: &impl AsRef<*mut c_void>) -> i32 {
        let hwnd = *child.as_ref();
        let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
        winapi::um::winuser::GetWindowRect(hwnd as _, &mut rect);
        rect.top
    }
    unsafe {
        let y1 = get_child_top(&label1);
        let y2 = get_child_top(&label2);
        let yb = get_child_top(&btn);
        let y3 = get_child_top(&label3);
        assert!(y1 < y2, "label1 (y={}) should be above label2 (y={})", y1, y2);
        assert!(y2 < yb, "label2 (y={}) should be above btn (y={})", y2, yb);
        assert!(yb < y3, "btn (y={}) should be above label3 (y={})", yb, y3);
    }

    // Test nested box — BoxWidget has Frame HWND; inner box Frame is reparented to outer box Frame
    {
        let mut sub_box = app.create_box(Orientation::Vertical, 4)?;
        sub_box.append(&app.create_label("Sub 1")?);
        sub_box.append(&app.create_label("Sub 2")?);
        let mut outer_box = app.create_box(Orientation::Vertical, 4)?;
        outer_box.append(&sub_box);
        outer_box.append(&label1);
        // No crash = nested Frame hierarchy works
    }

    println!("All layout position checks passed!");
    Ok(())
}
