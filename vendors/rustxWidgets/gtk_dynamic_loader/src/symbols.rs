use crate::error::Error;
use crate::loader::RawLib as Library;
use std::ffi::c_void;

// Minimal subset of function pointer types we need
pub type GMainLoopNew = unsafe extern "C" fn(context: *mut c_void, is_running: i32) -> *mut c_void;
pub type GMainLoopRun = unsafe extern "C" fn(loop_: *mut c_void);
pub type GMainLoopQuit = unsafe extern "C" fn(loop_: *mut c_void);

pub type GObjectRef = unsafe extern "C" fn(obj: *mut c_void) -> *mut c_void;
pub type GObjectUnref = unsafe extern "C" fn(obj: *mut c_void);
pub type GObjectRefSink = unsafe extern "C" fn(obj: *mut c_void) -> *mut c_void;

pub type GSignalConnectData = unsafe extern "C" fn(instance: *mut c_void, detailed_signal: *const i8, c_handler: *mut c_void, data: *mut c_void, destroy_data: Option<unsafe extern "C" fn(data: *mut c_void, closure: *mut c_void)>, connect_flags: u32) -> u64;
pub type GSignalConnect = unsafe extern "C" fn(instance: *mut c_void, detailed_signal: *const i8, c_handler: *mut c_void, data: *mut c_void) -> u64;
pub type GSignalEmitByName = unsafe extern "C" fn(instance: *mut c_void, detailed_signal: *const i8) -> u64;

pub type GtkWindowNew = unsafe extern "C" fn(window_type: i32) -> *mut c_void;
pub type GtkWindowSetTitle = unsafe extern "C" fn(window: *mut c_void, title: *const i8);
pub type GtkButtonNewWithLabel = unsafe extern "C" fn(label: *const i8) -> *mut c_void;
pub type GtkBinGetChild = unsafe extern "C" fn(bin: *mut c_void) -> *mut c_void;
pub type GtkLabelNew = unsafe extern "C" fn(str: *const i8) -> *mut c_void;
pub type GtkLabelSetText = unsafe extern "C" fn(label: *mut c_void, str: *const i8);
pub type GtkLabelGetText = unsafe extern "C" fn(label: *mut c_void) -> *const i8;
pub type GtkBoxNew = unsafe extern "C" fn(orientation: i32, spacing: i32) -> *mut c_void;

pub type GtkBoxAppend = unsafe extern "C" fn(box_: *mut c_void, child: *mut c_void);
pub type GtkBoxPackStart = unsafe extern "C" fn(box_: *mut c_void, child: *mut c_void, expand: i32, fill: i32, padding: u32);
pub type GtkContainerAdd = unsafe extern "C" fn(container: *mut c_void, widget: *mut c_void);
pub type GtkContainerRemove = unsafe extern "C" fn(container: *mut c_void, widget: *mut c_void);
pub type GtkWindowSetChild = unsafe extern "C" fn(window: *mut c_void, child: *mut c_void);
pub type GtkWidgetShowAll = unsafe extern "C" fn(widget: *mut c_void);
pub type GtkWindowPresent = unsafe extern "C" fn(window: *mut c_void);
pub type GtkInit = unsafe extern "C" fn(argc: *mut libc::c_int, argv: *mut *mut *mut libc::c_char);
pub type GtkLabelSetMarkup = unsafe extern "C" fn(label: *mut c_void, markup: *const i8);
pub type GtkWidgetSetVisible = unsafe extern "C" fn(widget: *mut c_void, visible: i32);
pub type GtkWidgetGrabFocus = unsafe extern "C" fn(widget: *mut c_void);
pub type GtkWidgetHasFocus = unsafe extern "C" fn(widget: *mut c_void) -> i32;
pub type GtkWidgetIsFocus = unsafe extern "C" fn(widget: *mut c_void) -> i32;
pub type GtkWindowGetFocus = unsafe extern "C" fn(window: *mut c_void) -> *mut c_void;
pub type GtkWidgetGetStyleContext = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;
pub type GtkStyleContextAddClass = unsafe extern "C" fn(context: *mut c_void, class_name: *const i8);
pub type GtkStyleContextRemoveClass = unsafe extern "C" fn(context: *mut c_void, class_name: *const i8);
pub type GtkCssProviderNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkCssProviderLoadFromData = unsafe extern "C" fn(provider: *mut c_void, data: *const i8, length: isize, error: *mut *mut c_void) -> i32;
pub type GtkStyleContextAddProvider = unsafe extern "C" fn(context: *mut c_void, provider: *mut c_void, priority: u32);
// GApplication / GMenu / Actions (gio)
pub type GtkApplicationNew = unsafe extern "C" fn(application_id: *const i8, flags: u32) -> *mut c_void;
pub type GApplicationRun = unsafe extern "C" fn(application: *mut c_void, argc: i32, argv: *mut *mut i8) -> i32;
pub type GApplicationRegister = unsafe extern "C" fn(application: *mut c_void, cancellable: *mut c_void, error: *mut *mut c_void) -> i32;
pub type GSimpleActionNew = unsafe extern "C" fn(name: *const i8, parameter_type: *mut c_void) -> *mut c_void;
pub type GActionMapAddAction = unsafe extern "C" fn(map: *mut c_void, action: *mut c_void);
pub type GActionGroupActivateAction = unsafe extern "C" fn(group: *mut c_void, action_name: *const i8, parameter: *mut c_void);
pub type GActionMapLookupAction = unsafe extern "C" fn(map: *mut c_void, action_name: *const i8) -> *mut c_void;
pub type GActionActivate = unsafe extern "C" fn(action: *mut c_void, parameter: *mut c_void);
pub type GMenuNew = unsafe extern "C" fn() -> *mut c_void;
pub type GMenuAppend = unsafe extern "C" fn(menu: *mut c_void, label: *const i8, detailed_action: *const i8);
pub type GApplicationSetAppMenu = unsafe extern "C" fn(application: *mut c_void, menu: *mut c_void);
pub type GApplicationSetMenubar = unsafe extern "C" fn(application: *mut c_void, menu: *mut c_void);
pub type GMenuAppendSubmenu = unsafe extern "C" fn(menu: *mut c_void, label: *const i8, submenu: *mut c_void);
pub type GtkPopoverMenuBarNewFromModel = unsafe extern "C" fn(model: *mut c_void) -> *mut c_void;
// GTK3 menu bar
pub type GtkMenuBarNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkMenuNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkMenuItemNewWithLabel = unsafe extern "C" fn(label: *const i8) -> *mut c_void;
pub type GtkMenuItemNewWithMnemonic = unsafe extern "C" fn(label: *const i8) -> *mut c_void;
pub type GtkMenuShellAppend = unsafe extern "C" fn(shell: *mut c_void, child: *mut c_void);
pub type GtkMenuItemSetSubmenu = unsafe extern "C" fn(item: *mut c_void, submenu: *mut c_void);
pub type GtkWindowSetApplication = unsafe extern "C" fn(window: *mut c_void, application: *mut c_void);
// Widget action group resolution (needed for GtkPopoverMenuBar to find actions)
pub type GtkWidgetInsertActionGroup = unsafe extern "C" fn(widget: *mut c_void, name: *const i8, group: *mut c_void);
// GtkActionable (for setting action names on GTK3 menu items)
pub type GtkActionableSetDetailedActionName = unsafe extern "C" fn(actionable: *mut c_void, detailed_action_name: *const i8);
// GMenuModel iteration (for GTK3 fallback)
pub type GMenuModelGetNItems = unsafe extern "C" fn(model: *mut c_void) -> i32;
pub type GMenuModelGetItemAttributeValue = unsafe extern "C" fn(model: *mut c_void, item_index: i32, attribute: *const i8, expected_type: *const c_void) -> *mut c_void;
pub type GMenuModelGetItemLink = unsafe extern "C" fn(model: *mut c_void, item_index: i32, link: *const i8) -> *mut c_void;
pub type GVariantGetString = unsafe extern "C" fn(value: *mut c_void, length: *mut usize) -> *const i8;
pub type GVariantUnref = unsafe extern "C" fn(value: *mut c_void);

// File chooser / native dialog
pub type GtkFileChooserNativeNew = unsafe extern "C" fn(title: *const i8, parent: *mut c_void, action: i32, accept_label: *const i8, cancel_label: *const i8) -> *mut c_void;
pub type GtkNativeDialogRun = unsafe extern "C" fn(native: *mut c_void) -> i32;
pub type GtkFileChooserGetFilename = unsafe extern "C" fn(chooser: *mut c_void) -> *const i8;
pub type GtkWidgetDestroy = unsafe extern "C" fn(widget: *mut c_void);
pub type GtkWindowClose = unsafe extern "C" fn(window: *mut c_void);
pub type GFree = unsafe extern "C" fn(ptr: *mut c_void);
// gdk event helpers
pub type GdkEventGetKeyval = unsafe extern "C" fn(event: *mut c_void, keyval: *mut u32) -> i32;
pub type GdkEventGetState = unsafe extern "C" fn(event: *mut c_void, state: *mut u32);
pub type GdkKeyvalFromName = unsafe extern "C" fn(name: *const i8) -> u32;
pub type GdkDisplayGetDefault = unsafe extern "C" fn() -> *mut c_void;
pub type GdkScreenGetDefault = unsafe extern "C" fn() -> *mut c_void;
pub type GtkStyleContextAddProviderForDisplay = unsafe extern "C" fn(display: *mut c_void, provider: *mut c_void, priority: u32);
pub type GtkStyleContextAddProviderForScreen = unsafe extern "C" fn(screen: *mut c_void, provider: *mut c_void, priority: u32);
pub type GtkLabelSetXalign = unsafe extern "C" fn(label: *mut c_void, xalign: f32);
pub type GtkEventControllerKeyNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkEventControllerFocusNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkWidgetAddController = unsafe extern "C" fn(widget: *mut c_void, controller: *mut c_void);
pub type GtkEventControllerSetPropagationPhase = unsafe extern "C" fn(controller: *mut c_void, phase: u32);
pub type GtkScrolledWindowGetVadjustment = unsafe extern "C" fn(sw: *mut c_void) -> *mut c_void;
pub type GtkScrolledWindowGetHadjustment = unsafe extern "C" fn(sw: *mut c_void) -> *mut c_void;
pub type GtkAdjustmentGetValue = unsafe extern "C" fn(adj: *mut c_void) -> f64;
pub type GtkAdjustmentGetUpper = unsafe extern "C" fn(adj: *mut c_void) -> f64;
pub type GtkAdjustmentGetPageSize = unsafe extern "C" fn(adj: *mut c_void) -> f64;
pub type GtkGestureClickNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkWidgetSetCanTarget = unsafe extern "C" fn(widget: *mut c_void, can_target: i32);
pub type GtkWidgetSetHalign = unsafe extern "C" fn(widget: *mut c_void, align: i32);
pub type GtkWidgetSetValign = unsafe extern "C" fn(widget: *mut c_void, align: i32);
pub type GtkWidgetGetAllocatedWidth = unsafe extern "C" fn(widget: *mut c_void) -> i32;
pub type GtkWidgetGetAllocatedHeight = unsafe extern "C" fn(widget: *mut c_void) -> i32;
pub type GtkWidgetGetMapped = unsafe extern "C" fn(widget: *mut c_void) -> i32;
pub type GtkWidgetGetDisplay = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;

// GtkPopoverMenuBarItem — get the popover from a bar item
pub type GtkPopoverMenuBarItemGetPopover = unsafe extern "C" fn(item: *mut c_void) -> *mut c_void;
// GtkPopover — get/set child widget
pub type GtkPopoverGetChild = unsafe extern "C" fn(popover: *mut c_void) -> *mut c_void;
pub type GtkPopoverNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkPopoverSetChild = unsafe extern "C" fn(popover: *mut c_void, child: *mut c_void);

// GtkMenuButton — for manual menu bar construction
pub type GtkMenuButtonNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkMenuButtonSetPopover = unsafe extern "C" fn(button: *mut c_void, popover: *mut c_void);
pub type GtkMenuButtonSetLabel = unsafe extern "C" fn(button: *mut c_void, label: *const i8);
pub type GtkMenuButtonGetPopover = unsafe extern "C" fn(button: *mut c_void) -> *mut c_void;
pub type GtkMenuButtonSetHasFrame = unsafe extern "C" fn(button: *mut c_void, has_frame: i32);
pub type GtkMenuButtonSetPrimary = unsafe extern "C" fn(button: *mut c_void, primary: i32);
pub type GtkButtonSetHasFrame = unsafe extern "C" fn(button: *mut c_void, has_frame: i32);
pub type GtkWidgetAddCssClass = unsafe extern "C" fn(widget: *mut c_void, css_class: *const i8);



