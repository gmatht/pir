pub use crate::core::{App, DrawContext, Error, HandlerId, Widget};
#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
pub use crate::common::{Window, WidgetBox, Label, Entry, Canvas, Menu, MenuBar, SimpleAction, Dialog, Orientation};
#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
pub use crate::backends_gtk_adapter::{Button, Grid, DropDown, CheckButton, RadioButton, TextView, Overlay, Spreadsheet, ScrolledWindow};
#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
pub use crate::common::{Window, WidgetBox, Label, Entry, Canvas, Menu, MenuBar, SimpleAction, Dialog, Orientation};
#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
pub use crate::backends_gtk_adapter::{Button, Grid, DropDown, CheckButton, RadioButton, TextView, Overlay, Spreadsheet, ScrolledWindow, TabView};
#[cfg(all(windows, not(feature = "zork")))]
pub use crate::common::{Window, WidgetBox, Entry, Canvas, Menu, MenuBar, SimpleAction, Dialog, Orientation};
#[cfg(all(windows, not(feature = "zork")))]
pub use crate::backends_nwg_adapter::{Button, Grid, DropDown, CheckButton, RadioButton, TextView, Label, Appendable, TabView};
#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
pub use crate::common::{Window, WidgetBox, Label, Entry, Canvas, Menu, MenuBar, SimpleAction, Dialog, Orientation};
#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
pub use crate::backends_wasm_adapter::{Button, Grid, DropDown, CheckButton, RadioButton, TextView, Overlay, ScrolledWindow};
#[cfg(all(target_os = "android", not(feature = "zork")))]
pub use crate::backends_android_adapter::{Window, Button, Label, Grid, DropDown, CheckButton, RadioButton, Dialog, TextView};

#[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
pub use crate::backends_pancurses_adapter::{Window, Button, Label, BoxWidget, Grid, Entry, Menu, MenuBar, SimpleAction, Dialog, DropDown, CheckButton, RadioButton, TextView, Orientation, Spreadsheet, TabView};

#[cfg(all(feature = "zork", not(feature = "pancurses")))]
pub use crate::backends_zork_adapter::{Window, Button, Label, BoxWidget, Grid, Entry, Menu, MenuBar, SimpleAction, Dialog, DropDown, CheckButton, RadioButton, TextView, Orientation};