// GtkPopoverMenu — new from model (submenu-level)
pub type GtkPopoverMenuNewFromModel = unsafe extern "C" fn(model: *mut c_void) -> *mut c_void;

// Display sync — flushes pending display requests and waits for server round-trip
pub type GdkDisplaySync = unsafe extern "C" fn(display: *mut c_void);

// Dialog
pub type GtkDialogNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkDialogAddButton = unsafe extern "C" fn(dialog: *mut c_void, button_text: *const i8, response_id: i32) -> *mut c_void;
pub type GtkDialogGetContentArea = unsafe extern "C" fn(dialog: *mut c_void) -> *mut c_void;
pub type GtkDialogRun = unsafe extern "C" fn(dialog: *mut c_void) -> i32;
// Dropdown - GTK3 ComboBoxText
pub type GtkComboBoxTextNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkComboBoxTextAppendText = unsafe extern "C" fn(combo: *mut c_void, text: *const i8);
pub type GtkComboBoxTextGetActiveText = unsafe extern "C" fn(combo: *mut c_void) -> *const i8;
pub type GtkComboBoxSetActive = unsafe extern "C" fn(combo: *mut c_void, index_: i32);
pub type GtkComboBoxGetActive = unsafe extern "C" fn(combo: *mut c_void) -> i32;

// Dropdown - GTK4 DropDown
pub type GtkDropDownNew = unsafe extern "C" fn(model: *mut c_void, expression: *mut c_void) -> *mut c_void;
pub type GtkDropDownSetSelected = unsafe extern "C" fn(dropdown: *mut c_void, selected: u32);
pub type GtkDropDownGetSelected = unsafe extern "C" fn(dropdown: *mut c_void) -> u32;
pub type GtkStringListNew = unsafe extern "C" fn(strings: *const *const i8) -> *mut c_void;

// Checkbox / CheckButton
pub type GtkCheckButtonNewWithLabel = unsafe extern "C" fn(label: *const i8) -> *mut c_void;
pub type GtkCheckButtonGetActive = unsafe extern "C" fn(check_button: *mut c_void) -> i32;
pub type GtkCheckButtonSetActive = unsafe extern "C" fn(check_button: *mut c_void, is_active: i32);
pub type GtkCheckButtonSetGroup = unsafe extern "C" fn(check_button: *mut c_void, group: *mut c_void);
pub type GtkToggleButtonGetActive = unsafe extern "C" fn(toggle_button: *mut c_void) -> i32;
pub type GtkToggleButtonSetActive = unsafe extern "C" fn(toggle_button: *mut c_void, is_active: i32);

// RadioButton
pub type GtkRadioButtonNewWithLabel = unsafe extern "C" fn(group: *mut c_void, label: *const i8) -> *mut c_void;

// TextView / TextArea
pub type GtkTextViewNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkTextBufferNew = unsafe extern "C" fn(table: *mut c_void) -> *mut c_void;
pub type GtkTextViewGetBuffer = unsafe extern "C" fn(text_view: *mut c_void) -> *mut c_void;
pub type GTimeoutAdd = unsafe extern "C" fn(interval_ms: u32, func: Option<unsafe extern "C" fn(*mut c_void) -> i32>, data: *mut c_void) -> u32;
pub type GtkTextBufferSetText = unsafe extern "C" fn(buffer: *mut c_void, text: *const i8, len: i32);
pub type GtkTextBufferGetText = unsafe extern "C" fn(buffer: *mut c_void, start: *mut c_void, end: *mut c_void, include_hidden_chars: i32) -> *mut c_void;
pub type GtkTextBufferGetStartIter = unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void);
pub type GtkTextBufferGetEndIter = unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void);
pub type GtkTextIterCopy = unsafe extern "C" fn(iter: *mut c_void) -> *mut c_void;
pub type GtkTextIterFree = unsafe extern "C" fn(iter: *mut c_void);
pub type GtkTextViewSetWrapMode = unsafe extern "C" fn(text_view: *mut c_void, wrap_mode: i32);
/// Append `text` at `iter` and return the iter just past the inserted text.
pub type GtkTextBufferInsert = unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void, text: *const i8, len: i32) -> *mut c_void;
/// Copy the current selection to the clipboard (GTK's own Ctrl-C path:
/// `gtk_text_buffer_copy_clipboard`). Takes effect only when a selection
/// exists; otherwise a no-op — never an error.
pub type GtkTextBufferCopyClipboard = unsafe extern "C" fn(buffer: *mut c_void, clipboard: *mut c_void);
/// Get the default clipboard for `widget`'s display (the CLIPBOARD selection
/// Ctrl-C/Ctrl-V use, not PRIMARY).
pub type GtkWidgetGetClipboard = unsafe extern "C" fn(widget: *mut c_void, selection: i32) -> *mut c_void;
/// Append Pango `markup` at `iter` (parses e.g. `<b>`, `<i>`, `<span>`,
/// `<tt>`); returns the iter just past the inserted text. GTK validates the
/// markup — on error it inserts nothing rather than raw tags.
pub type GtkTextBufferInsertMarkup = unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void, markup: *const i8, len: i32);
pub type GtkTextViewSetEditable = unsafe extern "C" fn(text_view: *mut c_void, editable: i32);
pub type GtkTextViewGetEditable = unsafe extern "C" fn(text_view: *mut c_void) -> i32;
pub type GtkWidgetGetCanFocus = unsafe extern "C" fn(widget: *mut c_void) -> i32;
/// Map widget (window) coordinates to buffer coordinates in a TextView
/// (`gtk_text_view_window_to_buffer_coords`). `win` is one of the
/// `GtkTextWindowType` values (GTK_TEXT_WINDOW_TEXT = 2, GTK3 only).
pub type GtkTextViewWindowToBufferCoords =
    unsafe extern "C" fn(text_view: *mut c_void, win: i32, window_x: i32, window_y: i32, buffer_x: *mut i32, buffer_y: *mut i32);
/// Get the iter at a buffer location (`gtk_text_view_get_iter_at_location`).
/// Returns 1 when an iter was set (always true for a non-empty buffer).
pub type GtkTextViewGetIterAtLocation =
    unsafe extern "C" fn(text_view: *mut c_void, iter: *mut c_void, x: i32, y: i32) -> i32;
/// Line number (0-based) of `iter` (`gtk_text_iter_get_line`).
pub type GtkTextIterGetLine = unsafe extern "C" fn(iter: *mut c_void) -> i32;
/// Char offset of `iter` within its line (`gtk_text_iter_get_line_offset`).
pub type GtkTextIterGetLineOffset = unsafe extern "C" fn(iter: *mut c_void) -> i32;
/// Whether the buffer holds a selection (`gtk_text_buffer_get_has_selection`).
/// The click handler consults this first: a release that ends a selection is
/// a copy gesture, never a link activation.
pub type GtkTextBufferGetHasSelection = unsafe extern "C" fn(buffer: *mut c_void) -> i32;
/// Which mouse button a button event is for (`gdk_event_get_button`,
/// GDK3/GDK-X11 only — absent on GTK4, where clicks arrive via gestures).
pub type GdkEventGetButton = unsafe extern "C" fn(event: *mut c_void, button: *mut u32) -> i32;

// GtkWidget helper for visibility/event handling
pub type GtkWidgetSetCanFocus = unsafe extern "C" fn(widget: *mut c_void, can_focus: i32);
pub type GtkWidgetSetHexpand = unsafe extern "C" fn(widget: *mut c_void, expand: i32);
pub type GtkWidgetSetVexpand = unsafe extern "C" fn(widget: *mut c_void, expand: i32);
pub type GtkWidgetGetHexpand = unsafe extern "C" fn(widget: *mut c_void) -> i32;
pub type GtkWidgetGetVexpand = unsafe extern "C" fn(widget: *mut c_void) -> i32;

// GtkEditable (GTK4 replacement for gtk_entry_get_text/set_text)
pub type GtkEditableGetText = unsafe extern "C" fn(editable: *mut c_void) -> *const i8;
pub type GtkEditableSetText = unsafe extern "C" fn(editable: *mut c_void, text: *const i8);
pub type GtkEditableSetPosition = unsafe extern "C" fn(editable: *mut c_void, position: i32);
pub type GtkEditableSelectRegion = unsafe extern "C" fn(editable: *mut c_void, start: i32, end: i32);

// GtkWidget parent handling
pub type GtkWidgetUnparent = unsafe extern "C" fn(widget: *mut c_void);
pub type GtkWidgetGetParent = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;

// GtkWidget tree traversal
pub type GtkWidgetGetFirstChild = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;
pub type GtkWidgetGetNextSibling = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;

// GtkWidget activate (triggers the default action, e.g. opening a submenu popover)
pub type GtkWidgetActivate = unsafe extern "C" fn(widget: *mut c_void);

// GtkWidget get_visible — checks the widget's visible flag
pub type GtkWidgetGetVisible = unsafe extern "C" fn(widget: *mut c_void) -> i32;

// GtkWidget activate_action — activates a named action on a widget
pub type GtkWidgetActivateAction = unsafe extern "C" fn(widget: *mut c_void, detailed_name: *const i8, parameter: *mut c_void);

// GtkMenuButton (GTK4 submenu opener)
pub type GtkMenuButtonGetLabel = unsafe extern "C" fn(button: *mut c_void) -> *const i8;
pub type GtkMenuButtonSetActive = unsafe extern "C" fn(button: *mut c_void, active: i32);

// GtkWindow default size
pub type GtkWindowSetDefaultSize = unsafe extern "C" fn(window: *mut c_void, width: i32, height: i32);

// Grid/Entry related
pub type GtkGridNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkGridAttach = unsafe extern "C" fn(grid: *mut c_void, child: *mut c_void, left: i32, top: i32, width: i32, height: i32);
pub type GtkEntryNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkEntrySetText = unsafe extern "C" fn(entry: *mut c_void, text: *const i8);
pub type GtkEntryGetText = unsafe extern "C" fn(entry: *mut c_void) -> *const i8;
pub type GtkEntrySetWidthChars = unsafe extern "C" fn(entry: *mut c_void, n_chars: i32);
pub type GtkEntrySetPlaceholderText = unsafe extern "C" fn(entry: *mut c_void, text: *const i8);
pub type GtkWidgetSetSizeRequest = unsafe extern "C" fn(widget: *mut c_void, width: i32, height: i32);
pub type GtkWidgetQueueDrawArea = unsafe extern "C" fn(widget: *mut c_void, x: i32, y: i32, width: i32, height: i32);
pub type GtkEntrySetHasFrame = unsafe extern "C" fn(entry: *mut c_void, has_frame: i32);
pub type GtkEntrySetEditable = unsafe extern "C" fn(entry: *mut c_void, editable: i32);
pub type GtkDrawingAreaNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkOverlayNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkOverlayAddOverlay = unsafe extern "C" fn(overlay: *mut c_void, widget: *mut c_void);
pub type GtkOverlaySetOverlayPassThrough = unsafe extern "C" fn(overlay: *mut c_void, widget: *mut c_void, pass_through: i32);
pub type GtkOverlaySetChild = unsafe extern "C" fn(overlay: *mut c_void, child: *mut c_void);
pub type GtkWidgetSetMarginStart = unsafe extern "C" fn(widget: *mut c_void, margin: i32);
pub type GtkWidgetSetMarginTop = unsafe extern "C" fn(widget: *mut c_void, margin: i32);
pub type GtkWidgetAddEvents = unsafe extern "C" fn(widget: *mut c_void, events: i32);

// ScrolledWindow
pub type GtkScrolledWindowNew = unsafe extern "C" fn(hadjustment: *mut c_void, vadjustment: *mut c_void) -> *mut c_void;
pub type GtkScrolledWindowSetPolicy = unsafe extern "C" fn(scrolled: *mut c_void, h_policy: u32, v_policy: u32);
pub type GtkScrolledWindowSetChild = unsafe extern "C" fn(scrolled: *mut c_void, child: *mut c_void);

// DrawingArea canvas
pub type GtkDrawingAreaSetDrawFunc = unsafe extern "C" fn(area: *mut c_void, draw_func: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, *mut c_void)>, user_data: *mut c_void, destroy: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>);
pub type GtkDrawingAreaSetContentWidth = unsafe extern "C" fn(area: *mut c_void, width: i32);
pub type GtkDrawingAreaSetContentHeight = unsafe extern "C" fn(area: *mut c_void, height: i32);

// Frame clock (GTK4's display-synchronized animation clock)
pub type GtkWidgetGetFrameClock = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;
pub type GdkFrameClockRequestPhase = unsafe extern "C" fn(clock: *mut c_void, phase: u32);
pub type GdkFrameClockBeginUpdating = unsafe extern "C" fn(clock: *mut c_void);
pub type GdkFrameClockEndUpdating = unsafe extern "C" fn(clock: *mut c_void);

// Cairo additions
pub type CairoTextExtents = unsafe extern "C" fn(cr: *mut c_void, utf8: *const i8, extents: *mut c_void);
pub type CairoSave = unsafe extern "C" fn(cr: *mut c_void);
pub type CairoRestore = unsafe extern "C" fn(cr: *mut c_void);
pub type CairoClip = unsafe extern "C" fn(cr: *mut c_void);

// Cairo line drawing
pub type CairoLineTo = unsafe extern "C" fn(cr: *mut c_void, x: f64, y: f64);
pub type CairoPaint = unsafe extern "C" fn(cr: *mut c_void);

// GdkSurface — for direct rendering bypassing frame clock
pub type GtkNativeGetSurface = unsafe extern "C" fn(native: *mut c_void) -> *mut c_void;
pub type GdkSurfaceCreateCairoContext = unsafe extern "C" fn(surface: *mut c_void) -> *mut c_void;
pub type GdkSurfaceGetWidth = unsafe extern "C" fn(surface: *mut c_void) -> i32;

// GdkSurface begin/end draw frame (GTK 4.0-4.14, deprecated in 4.14+)
pub type GdkSurfaceBeginDrawFrame = unsafe extern "C" fn(surface: *mut c_void, region: *mut c_void) -> *mut c_void;
pub type GdkSurfaceEndDrawFrame = unsafe extern "C" fn(surface: *mut c_void, context: *mut c_void);
pub type GdkDrawContextGetCairoContext = unsafe extern "C" fn(context: *mut c_void) -> *mut c_void;
pub type GdkSurfaceGetHeight = unsafe extern "C" fn(surface: *mut c_void) -> i32;

#[repr(C)]
pub struct CairoTextExtentsT {
    pub x_bearing: f64,
    pub y_bearing: f64,
    pub width: f64,
    pub height: f64,
    pub x_advance: f64,
    pub y_advance: f64,
}

pub struct Symbols {
    // glib
    pub g_main_loop_new: Option<unsafe extern "C" fn(context: *mut c_void, is_running: i32) -> *mut c_void>,
    pub g_main_loop_run: Option<unsafe extern "C" fn(loop_: *mut c_void)>,
    pub g_main_loop_quit: Option<unsafe extern "C" fn(loop_: *mut c_void)>,

    // gobject
    pub g_object_ref: Option<unsafe extern "C" fn(obj: *mut c_void) -> *mut c_void>,
    pub g_object_unref: Option<unsafe extern "C" fn(obj: *mut c_void)>,
    pub g_object_ref_sink: Option<unsafe extern "C" fn(obj: *mut c_void) -> *mut c_void>,
    pub g_signal_connect_data: Option<unsafe extern "C" fn(instance: *mut c_void, detailed_signal: *const i8, c_handler: *mut c_void, data: *mut c_void, destroy_data: Option<unsafe extern "C" fn(data: *mut c_void, closure: *mut c_void)>, connect_flags: u32) -> u64>,
    pub g_signal_connect: Option<unsafe extern "C" fn(instance: *mut c_void, detailed_signal: *const i8, c_handler: *mut c_void, data: *mut c_void) -> u64>,

    // gtk
    pub gtk_window_new: Option<unsafe extern "C" fn(window_type: i32) -> *mut c_void>,
    pub gtk_window_set_title: Option<unsafe extern "C" fn(window: *mut c_void, title: *const i8)>,
    pub gtk_button_new_with_label: Option<unsafe extern "C" fn(label: *const i8) -> *mut c_void>,
    /// `gtk_bin_get_child` — lets a container read its single child, so a
    /// GtkButton's internal GtkLabel can be reached without new widget types.
    pub gtk_bin_get_child: Option<unsafe extern "C" fn(bin: *mut c_void) -> *mut c_void>,
    pub gtk_label_new: Option<unsafe extern "C" fn(str: *const i8) -> *mut c_void>,
    pub gtk_label_set_text: Option<unsafe extern "C" fn(label: *mut c_void, str: *const i8)>,
    pub gtk_label_get_text: Option<unsafe extern "C" fn(label: *mut c_void) -> *const i8>,
    pub gtk_box_new: Option<unsafe extern "C" fn(orientation: i32, spacing: i32) -> *mut c_void>,
    pub gtk_box_append: Option<unsafe extern "C" fn(box_: *mut c_void, child: *mut c_void)>,
    pub gtk_box_pack_start: Option<unsafe extern "C" fn(box_: *mut c_void, child: *mut c_void, expand: i32, fill: i32, padding: u32)>,
    pub gtk_container_add: Option<unsafe extern "C" fn(container: *mut c_void, widget: *mut c_void)>,
    pub gtk_container_remove: Option<unsafe extern "C" fn(container: *mut c_void, widget: *mut c_void)>,
    pub gtk_window_set_child: Option<unsafe extern "C" fn(window: *mut c_void, child: *mut c_void)>,
    pub gtk_widget_show_all: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_show: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_hide: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_queue_resize: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_get_visible: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_window_present: Option<unsafe extern "C" fn(window: *mut c_void)>,
    pub gtk_window_set_application: Option<unsafe extern "C" fn(window: *mut c_void, application: *mut c_void)>,
    pub gtk_widget_insert_action_group: Option<unsafe extern "C" fn(widget: *mut c_void, name: *const i8, group: *mut c_void)>,
    pub gtk_actionable_set_detailed_action_name: Option<unsafe extern "C" fn(actionable: *mut c_void, detailed_action_name: *const i8)>,
    pub gtk_grid_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_grid_attach: Option<unsafe extern "C" fn(grid: *mut c_void, child: *mut c_void, left: i32, top: i32, width: i32, height: i32)>,
    pub gtk_entry_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_entry_set_text: Option<unsafe extern "C" fn(entry: *mut c_void, text: *const i8)>,
    pub gtk_entry_get_text: Option<unsafe extern "C" fn(entry: *mut c_void) -> *const i8>,
    pub gtk_entry_set_width_chars: Option<unsafe extern "C" fn(entry: *mut c_void, n_chars: i32)>,
    pub gtk_entry_set_placeholder_text: Option<unsafe extern "C" fn(entry: *mut c_void, text: *const i8)>,
    pub gtk_widget_set_size_request: Option<unsafe extern "C" fn(widget: *mut c_void, width: i32, height: i32)>,
    pub gtk_entry_set_has_frame: Option<unsafe extern "C" fn(entry: *mut c_void, has_frame: i32)>,
    pub gtk_entry_set_editable: Option<unsafe extern "C" fn(entry: *mut c_void, editable: i32)>,
    pub gtk_drawing_area_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_label_set_markup: Option<unsafe extern "C" fn(label: *mut c_void, markup: *const i8)>,
    pub gtk_widget_set_visible: Option<unsafe extern "C" fn(widget: *mut c_void, visible: i32)>,
    pub gtk_widget_grab_focus: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_has_focus: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_widget_is_focus: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_window_get_focus: Option<unsafe extern "C" fn(window: *mut c_void) -> *mut c_void>,
    pub gtk_widget_get_style_context: Option<unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void>,
    pub gtk_style_context_add_class: Option<unsafe extern "C" fn(context: *mut c_void, class_name: *const i8)>,
    pub gtk_style_context_remove_class: Option<unsafe extern "C" fn(context: *mut c_void, class_name: *const i8)>,
    pub gtk_css_provider_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_css_provider_load_from_data: Option<unsafe extern "C" fn(provider: *mut c_void, data: *const i8, length: isize, error: *mut *mut c_void) -> i32>,
    pub gtk_style_context_add_provider: Option<unsafe extern "C" fn(context: *mut c_void, provider: *mut c_void, priority: u32)>,
    pub gtk_overlay_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_overlay_add_overlay: Option<unsafe extern "C" fn(overlay: *mut c_void, widget: *mut c_void)>,
    pub gtk_overlay_set_overlay_pass_through: Option<unsafe extern "C" fn(overlay: *mut c_void, widget: *mut c_void, pass_through: i32)>,
    pub gtk_overlay_set_child: Option<unsafe extern "C" fn(overlay: *mut c_void, child: *mut c_void)>,
    pub gtk_widget_set_margin_start: Option<unsafe extern "C" fn(widget: *mut c_void, margin: i32)>,
    pub gtk_widget_set_margin_top: Option<unsafe extern "C" fn(widget: *mut c_void, margin: i32)>,
    pub gtk_widget_add_events: Option<unsafe extern "C" fn(widget: *mut c_void, events: i32)>,
    pub gtk_widget_activate: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_activate_action: Option<unsafe extern "C" fn(widget: *mut c_void, detailed_name: *const i8, parameter: *mut c_void)>,
    pub gtk_file_chooser_native_new: Option<unsafe extern "C" fn(title: *const i8, parent: *mut c_void, action: i32, accept_label: *const i8, cancel_label: *const i8) -> *mut c_void>,
    pub gtk_native_dialog_run: Option<unsafe extern "C" fn(native: *mut c_void) -> i32>,
    pub gtk_file_chooser_get_filename: Option<unsafe extern "C" fn(chooser: *mut c_void) -> *const i8>,
    pub gtk_widget_destroy: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_window_close: Option<unsafe extern "C" fn(window: *mut c_void)>,
    pub g_free: Option<unsafe extern "C" fn(ptr: *mut c_void)>,
    pub gdk_display_get_default: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gdk_screen_get_default: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_style_context_add_provider_for_display: Option<unsafe extern "C" fn(display: *mut c_void, provider: *mut c_void, priority: u32)>,
    pub gtk_style_context_add_provider_for_screen: Option<unsafe extern "C" fn(screen: *mut c_void, provider: *mut c_void, priority: u32)>,
    pub gdk_event_get_keyval: Option<unsafe extern "C" fn(event: *mut c_void, keyval: *mut u32) -> i32>,
    pub gdk_event_get_state: Option<unsafe extern "C" fn(event: *mut c_void, state: *mut u32)>,
    pub gdk_keyval_from_name: Option<unsafe extern "C" fn(name: *const i8) -> u32>,
    // application/menu/action
    pub gtk_application_new: Option<unsafe extern "C" fn(application_id: *const i8, flags: u32) -> *mut c_void>,
    pub g_application_run: Option<unsafe extern "C" fn(application: *mut c_void, argc: i32, argv: *mut *mut i8) -> i32>,
    pub g_application_register: Option<unsafe extern "C" fn(application: *mut c_void, cancellable: *mut c_void, error: *mut *mut c_void) -> i32>,
    pub g_simple_action_new: Option<unsafe extern "C" fn(name: *const i8, parameter_type: *mut c_void) -> *mut c_void>,
    pub g_action_map_add_action: Option<unsafe extern "C" fn(map: *mut c_void, action: *mut c_void)>,
    pub g_action_group_activate_action: Option<unsafe extern "C" fn(group: *mut c_void, action_name: *const i8, parameter: *mut c_void)>,
    pub g_action_map_lookup_action: Option<unsafe extern "C" fn(map: *mut c_void, action_name: *const i8) -> *mut c_void>,
    pub g_action_activate: Option<unsafe extern "C" fn(action: *mut c_void, parameter: *mut c_void)>,
    pub g_menu_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub g_menu_append: Option<unsafe extern "C" fn(menu: *mut c_void, label: *const i8, detailed_action: *const i8)>,
    pub g_application_set_app_menu: Option<unsafe extern "C" fn(application: *mut c_void, menu: *mut c_void)>,
    pub g_application_set_menubar: Option<unsafe extern "C" fn(application: *mut c_void, menu: *mut c_void)>,
    pub g_menu_append_submenu: Option<unsafe extern "C" fn(menu: *mut c_void, label: *const i8, submenu: *mut c_void)>,
    pub gtk_popover_menu_bar_new_from_model: Option<unsafe extern "C" fn(model: *mut c_void) -> *mut c_void>,
    pub gtk_menu_bar_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_menu_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_menu_item_new_with_label: Option<unsafe extern "C" fn(label: *const i8) -> *mut c_void>,
    pub gtk_menu_item_new_with_mnemonic: Option<unsafe extern "C" fn(label: *const i8) -> *mut c_void>,
    pub gtk_menu_shell_append: Option<unsafe extern "C" fn(shell: *mut c_void, child: *mut c_void)>,
    pub gtk_menu_item_set_submenu: Option<unsafe extern "C" fn(item: *mut c_void, submenu: *mut c_void)>,
    pub gtk_init: Option<unsafe extern "C" fn(argc: *mut libc::c_int, argv: *mut *mut *mut libc::c_char)>,
    pub g_signal_emit_by_name: Option<unsafe extern "C" fn(instance: *mut c_void, detailed_signal: *const i8) -> u64>,
    pub g_idle_add: Option<unsafe extern "C" fn(func: Option<unsafe extern "C" fn(*mut c_void) -> i32>, data: *mut c_void) -> u32>,
    /// `g_timeout_add` — a repeating main-loop timer. The callback must return
    /// 1 (true) to keep repeating, 0 (false) to stop.
    pub g_timeout_add: Option<unsafe extern "C" fn(interval_ms: u32, func: Option<unsafe extern "C" fn(*mut c_void) -> i32>, data: *mut c_void) -> u32>,
    // pango (optional)
    pub pango_layout_new: Option<unsafe extern "C" fn(context: *mut c_void) -> *mut c_void>,
    pub pango_layout_set_text: Option<unsafe extern "C" fn(layout: *mut c_void, text: *const i8, len: i32)>,
    pub pango_layout_get_size: Option<unsafe extern "C" fn(layout: *mut c_void, width: *mut i32, height: *mut i32)>,
    // cairo surface/context helpers
    pub cairo_create: Option<unsafe extern "C" fn(surface: *mut c_void) -> *mut c_void>,
    pub cairo_font_face_destroy: Option<unsafe extern "C" fn(face: *mut c_void)>,
    pub cairo_move_to: Option<unsafe extern "C" fn(cr: *mut c_void, x: f64, y: f64)>,
    pub cairo_set_source_rgb: Option<unsafe extern "C" fn(cr: *mut c_void, r: f64, g: f64, b: f64)>,
    pub cairo_set_source_rgba: Option<unsafe extern "C" fn(cr: *mut c_void, r: f64, g: f64, b: f64, a: f64)>,
    pub cairo_rectangle: Option<unsafe extern "C" fn(cr: *mut c_void, x: f64, y: f64, w: f64, h: f64)>,
    pub cairo_fill: Option<unsafe extern "C" fn(cr: *mut c_void)>,
    pub cairo_stroke: Option<unsafe extern "C" fn(cr: *mut c_void)>,
    pub cairo_set_line_width: Option<unsafe extern "C" fn(cr: *mut c_void, w: f64)>,
    pub cairo_select_font_face: Option<unsafe extern "C" fn(cr: *mut c_void, family: *const i8, slant: i32, weight: i32)>,
    pub cairo_set_font_size: Option<unsafe extern "C" fn(cr: *mut c_void, size: f64)>,
    pub cairo_show_text: Option<unsafe extern "C" fn(cr: *mut c_void, utf8: *const i8)>,
    // widget helpers
    pub gtk_widget_queue_draw: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_queue_draw_area: Option<unsafe extern "C" fn(widget: *mut c_void, x: i32, y: i32, width: i32, height: i32)>,
    pub gtk_widget_set_can_focus: Option<unsafe extern "C" fn(widget: *mut c_void, can_focus: i32)>,
    pub gtk_label_set_xalign: Option<unsafe extern "C" fn(label: *mut c_void, xalign: f32)>,
    pub gtk_event_controller_key_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_event_controller_focus_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_widget_add_controller: Option<unsafe extern "C" fn(widget: *mut c_void, controller: *mut c_void)>,
    pub gtk_event_controller_set_propagation_phase: Option<unsafe extern "C" fn(controller: *mut c_void, phase: u32)>,
    pub gtk_widget_set_can_target: Option<unsafe extern "C" fn(widget: *mut c_void, can_target: i32)>,
    pub gtk_widget_set_halign: Option<unsafe extern "C" fn(widget: *mut c_void, align: i32)>,
    pub gtk_widget_set_valign: Option<unsafe extern "C" fn(widget: *mut c_void, align: i32)>,
    pub gtk_widget_get_allocated_width: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_widget_get_allocated_height: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_widget_get_mapped: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_widget_get_display: Option<unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void>,
    pub gdk_display_sync: Option<unsafe extern "C" fn(display: *mut c_void)>,
    pub gtk_widget_get_frame_clock: Option<unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void>,
    pub gdk_frame_clock_request_phase: Option<unsafe extern "C" fn(clock: *mut c_void, phase: u32)>,
    pub gdk_frame_clock_begin_updating: Option<unsafe extern "C" fn(clock: *mut c_void)>,
    pub gdk_frame_clock_end_updating: Option<unsafe extern "C" fn(clock: *mut c_void)>,
    pub gtk_gesture_click_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_scrolled_window_new: Option<unsafe extern "C" fn(hadjustment: *mut c_void, vadjustment: *mut c_void) -> *mut c_void>,
    pub gtk_scrolled_window_set_policy: Option<unsafe extern "C" fn(scrolled: *mut c_void, h_policy: u32, v_policy: u32)>,
    pub gtk_scrolled_window_set_child: Option<unsafe extern "C" fn(scrolled: *mut c_void, child: *mut c_void)>,
    pub gtk_scrolled_window_get_vadjustment: Option<unsafe extern "C" fn(sw: *mut c_void) -> *mut c_void>,
    pub gtk_scrolled_window_get_hadjustment: Option<unsafe extern "C" fn(sw: *mut c_void) -> *mut c_void>,
    pub gtk_adjustment_get_value: Option<unsafe extern "C" fn(adj: *mut c_void) -> f64>,
    pub gtk_adjustment_get_upper: Option<unsafe extern "C" fn(adj: *mut c_void) -> f64>,
    pub gtk_adjustment_get_page_size: Option<unsafe extern "C" fn(adj: *mut c_void) -> f64>,
    // GMenuModel iteration (for GTK3 fallback)
    pub g_menu_model_get_n_items: Option<unsafe extern "C" fn(model: *mut c_void) -> i32>,
    pub g_menu_model_get_item_attribute_value: Option<unsafe extern "C" fn(model: *mut c_void, item_index: i32, attribute: *const i8, expected_type: *const c_void) -> *mut c_void>,
    pub g_menu_model_get_item_link: Option<unsafe extern "C" fn(model: *mut c_void, item_index: i32, link: *const i8) -> *mut c_void>,
    pub g_variant_get_string: Option<unsafe extern "C" fn(value: *mut c_void, length: *mut usize) -> *const i8>,
    pub g_variant_unref: Option<unsafe extern "C" fn(value: *mut c_void)>,

    // Dialog
    pub gtk_dialog_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_dialog_add_button: Option<unsafe extern "C" fn(dialog: *mut c_void, button_text: *const i8, response_id: i32) -> *mut c_void>,
    pub gtk_dialog_get_content_area: Option<unsafe extern "C" fn(dialog: *mut c_void) -> *mut c_void>,
    pub gtk_dialog_run: Option<unsafe extern "C" fn(dialog: *mut c_void) -> i32>,

    // Dropdown - GTK3 ComboBoxText
    pub gtk_combo_box_text_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_combo_box_text_append_text: Option<unsafe extern "C" fn(combo: *mut c_void, text: *const i8)>,
    pub gtk_combo_box_text_get_active_text: Option<unsafe extern "C" fn(combo: *mut c_void) -> *const i8>,
    pub gtk_combo_box_set_active: Option<unsafe extern "C" fn(combo: *mut c_void, index_: i32)>,
    pub gtk_combo_box_get_active: Option<unsafe extern "C" fn(combo: *mut c_void) -> i32>,

    // Dropdown - GTK4 DropDown
    pub gtk_drop_down_new: Option<unsafe extern "C" fn(model: *mut c_void, expression: *mut c_void) -> *mut c_void>,
    pub gtk_drop_down_set_selected: Option<unsafe extern "C" fn(dropdown: *mut c_void, selected: u32)>,
    pub gtk_drop_down_get_selected: Option<unsafe extern "C" fn(dropdown: *mut c_void) -> u32>,
    pub gtk_string_list_new: Option<unsafe extern "C" fn(strings: *const *const i8) -> *mut c_void>,

    // Checkbox / CheckButton
    pub gtk_check_button_new_with_label: Option<unsafe extern "C" fn(label: *const i8) -> *mut c_void>,
    pub gtk_check_button_get_active: Option<unsafe extern "C" fn(check_button: *mut c_void) -> i32>,
    pub gtk_check_button_set_active: Option<unsafe extern "C" fn(check_button: *mut c_void, is_active: i32)>,
    pub gtk_check_button_set_group: Option<unsafe extern "C" fn(check_button: *mut c_void, group: *mut c_void)>,
    pub gtk_toggle_button_get_active: Option<unsafe extern "C" fn(toggle_button: *mut c_void) -> i32>,
    pub gtk_toggle_button_set_active: Option<unsafe extern "C" fn(toggle_button: *mut c_void, is_active: i32)>,

    // RadioButton
    pub gtk_radio_button_new_with_label: Option<unsafe extern "C" fn(group: *mut c_void, label: *const i8) -> *mut c_void>,

    // TextView / TextArea
    pub gtk_text_view_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_text_buffer_new: Option<unsafe extern "C" fn(table: *mut c_void) -> *mut c_void>,
    pub gtk_text_view_get_buffer: Option<unsafe extern "C" fn(text_view: *mut c_void) -> *mut c_void>,
    pub gtk_text_buffer_set_text: Option<unsafe extern "C" fn(buffer: *mut c_void, text: *const i8, len: i32)>,
    pub gtk_text_buffer_get_text: Option<unsafe extern "C" fn(buffer: *mut c_void, start: *mut c_void, end: *mut c_void, include_hidden_chars: i32) -> *mut c_void>,
    pub gtk_text_buffer_get_start_iter: Option<unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void)>,
    pub gtk_text_buffer_get_end_iter: Option<unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void)>,
    pub gtk_text_iter_copy: Option<unsafe extern "C" fn(iter: *mut c_void) -> *mut c_void>,
    pub gtk_text_iter_free: Option<unsafe extern "C" fn(iter: *mut c_void)>,
    pub gtk_text_view_set_wrap_mode: Option<unsafe extern "C" fn(text_view: *mut c_void, wrap_mode: i32)>,
    pub gtk_text_buffer_insert: Option<unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void, text: *const i8, len: i32) -> *mut c_void>,
    pub gtk_text_buffer_insert_markup: Option<unsafe extern "C" fn(buffer: *mut c_void, iter: *mut c_void, markup: *const i8, len: i32)>,
    pub gtk_text_buffer_copy_clipboard: Option<unsafe extern "C" fn(buffer: *mut c_void, clipboard: *mut c_void)>,
    pub gtk_widget_get_clipboard: Option<unsafe extern "C" fn(widget: *mut c_void, selection: i32) -> *mut c_void>,
    pub gtk_text_view_set_editable: Option<unsafe extern "C" fn(text_view: *mut c_void, editable: i32)>,
    pub gtk_text_view_get_editable: Option<unsafe extern "C" fn(text_view: *mut c_void) -> i32>,
    pub gtk_widget_get_can_focus: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    // Clickable-link support (TextView coordinate mapping + selection guard +
    // button-release query). All optional: a missing symbol degrades to
    // non-clickable links, never a crash.
    pub gtk_text_view_window_to_buffer_coords: Option<GtkTextViewWindowToBufferCoords>,
    pub gtk_text_view_get_iter_at_location: Option<GtkTextViewGetIterAtLocation>,
    pub gtk_text_iter_get_line: Option<unsafe extern "C" fn(iter: *mut c_void) -> i32>,
    pub gtk_text_iter_get_line_offset: Option<unsafe extern "C" fn(iter: *mut c_void) -> i32>,
    pub gtk_text_buffer_get_has_selection: Option<unsafe extern "C" fn(buffer: *mut c_void) -> i32>,
    pub gdk_event_get_button: Option<unsafe extern "C" fn(event: *mut c_void, button: *mut u32) -> i32>,

    // Widget helpers
    pub gtk_widget_set_hexpand: Option<unsafe extern "C" fn(widget: *mut c_void, expand: i32)>,
    pub gtk_widget_set_vexpand: Option<unsafe extern "C" fn(widget: *mut c_void, expand: i32)>,
    pub gtk_widget_get_hexpand: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,
    pub gtk_widget_get_vexpand: Option<unsafe extern "C" fn(widget: *mut c_void) -> i32>,

    // GtkEditable (GTK4)
    pub gtk_editable_get_text: Option<unsafe extern "C" fn(editable: *mut c_void) -> *const i8>,
    pub gtk_editable_set_text: Option<unsafe extern "C" fn(editable: *mut c_void, text: *const i8)>,
    pub gtk_editable_set_position: Option<unsafe extern "C" fn(editable: *mut c_void, position: i32)>,
    pub gtk_editable_select_region: Option<unsafe extern "C" fn(editable: *mut c_void, start: i32, end: i32)>,

    // GtkWidget parent handling
    pub gtk_widget_unparent: Option<unsafe extern "C" fn(widget: *mut c_void)>,
    pub gtk_widget_get_parent: Option<unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void>,

    // GtkWidget tree traversal
    pub gtk_widget_get_first_child: Option<unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void>,
    pub gtk_widget_get_next_sibling: Option<unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void>,

    // GtkPopoverMenuBarItem
    pub gtk_popover_menu_bar_item_get_popover: Option<unsafe extern "C" fn(item: *mut c_void) -> *mut c_void>,
    // GtkPopover
    pub gtk_popover_get_child: Option<unsafe extern "C" fn(popover: *mut c_void) -> *mut c_void>,
    pub gtk_popover_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_popover_set_child: Option<unsafe extern "C" fn(popover: *mut c_void, child: *mut c_void)>,

    // GtkMenuButton — manual menu bar construction
    pub gtk_menu_button_new: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gtk_menu_button_set_popover: Option<unsafe extern "C" fn(button: *mut c_void, popover: *mut c_void)>,
    pub gtk_menu_button_set_label: Option<unsafe extern "C" fn(button: *mut c_void, label: *const i8)>,
        pub gtk_menu_button_set_has_frame: Option<unsafe extern "C" fn(button: *mut c_void, has_frame: i32)>,
    pub gtk_menu_button_set_primary: Option<unsafe extern "C" fn(button: *mut c_void, primary: i32)>,
    pub gtk_button_set_has_frame: Option<unsafe extern "C" fn(button: *mut c_void, has_frame: i32)>,
    pub gtk_widget_add_css_class: Option<unsafe extern "C" fn(widget: *mut c_void, css_class: *const i8)>,
    pub gtk_menu_button_get_popover: Option<unsafe extern "C" fn(button: *mut c_void) -> *mut c_void>,

    // GtkPopoverMenu — new from model
    pub gtk_popover_menu_new_from_model: Option<unsafe extern "C" fn(model: *mut c_void) -> *mut c_void>,

    // GtkMenuButton
    pub gtk_menu_button_get_label: Option<unsafe extern "C" fn(button: *mut c_void) -> *const i8>,
    pub gtk_menu_button_set_active: Option<unsafe extern "C" fn(button: *mut c_void, active: i32)>,

    // GtkWindow default size
    pub gtk_window_set_default_size: Option<unsafe extern "C" fn(window: *mut c_void, width: i32, height: i32)>,

    // DrawingArea canvas (GTK4)
    pub gtk_drawing_area_set_draw_func: Option<unsafe extern "C" fn(area: *mut c_void, draw_func: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, *mut c_void)>, user_data: *mut c_void, destroy: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>)>,
    pub gtk_drawing_area_set_content_width: Option<unsafe extern "C" fn(area: *mut c_void, width: i32)>,
    pub gtk_drawing_area_set_content_height: Option<unsafe extern "C" fn(area: *mut c_void, height: i32)>,

    // GtkNative (GtkWindow implements this) — for direct surface access
    pub gtk_native_get_surface: Option<unsafe extern "C" fn(native: *mut c_void) -> *mut c_void>,

    // GdkSurface operations — for direct Cairo rendering bypassing frame clock
    pub gdk_surface_create_cairo_context: Option<unsafe extern "C" fn(surface: *mut c_void) -> *mut c_void>,
    pub gdk_surface_get_width: Option<unsafe extern "C" fn(surface: *mut c_void) -> i32>,
    pub gdk_surface_get_height: Option<unsafe extern "C" fn(surface: *mut c_void) -> i32>,
    // GdkSurface begin/end draw frame (GTK 4.0-4.14, fallback when gdk_surface_create_cairo_context unavailable)
    pub gdk_surface_begin_draw_frame: Option<unsafe extern "C" fn(surface: *mut c_void, region: *mut c_void) -> *mut c_void>,
    pub gdk_surface_end_draw_frame: Option<unsafe extern "C" fn(surface: *mut c_void, context: *mut c_void)>,
    pub gdk_draw_context_get_cairo_context: Option<unsafe extern "C" fn(context: *mut c_void) -> *mut c_void>,

    // Cairo canvas drawing
    pub cairo_text_extents: Option<unsafe extern "C" fn(cr: *mut c_void, utf8: *const i8, extents: *mut c_void)>,
    pub cairo_save: Option<unsafe extern "C" fn(cr: *mut c_void)>,
    pub cairo_restore: Option<unsafe extern "C" fn(cr: *mut c_void)>,
    pub cairo_clip: Option<unsafe extern "C" fn(cr: *mut c_void)>,
    pub cairo_line_to: Option<unsafe extern "C" fn(cr: *mut c_void, x: f64, y: f64)>,
    pub cairo_paint: Option<unsafe extern "C" fn(cr: *mut c_void)>,
    // Cairo context lifecycle
    pub cairo_destroy: Option<unsafe extern "C" fn(cr: *mut c_void)>,
}

impl Symbols {
    pub fn empty() -> Self {
        unsafe { std::mem::zeroed() }
    }

    pub fn load(libs: &std::collections::HashMap<String, std::sync::Arc<Library>>) -> Result<Self, Error> {
        // helper to lookup in libgtk first then glib/gobject as appropriate
        let gtk = libs.get("libgtk").expect("libgtk missing");
        let glib = libs.get("libglib").expect("libglib missing");
        let gobject = libs.get("libgobject").expect("libgobject missing");

        unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Option<T> {
            match lib.get::<T>(name.as_bytes()) {
                Ok(s) => Some(*s),
                Err(_) => None,
            }
        }

        macro_rules! open_sym_try {
            ($libs:ident, $key:expr, $t:ty, $name:expr) => {
                if let Some(lib) = $libs.get($key) { unsafe { match lib.get::<$t>($name.as_bytes()) { Ok(s) => Some(*s), Err(_) => None } } } else { None }
            };
        }

        // glib
        let g_main_loop_new = unsafe { sym::<GMainLoopNew>(glib, "g_main_loop_new") };
        let g_main_loop_run = unsafe { sym::<GMainLoopRun>(glib, "g_main_loop_run") };
        let g_main_loop_quit = unsafe { sym::<GMainLoopQuit>(glib, "g_main_loop_quit") };

        // gobject
        let g_object_ref = unsafe { sym::<GObjectRef>(gobject, "g_object_ref") };
        let g_object_unref = unsafe { sym::<GObjectUnref>(gobject, "g_object_unref") };
        let g_object_ref_sink = unsafe { sym::<GObjectRefSink>(gobject, "g_object_ref_sink") };
        let g_signal_connect_data = unsafe { sym::<GSignalConnectData>(gobject, "g_signal_connect_data") };
        let g_signal_connect = unsafe { sym::<GSignalConnect>(gobject, "g_signal_connect") };

        // gtk symbols
        let gtk_window_new = unsafe { sym::<GtkWindowNew>(gtk, "gtk_window_new") };
        let gtk_window_set_title = unsafe { sym::<GtkWindowSetTitle>(gtk, "gtk_window_set_title") };
        let gtk_button_new_with_label = unsafe { sym::<GtkButtonNewWithLabel>(gtk, "gtk_button_new_with_label") };
        let gtk_bin_get_child = unsafe { sym::<GtkBinGetChild>(gtk, "gtk_bin_get_child") };
        let gtk_label_new = unsafe { sym::<GtkLabelNew>(gtk, "gtk_label_new") };
        let gtk_label_set_text = unsafe { sym::<GtkLabelSetText>(gtk, "gtk_label_set_text") };
        let gtk_label_get_text = unsafe { sym::<GtkLabelGetText>(gtk, "gtk_label_get_text") };
        let gtk_box_new = unsafe { sym::<GtkBoxNew>(gtk, "gtk_box_new") };
        let gtk_box_append = unsafe { sym::<GtkBoxAppend>(gtk, "gtk_box_append") };
        let gtk_box_pack_start = unsafe { sym::<GtkBoxPackStart>(gtk, "gtk_box_pack_start") };
        let gtk_container_add = unsafe { sym::<GtkContainerAdd>(gtk, "gtk_container_add") };
        let gtk_container_remove = unsafe { sym::<GtkContainerRemove>(gtk, "gtk_container_remove") };
        let gtk_window_set_child = unsafe { sym::<GtkWindowSetChild>(gtk, "gtk_window_set_child") };
        let gtk_widget_show_all = unsafe { sym::<GtkWidgetShowAll>(gtk, "gtk_widget_show_all") };
        let gtk_widget_show = unsafe { sym::<unsafe extern "C" fn(*mut c_void)>(gtk, "gtk_widget_show") };
        let gtk_widget_hide = unsafe { sym::<unsafe extern "C" fn(*mut c_void)>(gtk, "gtk_widget_hide") };
        let gtk_widget_queue_resize = unsafe { sym::<unsafe extern "C" fn(*mut c_void)>(gtk, "gtk_widget_queue_resize") };
        let gtk_window_present = unsafe { sym::<GtkWindowPresent>(gtk, "gtk_window_present") };
        let _gtk_window_set_application = unsafe { sym::<GtkWindowSetApplication>(gtk, "gtk_window_set_application") };
        let gtk_file_chooser_native_new = open_sym_try!(libs, "libgio", GtkFileChooserNativeNew, "gtk_file_chooser_native_new").or_else(|| unsafe { sym::<GtkFileChooserNativeNew>(gtk, "gtk_file_chooser_native_new") });
        let gtk_native_dialog_run = open_sym_try!(libs, "libgio", GtkNativeDialogRun, "gtk_native_dialog_run").or_else(|| unsafe { sym::<GtkNativeDialogRun>(gtk, "gtk_native_dialog_run") });
        let gtk_file_chooser_get_filename = open_sym_try!(libs, "libgio", GtkFileChooserGetFilename, "gtk_file_chooser_get_filename").or_else(|| unsafe { sym::<GtkFileChooserGetFilename>(gtk, "gtk_file_chooser_get_filename") });
        let gtk_widget_destroy = unsafe { sym::<GtkWidgetDestroy>(gtk, "gtk_widget_destroy") };
        let gtk_window_close = unsafe { sym::<GtkWindowClose>(gtk, "gtk_window_close") };
        let g_free = unsafe { sym::<GFree>(glib, "g_free") };
        let gdk_display_get_default = open_sym_try!(libs, "libgdk", GdkDisplayGetDefault, "gdk_display_get_default").or_else(|| unsafe { sym::<GdkDisplayGetDefault>(gtk, "gdk_display_get_default") });
        let gdk_screen_get_default = open_sym_try!(libs, "libgdk", GdkScreenGetDefault, "gdk_screen_get_default").or_else(|| unsafe { sym::<GdkScreenGetDefault>(gtk, "gdk_screen_get_default") });
        let gtk_style_context_add_provider_for_display = unsafe { sym::<GtkStyleContextAddProviderForDisplay>(gtk, "gtk_style_context_add_provider_for_display") };
        let gtk_style_context_add_provider_for_screen = unsafe { sym::<GtkStyleContextAddProviderForScreen>(gtk, "gtk_style_context_add_provider_for_screen") };
        let gdk_event_get_keyval = open_sym_try!(libs, "libgdk", GdkEventGetKeyval, "gdk_event_get_keyval").or_else(|| unsafe { sym::<GdkEventGetKeyval>(gtk, "gdk_event_get_keyval") });
        let gdk_event_get_state = open_sym_try!(libs, "libgdk", GdkEventGetState, "gdk_event_get_state").or_else(|| unsafe { sym::<GdkEventGetState>(gtk, "gdk_event_get_state") });
        let gdk_keyval_from_name = open_sym_try!(libs, "libgdk", GdkKeyvalFromName, "gdk_keyval_from_name").or_else(|| unsafe { sym::<GdkKeyvalFromName>(gtk, "gdk_keyval_from_name") });
        let gtk_grid_new = unsafe { sym::<GtkGridNew>(gtk, "gtk_grid_new") };
        let gtk_grid_attach = unsafe { sym::<GtkGridAttach>(gtk, "gtk_grid_attach") };
        let gtk_entry_new = unsafe { sym::<GtkEntryNew>(gtk, "gtk_entry_new") };
        let gtk_entry_set_text = unsafe { sym::<GtkEntrySetText>(gtk, "gtk_entry_set_text") };
        let gtk_entry_get_text = unsafe { sym::<GtkEntryGetText>(gtk, "gtk_entry_get_text") };
        let gtk_entry_set_width_chars = unsafe { sym::<GtkEntrySetWidthChars>(gtk, "gtk_entry_set_width_chars") };
        let gtk_entry_set_placeholder_text = unsafe { sym::<GtkEntrySetPlaceholderText>(gtk, "gtk_entry_set_placeholder_text") };
        let gtk_widget_set_size_request = unsafe { sym::<GtkWidgetSetSizeRequest>(gtk, "gtk_widget_set_size_request") };
        let gtk_entry_set_has_frame = unsafe { sym::<GtkEntrySetHasFrame>(gtk, "gtk_entry_set_has_frame") };
        let gtk_entry_set_editable = unsafe { sym::<GtkEntrySetEditable>(gtk, "gtk_editable_set_editable") };
        let gtk_drawing_area_new = unsafe { sym::<GtkDrawingAreaNew>(gtk, "gtk_drawing_area_new") };
        let gtk_label_set_markup = unsafe { sym::<GtkLabelSetMarkup>(gtk, "gtk_label_set_markup") };
        let gtk_widget_set_visible = unsafe { sym::<GtkWidgetSetVisible>(gtk, "gtk_widget_set_visible") };
        let gtk_widget_grab_focus = unsafe { sym::<GtkWidgetGrabFocus>(gtk, "gtk_widget_grab_focus") };
        let gtk_widget_has_focus = unsafe { sym::<GtkWidgetHasFocus>(gtk, "gtk_widget_has_focus") };
        let gtk_widget_is_focus = unsafe { sym::<GtkWidgetIsFocus>(gtk, "gtk_widget_is_focus") };
        let gtk_window_get_focus = unsafe { sym::<GtkWindowGetFocus>(gtk, "gtk_window_get_focus") };
        let gtk_widget_get_style_context = unsafe { sym::<GtkWidgetGetStyleContext>(gtk, "gtk_widget_get_style_context") };
        let gtk_style_context_add_class = unsafe { sym::<GtkStyleContextAddClass>(gtk, "gtk_style_context_add_class") };
        let gtk_style_context_remove_class = unsafe { sym::<GtkStyleContextRemoveClass>(gtk, "gtk_style_context_remove_class") };
        let gtk_css_provider_new = unsafe { sym::<GtkCssProviderNew>(gtk, "gtk_css_provider_new") };
        let gtk_css_provider_load_from_data = unsafe { sym::<GtkCssProviderLoadFromData>(gtk, "gtk_css_provider_load_from_data") };
        let gtk_style_context_add_provider = unsafe { sym::<GtkStyleContextAddProvider>(gtk, "gtk_style_context_add_provider") };
        let gtk_overlay_new = unsafe { sym::<GtkOverlayNew>(gtk, "gtk_overlay_new") };
        let gtk_overlay_add_overlay = unsafe { sym::<GtkOverlayAddOverlay>(gtk, "gtk_overlay_add_overlay") };
        let gtk_overlay_set_overlay_pass_through = unsafe { sym::<GtkOverlaySetOverlayPassThrough>(gtk, "gtk_overlay_set_overlay_pass_through") };
        let gtk_overlay_set_child = unsafe { sym::<GtkOverlaySetChild>(gtk, "gtk_overlay_set_child") };
        let gtk_widget_set_margin_start = unsafe { sym::<GtkWidgetSetMarginStart>(gtk, "gtk_widget_set_margin_start") };
        let gtk_widget_set_margin_top = unsafe { sym::<GtkWidgetSetMarginTop>(gtk, "gtk_widget_set_margin_top") };
        let gtk_widget_add_events = unsafe { sym::<GtkWidgetAddEvents>(gtk, "gtk_widget_add_events") };
        let gtk_widget_activate = unsafe { sym::<GtkWidgetActivate>(gtk, "gtk_widget_activate") };
        let gtk_widget_get_visible = unsafe { sym::<GtkWidgetGetVisible>(gtk, "gtk_widget_get_visible") };
        let gtk_widget_activate_action = unsafe { sym::<GtkWidgetActivateAction>(gtk, "gtk_widget_activate_action") };
        let gtk_init = unsafe { sym::<GtkInit>(gtk, "gtk_init") };
        let g_signal_emit_by_name = unsafe { sym::<GSignalEmitByName>(gobject, "g_signal_emit_by_name") };
        let g_idle_add = unsafe { sym::<unsafe extern "C" fn(func: Option<unsafe extern "C" fn(*mut c_void) -> i32>, data: *mut c_void) -> u32>(glib, "g_idle_add") };
        let g_timeout_add = unsafe { sym::<GTimeoutAdd>(glib, "g_timeout_add") };
        // pango symbols are optional; we try to resolve them from the gtk lib too (some symbols may be available)
        let pango_layout_new = None;
        let pango_layout_set_text = None;
        let pango_layout_get_size = None;

        // cairo symbols (optional)
        let cairo_create = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void) -> *mut c_void, "cairo_create").or_else(|| None);
        let cairo_font_face_destroy = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void), "cairo_font_face_destroy").or_else(|| None);
        let cairo_move_to = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, f64, f64), "cairo_move_to").or_else(|| None);
        let cairo_set_source_rgb = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, f64, f64, f64), "cairo_set_source_rgb").or_else(|| None);
        let cairo_set_source_rgba = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, f64, f64, f64, f64), "cairo_set_source_rgba").or_else(|| None);
        let cairo_rectangle = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, f64, f64, f64, f64), "cairo_rectangle").or_else(|| None);
        let cairo_fill = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void), "cairo_fill").or_else(|| None);
        let cairo_stroke = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void), "cairo_stroke").or_else(|| None);
        let cairo_set_line_width = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, f64), "cairo_set_line_width").or_else(|| None);
        let cairo_select_font_face = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, *const i8, i32, i32), "cairo_select_font_face").or_else(|| None);
        let cairo_set_font_size = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, f64), "cairo_set_font_size").or_else(|| None);
        let cairo_show_text = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void, *const i8), "cairo_show_text").or_else(|| None);
        let cairo_text_extents = open_sym_try!(libs, "libcairo", CairoTextExtents, "cairo_text_extents").or_else(|| None);
        let cairo_save = open_sym_try!(libs, "libcairo", CairoSave, "cairo_save").or_else(|| None);
        let cairo_restore = open_sym_try!(libs, "libcairo", CairoRestore, "cairo_restore").or_else(|| None);
        let cairo_clip = open_sym_try!(libs, "libcairo", CairoClip, "cairo_clip").or_else(|| None);
        let cairo_line_to = open_sym_try!(libs, "libcairo", CairoLineTo, "cairo_line_to").or_else(|| None);
        let cairo_paint = open_sym_try!(libs, "libcairo", CairoPaint, "cairo_paint").or_else(|| None);
        let cairo_destroy = open_sym_try!(libs, "libcairo", unsafe extern "C" fn(*mut c_void), "cairo_destroy").or_else(|| None);
        let gtk_drawing_area_set_draw_func = unsafe { sym::<GtkDrawingAreaSetDrawFunc>(gtk, "gtk_drawing_area_set_draw_func") };
        let gtk_drawing_area_set_content_width = unsafe { sym::<GtkDrawingAreaSetContentWidth>(gtk, "gtk_drawing_area_set_content_width") };
        let gtk_drawing_area_set_content_height = unsafe { sym::<GtkDrawingAreaSetContentHeight>(gtk, "gtk_drawing_area_set_content_height") };
        let gtk_native_get_surface = unsafe { sym::<GtkNativeGetSurface>(gtk, "gtk_native_get_surface") };
        let gdk_surface_create_cairo_context = unsafe { sym::<GdkSurfaceCreateCairoContext>(gtk, "gdk_surface_create_cairo_context") };
        let gdk_surface_get_width = unsafe { sym::<GdkSurfaceGetWidth>(gtk, "gdk_surface_get_width") };
        let gdk_surface_get_height = unsafe { sym::<GdkSurfaceGetHeight>(gtk, "gdk_surface_get_height") };
        // Fallback direct-drawing symbols (GTK 4.0-4.14, deprecated in favor of gdk_surface_create_cairo_context)
        let gdk_surface_begin_draw_frame = unsafe { sym::<GdkSurfaceBeginDrawFrame>(gtk, "gdk_surface_begin_draw_frame") };
        let gdk_surface_end_draw_frame = unsafe { sym::<GdkSurfaceEndDrawFrame>(gtk, "gdk_surface_end_draw_frame") };
        let gdk_draw_context_get_cairo_context = unsafe { sym::<GdkDrawContextGetCairoContext>(gtk, "gdk_draw_context_get_cairo_context") };
        let gtk_widget_set_can_focus = unsafe { sym::<GtkWidgetSetCanFocus>(gtk, "gtk_widget_set_can_focus") };
        let gtk_widget_queue_draw = unsafe { sym::<unsafe extern "C" fn(*mut c_void)>(gtk, "gtk_widget_queue_draw") };
        let gtk_widget_queue_draw_area = unsafe { sym::<GtkWidgetQueueDrawArea>(gtk, "gtk_widget_queue_draw_area") };
        let gtk_label_set_xalign = unsafe { sym::<GtkLabelSetXalign>(gtk, "gtk_label_set_xalign") };
        let gtk_event_controller_key_new = unsafe { sym::<GtkEventControllerKeyNew>(gtk, "gtk_event_controller_key_new") };
        let gtk_event_controller_focus_new = unsafe { sym::<GtkEventControllerFocusNew>(gtk, "gtk_event_controller_focus_new") };
        let gtk_widget_add_controller = unsafe { sym::<GtkWidgetAddController>(gtk, "gtk_widget_add_controller") };
        let gtk_event_controller_set_propagation_phase = unsafe { sym::<GtkEventControllerSetPropagationPhase>(gtk, "gtk_event_controller_set_propagation_phase") };
        let gtk_widget_set_halign = unsafe { sym::<GtkWidgetSetHalign>(gtk, "gtk_widget_set_halign") };
        let gtk_widget_set_valign = unsafe { sym::<GtkWidgetSetValign>(gtk, "gtk_widget_set_valign") };
        let gtk_widget_get_allocated_width = unsafe { sym::<GtkWidgetGetAllocatedWidth>(gtk, "gtk_widget_get_allocated_width") };
        let gtk_widget_get_allocated_height = unsafe { sym::<GtkWidgetGetAllocatedHeight>(gtk, "gtk_widget_get_allocated_height") };
        let gtk_widget_get_mapped = unsafe { sym::<GtkWidgetGetMapped>(gtk, "gtk_widget_get_mapped") };
        let gtk_widget_get_display = unsafe { sym::<GtkWidgetGetDisplay>(gtk, "gtk_widget_get_display") };
        let gdk_display_sync = unsafe { sym::<GdkDisplaySync>(gtk, "gdk_display_sync") };
        let gtk_widget_get_frame_clock = unsafe { sym::<GtkWidgetGetFrameClock>(gtk, "gtk_widget_get_frame_clock") };
        let gdk_frame_clock_request_phase = unsafe { sym::<GdkFrameClockRequestPhase>(gtk, "gdk_frame_clock_request_phase") };
        let gdk_frame_clock_begin_updating = unsafe { sym::<GdkFrameClockBeginUpdating>(gtk, "gdk_frame_clock_begin_updating") };
        let gdk_frame_clock_end_updating = unsafe { sym::<GdkFrameClockEndUpdating>(gtk, "gdk_frame_clock_end_updating") };
        let gtk_gesture_click_new = unsafe { sym::<GtkGestureClickNew>(gtk, "gtk_gesture_click_new") };
        let gtk_scrolled_window_new = unsafe { sym::<GtkScrolledWindowNew>(gtk, "gtk_scrolled_window_new") };
        let gtk_scrolled_window_set_policy = unsafe { sym::<GtkScrolledWindowSetPolicy>(gtk, "gtk_scrolled_window_set_policy") };
        let gtk_scrolled_window_set_child = unsafe { sym::<GtkScrolledWindowSetChild>(gtk, "gtk_scrolled_window_set_child") };
        let gtk_scrolled_window_get_vadjustment = unsafe { sym::<GtkScrolledWindowGetVadjustment>(gtk, "gtk_scrolled_window_get_vadjustment") };
        let gtk_scrolled_window_get_hadjustment = unsafe { sym::<GtkScrolledWindowGetHadjustment>(gtk, "gtk_scrolled_window_get_hadjustment") };
        let gtk_adjustment_get_value = unsafe { sym::<GtkAdjustmentGetValue>(gtk, "gtk_adjustment_get_value") };
        let gtk_adjustment_get_upper = unsafe { sym::<GtkAdjustmentGetUpper>(gtk, "gtk_adjustment_get_upper") };
        let gtk_adjustment_get_page_size = unsafe { sym::<GtkAdjustmentGetPageSize>(gtk, "gtk_adjustment_get_page_size") };

        // application/menu/action symbols (try glib/gio)
        // try gio first then glib for the app/menu symbols
        let gtk_application_new = open_sym_try!(libs, "libgio", GtkApplicationNew, "g_application_new").or_else(|| unsafe { sym::<GtkApplicationNew>(glib, "g_application_new") });
        let g_application_run = open_sym_try!(libs, "libgio", GApplicationRun, "g_application_run").or_else(|| unsafe { sym::<GApplicationRun>(glib, "g_application_run") });
        let g_application_register = open_sym_try!(libs, "libgio", GApplicationRegister, "g_application_register").or_else(|| unsafe { sym::<GApplicationRegister>(glib, "g_application_register") });
        let g_simple_action_new = open_sym_try!(libs, "libgio", GSimpleActionNew, "g_simple_action_new").or_else(|| unsafe { sym::<GSimpleActionNew>(glib, "g_simple_action_new") });
        let g_action_map_add_action = open_sym_try!(libs, "libgio", GActionMapAddAction, "g_action_map_add_action").or_else(|| unsafe { sym::<GActionMapAddAction>(glib, "g_action_map_add_action") });
        let g_action_group_activate_action = open_sym_try!(libs, "libgio", GActionGroupActivateAction, "g_action_group_activate_action").or_else(|| unsafe { sym::<GActionGroupActivateAction>(glib, "g_action_group_activate_action") });
        let g_action_map_lookup_action = open_sym_try!(libs, "libgio", GActionMapLookupAction, "g_action_map_lookup_action").or_else(|| unsafe { sym::<GActionMapLookupAction>(glib, "g_action_map_lookup_action") });
        let g_action_activate = open_sym_try!(libs, "libgio", GActionActivate, "g_action_activate").or_else(|| unsafe { sym::<GActionActivate>(glib, "g_action_activate") });
        let g_menu_new = open_sym_try!(libs, "libgio", GMenuNew, "g_menu_new").or_else(|| unsafe { sym::<GMenuNew>(glib, "g_menu_new") });
        let g_menu_append = open_sym_try!(libs, "libgio", GMenuAppend, "g_menu_append").or_else(|| unsafe { sym::<GMenuAppend>(glib, "g_menu_append") });
        let g_application_set_app_menu = open_sym_try!(libs, "libgio", GApplicationSetAppMenu, "g_application_set_app_menu").or_else(|| unsafe { sym::<GApplicationSetAppMenu>(glib, "g_application_set_app_menu") });
        let g_application_set_menubar = open_sym_try!(libs, "libgio", GApplicationSetMenubar, "g_application_set_menubar").or_else(|| unsafe { sym::<GApplicationSetMenubar>(glib, "g_application_set_menubar") });
        let g_menu_append_submenu = open_sym_try!(libs, "libgio", GMenuAppendSubmenu, "g_menu_append_submenu").or_else(|| unsafe { sym::<GMenuAppendSubmenu>(glib, "g_menu_append_submenu") });
        let gtk_popover_menu_bar_new_from_model = unsafe { sym::<GtkPopoverMenuBarNewFromModel>(gtk, "gtk_popover_menu_bar_new_from_model") };
        let gtk_menu_bar_new = unsafe { sym::<GtkMenuBarNew>(gtk, "gtk_menu_bar_new") };
        let gtk_menu_new = unsafe { sym::<GtkMenuNew>(gtk, "gtk_menu_new") };
        let gtk_menu_item_new_with_label = unsafe { sym::<GtkMenuItemNewWithLabel>(gtk, "gtk_menu_item_new_with_label") };
        let gtk_menu_item_new_with_mnemonic = unsafe { sym::<GtkMenuItemNewWithMnemonic>(gtk, "gtk_menu_item_new_with_mnemonic") };
        let gtk_menu_shell_append = unsafe { sym::<GtkMenuShellAppend>(gtk, "gtk_menu_shell_append") };
        let gtk_menu_item_set_submenu = unsafe { sym::<GtkMenuItemSetSubmenu>(gtk, "gtk_menu_item_set_submenu") };
        let gtk_window_set_application = unsafe { sym::<GtkWindowSetApplication>(gtk, "gtk_window_set_application") };
        let gtk_widget_insert_action_group = unsafe { sym::<GtkWidgetInsertActionGroup>(gtk, "gtk_widget_insert_action_group") };
        let gtk_actionable_set_detailed_action_name = unsafe { sym::<GtkActionableSetDetailedActionName>(gtk, "gtk_actionable_set_detailed_action_name") };
        let g_menu_model_get_n_items = open_sym_try!(libs, "libgio", GMenuModelGetNItems, "g_menu_model_get_n_items").or_else(|| unsafe { sym::<GMenuModelGetNItems>(glib, "g_menu_model_get_n_items") });
        let g_menu_model_get_item_attribute_value = open_sym_try!(libs, "libgio", GMenuModelGetItemAttributeValue, "g_menu_model_get_item_attribute_value").or_else(|| unsafe { sym::<GMenuModelGetItemAttributeValue>(glib, "g_menu_model_get_item_attribute_value") });
        let g_menu_model_get_item_link = open_sym_try!(libs, "libgio", GMenuModelGetItemLink, "g_menu_model_get_item_link").or_else(|| unsafe { sym::<GMenuModelGetItemLink>(glib, "g_menu_model_get_item_link") });
        let g_variant_get_string = unsafe { sym::<GVariantGetString>(glib, "g_variant_get_string") };
        let g_variant_unref = unsafe { sym::<GVariantUnref>(glib, "g_variant_unref") };

        // Dialog
        let gtk_dialog_new = unsafe { sym::<GtkDialogNew>(gtk, "gtk_dialog_new") };
        let gtk_dialog_add_button = unsafe { sym::<GtkDialogAddButton>(gtk, "gtk_dialog_add_button") };
        let gtk_dialog_get_content_area = unsafe { sym::<GtkDialogGetContentArea>(gtk, "gtk_dialog_get_content_area") };
        let gtk_dialog_run = unsafe { sym::<GtkDialogRun>(gtk, "gtk_dialog_run") };

        // Dropdown - GTK3 ComboBoxText
        let gtk_combo_box_text_new = unsafe { sym::<GtkComboBoxTextNew>(gtk, "gtk_combo_box_text_new") };
        let gtk_combo_box_text_append_text = unsafe { sym::<GtkComboBoxTextAppendText>(gtk, "gtk_combo_box_text_append_text") };
        let gtk_combo_box_text_get_active_text = unsafe { sym::<GtkComboBoxTextGetActiveText>(gtk, "gtk_combo_box_text_get_active_text") };
        let gtk_combo_box_set_active = unsafe { sym::<GtkComboBoxSetActive>(gtk, "gtk_combo_box_set_active") };
        let gtk_combo_box_get_active = unsafe { sym::<GtkComboBoxGetActive>(gtk, "gtk_combo_box_get_active") };

        // Dropdown - GTK4 DropDown
        let gtk_drop_down_new = unsafe { sym::<GtkDropDownNew>(gtk, "gtk_drop_down_new") };
        let gtk_drop_down_set_selected = unsafe { sym::<GtkDropDownSetSelected>(gtk, "gtk_drop_down_set_selected") };
        let gtk_drop_down_get_selected = unsafe { sym::<GtkDropDownGetSelected>(gtk, "gtk_drop_down_get_selected") };
        let gtk_string_list_new = unsafe { sym::<GtkStringListNew>(gtk, "gtk_string_list_new") };

        // Checkbox / CheckButton
        let gtk_check_button_new_with_label = unsafe { sym::<GtkCheckButtonNewWithLabel>(gtk, "gtk_check_button_new_with_label") };
        let gtk_check_button_get_active = unsafe { sym::<GtkCheckButtonGetActive>(gtk, "gtk_check_button_get_active") };
        let gtk_check_button_set_active = unsafe { sym::<GtkCheckButtonSetActive>(gtk, "gtk_check_button_set_active") };
        let gtk_check_button_set_group = unsafe { sym::<GtkCheckButtonSetGroup>(gtk, "gtk_check_button_set_group") };
        let gtk_toggle_button_get_active = unsafe { sym::<GtkToggleButtonGetActive>(gtk, "gtk_toggle_button_get_active") };
        let gtk_toggle_button_set_active = unsafe { sym::<GtkToggleButtonSetActive>(gtk, "gtk_toggle_button_set_active") };

        // RadioButton
        let gtk_radio_button_new_with_label = unsafe { sym::<GtkRadioButtonNewWithLabel>(gtk, "gtk_radio_button_new_with_label") };

        // TextView / TextArea
        let gtk_text_view_new = unsafe { sym::<GtkTextViewNew>(gtk, "gtk_text_view_new") };
        let gtk_text_buffer_new = unsafe { sym::<GtkTextBufferNew>(gtk, "gtk_text_buffer_new") };
        let gtk_text_view_get_buffer = unsafe { sym::<GtkTextViewGetBuffer>(gtk, "gtk_text_view_get_buffer") };
        let gtk_text_buffer_set_text = unsafe { sym::<GtkTextBufferSetText>(gtk, "gtk_text_buffer_set_text") };
        let gtk_text_buffer_get_text = unsafe { sym::<GtkTextBufferGetText>(gtk, "gtk_text_buffer_get_text") };
        let gtk_text_buffer_get_start_iter = unsafe { sym::<GtkTextBufferGetStartIter>(gtk, "gtk_text_buffer_get_start_iter") };
        let gtk_text_buffer_get_end_iter = unsafe { sym::<GtkTextBufferGetEndIter>(gtk, "gtk_text_buffer_get_end_iter") };
        let gtk_text_iter_copy = unsafe { sym::<GtkTextIterCopy>(gtk, "gtk_text_iter_copy") };
        let gtk_text_iter_free = unsafe { sym::<GtkTextIterFree>(gtk, "gtk_text_iter_free") };
        let gtk_text_view_set_wrap_mode = unsafe { sym::<GtkTextViewSetWrapMode>(gtk, "gtk_text_view_set_wrap_mode") };
        let gtk_text_buffer_insert = unsafe { sym::<GtkTextBufferInsert>(gtk, "gtk_text_buffer_insert") };
        let gtk_text_buffer_insert_markup = unsafe { sym::<GtkTextBufferInsertMarkup>(gtk, "gtk_text_buffer_insert_markup") };
        let gtk_text_buffer_copy_clipboard = unsafe { sym::<GtkTextBufferCopyClipboard>(gtk, "gtk_text_buffer_copy_clipboard") };
        let gtk_widget_get_clipboard = unsafe { sym::<GtkWidgetGetClipboard>(gtk, "gtk_widget_get_clipboard") };
        let gtk_text_view_set_editable = unsafe { sym::<GtkTextViewSetEditable>(gtk, "gtk_text_view_set_editable") };
        let gtk_text_view_get_editable = unsafe { sym::<GtkTextViewGetEditable>(gtk, "gtk_text_view_get_editable") };
        let gtk_widget_get_can_focus = unsafe { sym::<GtkWidgetGetCanFocus>(gtk, "gtk_widget_get_can_focus") };
        // Clickable-link support. Coordinates come from libgtk; the button
        // query lives in libgdk on builds that split GDK out of libgtk.
        let gtk_text_view_window_to_buffer_coords = unsafe { sym::<GtkTextViewWindowToBufferCoords>(gtk, "gtk_text_view_window_to_buffer_coords") };
        let gtk_text_view_get_iter_at_location = unsafe { sym::<GtkTextViewGetIterAtLocation>(gtk, "gtk_text_view_get_iter_at_location") };
        let gtk_text_iter_get_line = unsafe { sym::<GtkTextIterGetLine>(gtk, "gtk_text_iter_get_line") };
        let gtk_text_iter_get_line_offset = unsafe { sym::<GtkTextIterGetLineOffset>(gtk, "gtk_text_iter_get_line_offset") };
        let gtk_text_buffer_get_has_selection = unsafe { sym::<GtkTextBufferGetHasSelection>(gtk, "gtk_text_buffer_get_has_selection") };
        let gdk_event_get_button = open_sym_try!(libs, "libgdk", GdkEventGetButton, "gdk_event_get_button").or_else(|| unsafe { sym::<GdkEventGetButton>(gtk, "gdk_event_get_button") });

        // Widget helpers
        let gtk_widget_set_hexpand = unsafe { sym::<GtkWidgetSetHexpand>(gtk, "gtk_widget_set_hexpand") };
        let gtk_widget_set_vexpand = unsafe { sym::<GtkWidgetSetVexpand>(gtk, "gtk_widget_set_vexpand") };
        let gtk_widget_get_hexpand = unsafe { sym::<GtkWidgetGetHexpand>(gtk, "gtk_widget_get_hexpand") };
        let gtk_widget_get_vexpand = unsafe { sym::<GtkWidgetGetVexpand>(gtk, "gtk_widget_get_vexpand") };

        // GtkEditable (GTK4, replaces gtk_entry_get_text/set_text)
        let gtk_editable_get_text = unsafe { sym::<GtkEditableGetText>(gtk, "gtk_editable_get_text") };
        let gtk_editable_set_text = unsafe { sym::<GtkEditableSetText>(gtk, "gtk_editable_set_text") };
        let gtk_editable_set_position = unsafe { sym::<GtkEditableSetPosition>(gtk, "gtk_editable_set_position") };
        let gtk_editable_select_region = unsafe { sym::<GtkEditableSelectRegion>(gtk, "gtk_editable_select_region") };

        // GtkWidget parent handling
        let gtk_widget_unparent = unsafe { sym::<GtkWidgetUnparent>(gtk, "gtk_widget_unparent") };
        let gtk_widget_get_parent = unsafe { sym::<GtkWidgetGetParent>(gtk, "gtk_widget_get_parent") };

        // GtkWidget tree traversal
        let gtk_widget_get_first_child = unsafe { sym::<GtkWidgetGetFirstChild>(gtk, "gtk_widget_get_first_child") };
        let gtk_widget_get_next_sibling = unsafe { sym::<GtkWidgetGetNextSibling>(gtk, "gtk_widget_get_next_sibling") };

        // GtkPopoverMenuBarItem
        let gtk_popover_menu_bar_item_get_popover = unsafe { sym::<GtkPopoverMenuBarItemGetPopover>(gtk, "gtk_popover_menu_bar_item_get_popover") };
        // GtkPopover
        let gtk_popover_get_child = unsafe { sym::<GtkPopoverGetChild>(gtk, "gtk_popover_get_child") };
        let gtk_popover_new = unsafe { sym::<GtkPopoverNew>(gtk, "gtk_popover_new") };
        let gtk_popover_set_child = unsafe { sym::<GtkPopoverSetChild>(gtk, "gtk_popover_set_child") };

        // GtkMenuButton — manual menu bar construction
        let gtk_menu_button_new = unsafe { sym::<GtkMenuButtonNew>(gtk, "gtk_menu_button_new") };
        let gtk_menu_button_set_popover = unsafe { sym::<GtkMenuButtonSetPopover>(gtk, "gtk_menu_button_set_popover") };
        let gtk_menu_button_set_label = unsafe { sym::<GtkMenuButtonSetLabel>(gtk, "gtk_menu_button_set_label") };
                let gtk_menu_button_set_has_frame = unsafe { sym::<GtkMenuButtonSetHasFrame>(gtk, "gtk_menu_button_set_has_frame") };
        let gtk_menu_button_set_primary = unsafe { sym::<GtkMenuButtonSetPrimary>(gtk, "gtk_menu_button_set_primary") };
        let gtk_button_set_has_frame = unsafe { sym::<GtkButtonSetHasFrame>(gtk, "gtk_button_set_has_frame") };
        let gtk_widget_add_css_class = unsafe { sym::<GtkWidgetAddCssClass>(gtk, "gtk_widget_add_css_class") };
        let gtk_menu_button_get_popover = unsafe { sym::<GtkMenuButtonGetPopover>(gtk, "gtk_menu_button_get_popover") };
        let gtk_widget_set_can_target = unsafe { sym::<GtkWidgetSetCanTarget>(gtk, "gtk_widget_set_can_target") };

        // GtkPopoverMenu — new from model
        let gtk_popover_menu_new_from_model = unsafe { sym::<GtkPopoverMenuNewFromModel>(gtk, "gtk_popover_menu_new_from_model") };

        // GtkMenuButton
        let gtk_menu_button_get_label = unsafe { sym::<GtkMenuButtonGetLabel>(gtk, "gtk_menu_button_get_label") };
        let gtk_menu_button_set_active = unsafe { sym::<GtkMenuButtonSetActive>(gtk, "gtk_menu_button_set_active") };

        // GtkWindow default size
        let gtk_window_set_default_size = unsafe { sym::<GtkWindowSetDefaultSize>(gtk, "gtk_window_set_default_size") };

        Ok(Symbols {
            g_main_loop_new, g_main_loop_run, g_main_loop_quit,
            g_object_ref, g_object_unref, g_object_ref_sink, g_signal_connect_data, g_signal_connect,
            gtk_window_new, gtk_window_set_title, gtk_button_new_with_label, gtk_bin_get_child, gtk_label_new, gtk_label_set_text,
            gtk_box_new, gtk_box_append, gtk_box_pack_start, gtk_container_add, gtk_container_remove, gtk_window_set_child,
            gtk_widget_show_all, gtk_widget_show, gtk_widget_hide, gtk_widget_queue_resize, gtk_window_present,
            gtk_grid_new, gtk_grid_attach, gtk_entry_new, gtk_entry_set_text, gtk_entry_get_text,
            gtk_entry_set_width_chars, gtk_entry_set_placeholder_text, gtk_widget_set_size_request, gtk_entry_set_has_frame,            gtk_label_set_markup, gtk_widget_set_visible, gtk_widget_grab_focus,
            gtk_widget_has_focus, gtk_widget_is_focus, gtk_window_get_focus,
            gtk_widget_get_style_context, gtk_style_context_add_class, gtk_style_context_remove_class,
            gtk_css_provider_new, gtk_css_provider_load_from_data, gtk_style_context_add_provider,
            gtk_overlay_new, gtk_overlay_add_overlay, gtk_overlay_set_overlay_pass_through, gtk_overlay_set_child,
            gtk_widget_set_margin_start, gtk_widget_set_margin_top, gtk_widget_add_events,
            gtk_widget_activate, gtk_widget_get_visible, gtk_widget_activate_action,
            gtk_init,
            g_signal_emit_by_name,
            gtk_label_get_text,
            gtk_entry_set_editable,
            gtk_drawing_area_new,
            g_idle_add, g_timeout_add,
            pango_layout_new, pango_layout_set_text, pango_layout_get_size,
            cairo_create, cairo_font_face_destroy,
            cairo_move_to, cairo_set_source_rgb, cairo_set_source_rgba, cairo_rectangle, cairo_fill, cairo_stroke, cairo_set_line_width, cairo_select_font_face, cairo_set_font_size, cairo_show_text,
            gtk_widget_queue_draw, gtk_widget_queue_draw_area,
            gtk_widget_set_can_focus,
            gtk_file_chooser_native_new, gtk_native_dialog_run, gtk_file_chooser_get_filename, gtk_widget_destroy, gtk_window_close, g_free, gdk_display_get_default, gdk_screen_get_default, gtk_style_context_add_provider_for_display, gtk_style_context_add_provider_for_screen, gdk_event_get_keyval, gdk_event_get_state, gdk_keyval_from_name,
            gtk_application_new, g_application_run, g_application_register, g_simple_action_new, g_action_map_add_action, g_action_group_activate_action, g_action_map_lookup_action, g_action_activate,
            g_menu_new, g_menu_append, g_application_set_app_menu, g_application_set_menubar, g_menu_append_submenu, gtk_popover_menu_bar_new_from_model, gtk_menu_bar_new, gtk_menu_new, gtk_menu_item_new_with_label, gtk_menu_item_new_with_mnemonic, gtk_menu_shell_append, gtk_menu_item_set_submenu, gtk_window_set_application, gtk_widget_insert_action_group, gtk_actionable_set_detailed_action_name, g_menu_model_get_n_items, g_menu_model_get_item_attribute_value, g_menu_model_get_item_link, g_variant_get_string, g_variant_unref,
            gtk_label_set_xalign,
            gtk_event_controller_key_new,
            gtk_event_controller_focus_new,
            gtk_widget_add_controller,
            gtk_event_controller_set_propagation_phase,
            gtk_widget_set_halign,
            gtk_widget_set_valign,
            gtk_widget_get_allocated_width,
            gtk_widget_get_allocated_height,
            gtk_widget_get_mapped,
            gtk_widget_get_display,
            gdk_display_sync,
            gtk_widget_get_frame_clock,
            gdk_frame_clock_request_phase,
            gdk_frame_clock_begin_updating,
            gdk_frame_clock_end_updating,
            gtk_gesture_click_new,
            gtk_scrolled_window_new,
            gtk_scrolled_window_set_policy,
            gtk_scrolled_window_set_child,
            gtk_scrolled_window_get_vadjustment,
            gtk_scrolled_window_get_hadjustment,
            gtk_adjustment_get_value,
            gtk_adjustment_get_upper,
            gtk_adjustment_get_page_size,
            gtk_dialog_new, gtk_dialog_add_button, gtk_dialog_get_content_area, gtk_dialog_run,
            gtk_combo_box_text_new, gtk_combo_box_text_append_text, gtk_combo_box_text_get_active_text, gtk_combo_box_set_active, gtk_combo_box_get_active,
            gtk_drop_down_new, gtk_drop_down_set_selected, gtk_drop_down_get_selected, gtk_string_list_new,
            gtk_check_button_new_with_label, gtk_check_button_get_active, gtk_check_button_set_active, gtk_check_button_set_group, gtk_toggle_button_get_active, gtk_toggle_button_set_active,
            gtk_radio_button_new_with_label,
            gtk_text_view_new, gtk_text_buffer_new, gtk_text_view_get_buffer, gtk_text_buffer_set_text, gtk_text_buffer_get_text, gtk_text_buffer_get_start_iter, gtk_text_buffer_get_end_iter, gtk_text_iter_copy, gtk_text_iter_free, gtk_text_view_set_wrap_mode, gtk_text_buffer_insert, gtk_text_buffer_insert_markup, gtk_text_buffer_copy_clipboard, gtk_widget_get_clipboard, gtk_text_view_set_editable, gtk_text_view_get_editable, gtk_widget_get_can_focus,
            gtk_text_view_window_to_buffer_coords, gtk_text_view_get_iter_at_location, gtk_text_iter_get_line, gtk_text_iter_get_line_offset, gtk_text_buffer_get_has_selection, gdk_event_get_button,
            gtk_widget_set_hexpand, gtk_widget_set_vexpand,
            gtk_widget_get_hexpand, gtk_widget_get_vexpand,
            gtk_editable_get_text, gtk_editable_set_text, gtk_editable_set_position, gtk_editable_select_region,
            gtk_widget_unparent, gtk_widget_get_parent,
            gtk_widget_get_first_child, gtk_widget_get_next_sibling,
            gtk_popover_menu_bar_item_get_popover, gtk_popover_get_child, gtk_popover_new, gtk_popover_set_child,
            gtk_menu_button_get_label, gtk_menu_button_set_active,
            gtk_menu_button_new, gtk_menu_button_set_popover, gtk_menu_button_set_label,
            gtk_menu_button_set_has_frame, gtk_menu_button_set_primary,
            gtk_button_set_has_frame, gtk_widget_add_css_class,
            gtk_menu_button_get_popover, gtk_widget_set_can_target,
            gtk_popover_menu_new_from_model,
            gtk_window_set_default_size,
            gtk_drawing_area_set_draw_func, gtk_drawing_area_set_content_width, gtk_drawing_area_set_content_height,
            gtk_native_get_surface, gdk_surface_create_cairo_context, gdk_surface_get_width, gdk_surface_get_height,
            gdk_surface_begin_draw_frame, gdk_surface_end_draw_frame, gdk_draw_context_get_cairo_context,
            cairo_text_extents, cairo_save, cairo_restore, cairo_clip, cairo_line_to, cairo_paint, cairo_destroy,
        })
    }
}
