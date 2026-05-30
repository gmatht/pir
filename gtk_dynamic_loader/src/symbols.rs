use crate::error::Error;
use libloading::os::unix::Library;
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
pub type GtkLabelNew = unsafe extern "C" fn(str: *const i8) -> *mut c_void;
pub type GtkLabelSetText = unsafe extern "C" fn(label: *mut c_void, str: *const i8);
pub type GtkLabelGetText = unsafe extern "C" fn(label: *mut c_void) -> *const i8;
pub type GtkBoxNew = unsafe extern "C" fn(orientation: i32, spacing: i32) -> *mut c_void;

pub type GtkBoxAppend = unsafe extern "C" fn(box_: *mut c_void, child: *mut c_void);
pub type GtkBoxPackStart = unsafe extern "C" fn(box_: *mut c_void, child: *mut c_void, expand: i32, fill: i32, padding: u32);
pub type GtkContainerAdd = unsafe extern "C" fn(container: *mut c_void, widget: *mut c_void);
pub type GtkWindowSetChild = unsafe extern "C" fn(window: *mut c_void, child: *mut c_void);
pub type GtkWidgetShowAll = unsafe extern "C" fn(widget: *mut c_void);
pub type GtkWindowPresent = unsafe extern "C" fn(window: *mut c_void);
pub type GtkInit = unsafe extern "C" fn(argc: *mut libc::c_int, argv: *mut *mut *mut libc::c_char);
pub type GtkLabelSetMarkup = unsafe extern "C" fn(label: *mut c_void, markup: *const i8);
pub type GtkWidgetSetVisible = unsafe extern "C" fn(widget: *mut c_void, visible: i32);
pub type GtkWidgetGrabFocus = unsafe extern "C" fn(widget: *mut c_void);
pub type GtkWidgetGetStyleContext = unsafe extern "C" fn(widget: *mut c_void) -> *mut c_void;
pub type GtkStyleContextAddClass = unsafe extern "C" fn(context: *mut c_void, class_name: *const i8);
pub type GtkStyleContextRemoveClass = unsafe extern "C" fn(context: *mut c_void, class_name: *const i8);
pub type GtkCssProviderNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkCssProviderLoadFromData = unsafe extern "C" fn(provider: *mut c_void, data: *const i8, length: isize, error: *mut *mut c_void) -> i32;
pub type GtkStyleContextAddProvider = unsafe extern "C" fn(context: *mut c_void, provider: *mut c_void, priority: u32);
// GApplication / GMenu / Actions (gio)
pub type GtkApplicationNew = unsafe extern "C" fn(application_id: *const i8, flags: u32) -> *mut c_void;
pub type GApplicationRun = unsafe extern "C" fn(application: *mut c_void, argc: i32, argv: *mut *mut i8) -> i32;
pub type GSimpleActionNew = unsafe extern "C" fn(name: *const i8, parameter_type: *mut c_void) -> *mut c_void;
pub type GActionMapAddAction = unsafe extern "C" fn(map: *mut c_void, action: *mut c_void);
pub type GMenuNew = unsafe extern "C" fn() -> *mut c_void;
pub type GMenuAppend = unsafe extern "C" fn(menu: *mut c_void, label: *const i8, detailed_action: *const i8);
pub type GApplicationSetAppMenu = unsafe extern "C" fn(application: *mut c_void, menu: *mut c_void);
pub type GtkWindowSetApplication = unsafe extern "C" fn(window: *mut c_void, application: *mut c_void);

// File chooser / native dialog
pub type GtkFileChooserNativeNew = unsafe extern "C" fn(title: *const i8, parent: *mut c_void, action: i32, accept_label: *const i8, cancel_label: *const i8) -> *mut c_void;
pub type GtkNativeDialogRun = unsafe extern "C" fn(native: *mut c_void) -> i32;
pub type GtkFileChooserGetFilename = unsafe extern "C" fn(chooser: *mut c_void) -> *const i8;
pub type GtkWidgetDestroy = unsafe extern "C" fn(widget: *mut c_void);
pub type GFree = unsafe extern "C" fn(ptr: *mut c_void);
// gdk event helpers
pub type GdkEventGetKeyval = unsafe extern "C" fn(event: *mut c_void) -> u32;
pub type GdkKeyvalFromName = unsafe extern "C" fn(name: *const i8) -> u32;
pub type GtkLabelSetXalign = unsafe extern "C" fn(label: *mut c_void, xalign: f32);
pub type GtkEventControllerKeyNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkWidgetAddController = unsafe extern "C" fn(widget: *mut c_void, controller: *mut c_void);

// Grid/Entry related
pub type GtkGridNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkGridAttach = unsafe extern "C" fn(grid: *mut c_void, child: *mut c_void, left: i32, top: i32, width: i32, height: i32);
pub type GtkEntryNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkEntrySetText = unsafe extern "C" fn(entry: *mut c_void, text: *const i8);
pub type GtkEntryGetText = unsafe extern "C" fn(entry: *mut c_void) -> *const i8;
pub type GtkEntrySetWidthChars = unsafe extern "C" fn(entry: *mut c_void, n_chars: i32);
pub type GtkWidgetSetSizeRequest = unsafe extern "C" fn(widget: *mut c_void, width: i32, height: i32);
pub type GtkEntrySetHasFrame = unsafe extern "C" fn(entry: *mut c_void, has_frame: i32);
pub type GtkEntrySetEditable = unsafe extern "C" fn(entry: *mut c_void, editable: i32);
pub type GtkDrawingAreaNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkOverlayNew = unsafe extern "C" fn() -> *mut c_void;
pub type GtkOverlayAddOverlay = unsafe extern "C" fn(overlay: *mut c_void, widget: *mut c_void);
pub type GtkOverlaySetOverlayPassThrough = unsafe extern "C" fn(overlay: *mut c_void, widget: *mut c_void, pass_through: i32);
pub type GtkWidgetSetMarginStart = unsafe extern "C" fn(widget: *mut c_void, margin: i32);
pub type GtkWidgetSetMarginTop = unsafe extern "C" fn(widget: *mut c_void, margin: i32);

pub struct Symbols {
    // glib
    pub g_main_loop_new: Option<GMainLoopNew>,
    pub g_main_loop_run: Option<GMainLoopRun>,
    pub g_main_loop_quit: Option<GMainLoopQuit>,

    // gobject
    pub g_object_ref: Option<GObjectRef>,
    pub g_object_unref: Option<GObjectUnref>,
    pub g_object_ref_sink: Option<GObjectRefSink>,
    pub g_signal_connect_data: Option<GSignalConnectData>,
    pub g_signal_connect: Option<GSignalConnect>,

    // gtk
    pub gtk_window_new: Option<GtkWindowNew>,
    pub gtk_window_set_title: Option<GtkWindowSetTitle>,
    pub gtk_button_new_with_label: Option<GtkButtonNewWithLabel>,
    pub gtk_label_new: Option<GtkLabelNew>,
    pub gtk_label_set_text: Option<GtkLabelSetText>,
    pub gtk_label_get_text: Option<GtkLabelGetText>,
    pub gtk_box_new: Option<GtkBoxNew>,
    pub gtk_box_append: Option<GtkBoxAppend>,
    pub gtk_box_pack_start: Option<GtkBoxPackStart>,
    pub gtk_container_add: Option<GtkContainerAdd>,
    pub gtk_window_set_child: Option<GtkWindowSetChild>,
    pub gtk_widget_show_all: Option<GtkWidgetShowAll>,
    pub gtk_window_present: Option<GtkWindowPresent>,
    pub gtk_window_set_application: Option<GtkWindowSetApplication>,
    pub gtk_grid_new: Option<GtkGridNew>,
    pub gtk_grid_attach: Option<GtkGridAttach>,
    pub gtk_entry_new: Option<GtkEntryNew>,
    pub gtk_entry_set_text: Option<GtkEntrySetText>,
    pub gtk_entry_get_text: Option<GtkEntryGetText>,
    pub gtk_entry_set_width_chars: Option<GtkEntrySetWidthChars>,
    pub gtk_widget_set_size_request: Option<GtkWidgetSetSizeRequest>,
    pub gtk_entry_set_has_frame: Option<GtkEntrySetHasFrame>,
    pub gtk_entry_set_editable: Option<GtkEntrySetEditable>,
    pub gtk_drawing_area_new: Option<GtkDrawingAreaNew>,
    pub gtk_label_set_markup: Option<GtkLabelSetMarkup>,
    pub gtk_widget_set_visible: Option<GtkWidgetSetVisible>,
    pub gtk_widget_grab_focus: Option<GtkWidgetGrabFocus>,
    pub gtk_widget_get_style_context: Option<GtkWidgetGetStyleContext>,
    pub gtk_style_context_add_class: Option<GtkStyleContextAddClass>,
    pub gtk_style_context_remove_class: Option<GtkStyleContextRemoveClass>,
    pub gtk_css_provider_new: Option<GtkCssProviderNew>,
    pub gtk_css_provider_load_from_data: Option<GtkCssProviderLoadFromData>,
    pub gtk_style_context_add_provider: Option<GtkStyleContextAddProvider>,
    pub gtk_overlay_new: Option<GtkOverlayNew>,
    pub gtk_overlay_add_overlay: Option<GtkOverlayAddOverlay>,
    pub gtk_overlay_set_overlay_pass_through: Option<GtkOverlaySetOverlayPassThrough>,
    pub gtk_widget_set_margin_start: Option<GtkWidgetSetMarginStart>,
    pub gtk_widget_set_margin_top: Option<GtkWidgetSetMarginTop>,
    pub gtk_file_chooser_native_new: Option<GtkFileChooserNativeNew>,
    pub gtk_native_dialog_run: Option<GtkNativeDialogRun>,
    pub gtk_file_chooser_get_filename: Option<GtkFileChooserGetFilename>,
    pub gtk_widget_destroy: Option<GtkWidgetDestroy>,
    pub g_free: Option<GFree>,
    pub gdk_event_get_keyval: Option<GdkEventGetKeyval>,
    pub gdk_keyval_from_name: Option<GdkKeyvalFromName>,
    // application/menu/action
    pub gtk_application_new: Option<GtkApplicationNew>,
    pub g_application_run: Option<GApplicationRun>,
    pub g_simple_action_new: Option<GSimpleActionNew>,
    pub g_action_map_add_action: Option<GActionMapAddAction>,
    pub g_menu_new: Option<GMenuNew>,
    pub g_menu_append: Option<GMenuAppend>,
    pub g_application_set_app_menu: Option<GApplicationSetAppMenu>,
    pub gtk_init: Option<GtkInit>,
    pub g_signal_emit_by_name: Option<GSignalEmitByName>,
    pub g_idle_add: Option<unsafe extern "C" fn(func: Option<unsafe extern "C" fn(*mut c_void) -> i32>, data: *mut c_void) -> u32>,
    // pango (optional) - functions will be looked up from libpango if loaded
    pub pango_layout_new: Option<unsafe extern "C" fn(context: *mut c_void) -> *mut c_void>,
    pub pango_layout_set_text: Option<unsafe extern "C" fn(layout: *mut c_void, text: *const i8, len: i32)>,
    pub pango_layout_get_size: Option<unsafe extern "C" fn(layout: *mut c_void, width: *mut i32, height: *mut i32)>,
    // cairo surface/context helpers
    pub cairo_create: Option<unsafe extern "C" fn(surface: *mut c_void) -> *mut c_void>,
    pub cairo_font_face_destroy: Option<unsafe extern "C" fn(face: *mut c_void)>,
    // basic cairo drawing operations
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
    pub gtk_label_set_xalign: Option<GtkLabelSetXalign>,
    pub gtk_event_controller_key_new: Option<GtkEventControllerKeyNew>,
    pub gtk_widget_add_controller: Option<GtkWidgetAddController>,
}

impl Symbols {
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
        let gtk_label_new = unsafe { sym::<GtkLabelNew>(gtk, "gtk_label_new") };
        let gtk_label_set_text = unsafe { sym::<GtkLabelSetText>(gtk, "gtk_label_set_text") };
        let gtk_label_get_text = unsafe { sym::<GtkLabelGetText>(gtk, "gtk_label_get_text") };
        let gtk_box_new = unsafe { sym::<GtkBoxNew>(gtk, "gtk_box_new") };
        let gtk_box_append = unsafe { sym::<GtkBoxAppend>(gtk, "gtk_box_append") };
        let gtk_box_pack_start = unsafe { sym::<GtkBoxPackStart>(gtk, "gtk_box_pack_start") };
        let gtk_container_add = unsafe { sym::<GtkContainerAdd>(gtk, "gtk_container_add") };
        let gtk_window_set_child = unsafe { sym::<GtkWindowSetChild>(gtk, "gtk_window_set_child") };
        let gtk_widget_show_all = unsafe { sym::<GtkWidgetShowAll>(gtk, "gtk_widget_show_all") };
        let gtk_window_present = unsafe { sym::<GtkWindowPresent>(gtk, "gtk_window_present") };
        let _gtk_window_set_application = unsafe { sym::<GtkWindowSetApplication>(gtk, "gtk_window_set_application") };
        let gtk_file_chooser_native_new = open_sym_try!(libs, "libgio", GtkFileChooserNativeNew, "gtk_file_chooser_native_new").or_else(|| unsafe { sym::<GtkFileChooserNativeNew>(gtk, "gtk_file_chooser_native_new") });
        let gtk_native_dialog_run = open_sym_try!(libs, "libgio", GtkNativeDialogRun, "gtk_native_dialog_run").or_else(|| unsafe { sym::<GtkNativeDialogRun>(gtk, "gtk_native_dialog_run") });
        let gtk_file_chooser_get_filename = open_sym_try!(libs, "libgio", GtkFileChooserGetFilename, "gtk_file_chooser_get_filename").or_else(|| unsafe { sym::<GtkFileChooserGetFilename>(gtk, "gtk_file_chooser_get_filename") });
        let gtk_widget_destroy = unsafe { sym::<GtkWidgetDestroy>(gtk, "gtk_widget_destroy") };
        let g_free = unsafe { sym::<GFree>(glib, "g_free") };
        let gdk_event_get_keyval = open_sym_try!(libs, "libgdk", GdkEventGetKeyval, "gdk_event_get_keyval").or_else(|| unsafe { sym::<GdkEventGetKeyval>(gtk, "gdk_event_get_keyval") });
        let gdk_keyval_from_name = open_sym_try!(libs, "libgdk", GdkKeyvalFromName, "gdk_keyval_from_name").or_else(|| unsafe { sym::<GdkKeyvalFromName>(gtk, "gdk_keyval_from_name") });
        let gtk_grid_new = unsafe { sym::<GtkGridNew>(gtk, "gtk_grid_new") };
        let gtk_grid_attach = unsafe { sym::<GtkGridAttach>(gtk, "gtk_grid_attach") };
        let gtk_entry_new = unsafe { sym::<GtkEntryNew>(gtk, "gtk_entry_new") };
        let gtk_entry_set_text = unsafe { sym::<GtkEntrySetText>(gtk, "gtk_entry_set_text") };
        let gtk_entry_get_text = unsafe { sym::<GtkEntryGetText>(gtk, "gtk_entry_get_text") };
        let gtk_entry_set_width_chars = unsafe { sym::<GtkEntrySetWidthChars>(gtk, "gtk_entry_set_width_chars") };
        let gtk_widget_set_size_request = unsafe { sym::<GtkWidgetSetSizeRequest>(gtk, "gtk_widget_set_size_request") };
        let gtk_entry_set_has_frame = unsafe { sym::<GtkEntrySetHasFrame>(gtk, "gtk_entry_set_has_frame") };
        let gtk_entry_set_editable = unsafe { sym::<GtkEntrySetEditable>(gtk, "gtk_editable_set_editable") };
        let gtk_drawing_area_new = unsafe { sym::<GtkDrawingAreaNew>(gtk, "gtk_drawing_area_new") };
        let gtk_label_set_markup = unsafe { sym::<GtkLabelSetMarkup>(gtk, "gtk_label_set_markup") };
        let gtk_widget_set_visible = unsafe { sym::<GtkWidgetSetVisible>(gtk, "gtk_widget_set_visible") };
        let gtk_widget_grab_focus = unsafe { sym::<GtkWidgetGrabFocus>(gtk, "gtk_widget_grab_focus") };
        let gtk_widget_get_style_context = unsafe { sym::<GtkWidgetGetStyleContext>(gtk, "gtk_widget_get_style_context") };
        let gtk_style_context_add_class = unsafe { sym::<GtkStyleContextAddClass>(gtk, "gtk_style_context_add_class") };
        let gtk_style_context_remove_class = unsafe { sym::<GtkStyleContextRemoveClass>(gtk, "gtk_style_context_remove_class") };
        let gtk_css_provider_new = unsafe { sym::<GtkCssProviderNew>(gtk, "gtk_css_provider_new") };
        let gtk_css_provider_load_from_data = unsafe { sym::<GtkCssProviderLoadFromData>(gtk, "gtk_css_provider_load_from_data") };
        let gtk_style_context_add_provider = unsafe { sym::<GtkStyleContextAddProvider>(gtk, "gtk_style_context_add_provider") };
        let gtk_overlay_new = unsafe { sym::<GtkOverlayNew>(gtk, "gtk_overlay_new") };
        let gtk_overlay_add_overlay = unsafe { sym::<GtkOverlayAddOverlay>(gtk, "gtk_overlay_add_overlay") };
        let gtk_overlay_set_overlay_pass_through = unsafe { sym::<GtkOverlaySetOverlayPassThrough>(gtk, "gtk_overlay_set_overlay_pass_through") };
        let gtk_widget_set_margin_start = unsafe { sym::<GtkWidgetSetMarginStart>(gtk, "gtk_widget_set_margin_start") };
        let gtk_widget_set_margin_top = unsafe { sym::<GtkWidgetSetMarginTop>(gtk, "gtk_widget_set_margin_top") };
        let gtk_init = unsafe { sym::<GtkInit>(gtk, "gtk_init") };
        let g_signal_emit_by_name = unsafe { sym::<GSignalEmitByName>(gobject, "g_signal_emit_by_name") };
        let g_idle_add = unsafe { sym::<unsafe extern "C" fn(func: Option<unsafe extern "C" fn(*mut c_void) -> i32>, data: *mut c_void) -> u32>(glib, "g_idle_add") };
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
        let gtk_widget_queue_draw = unsafe { sym::<unsafe extern "C" fn(*mut c_void)>(gtk, "gtk_widget_queue_draw") };
        let gtk_label_set_xalign = unsafe { sym::<GtkLabelSetXalign>(gtk, "gtk_label_set_xalign") };
        let gtk_event_controller_key_new = unsafe { sym::<GtkEventControllerKeyNew>(gtk, "gtk_event_controller_key_new") };
        let gtk_widget_add_controller = unsafe { sym::<GtkWidgetAddController>(gtk, "gtk_widget_add_controller") };

        // application/menu/action symbols (try glib/gio)
        // try gio first then glib for the app/menu symbols
        let gtk_application_new = open_sym_try!(libs, "libgio", GtkApplicationNew, "g_application_new").or_else(|| unsafe { sym::<GtkApplicationNew>(glib, "g_application_new") });
        let g_application_run = open_sym_try!(libs, "libgio", GApplicationRun, "g_application_run").or_else(|| unsafe { sym::<GApplicationRun>(glib, "g_application_run") });
        let g_simple_action_new = open_sym_try!(libs, "libgio", GSimpleActionNew, "g_simple_action_new").or_else(|| unsafe { sym::<GSimpleActionNew>(glib, "g_simple_action_new") });
        let g_action_map_add_action = open_sym_try!(libs, "libgio", GActionMapAddAction, "g_action_map_add_action").or_else(|| unsafe { sym::<GActionMapAddAction>(glib, "g_action_map_add_action") });
        let g_menu_new = open_sym_try!(libs, "libgio", GMenuNew, "g_menu_new").or_else(|| unsafe { sym::<GMenuNew>(glib, "g_menu_new") });
        let g_menu_append = open_sym_try!(libs, "libgio", GMenuAppend, "g_menu_append").or_else(|| unsafe { sym::<GMenuAppend>(glib, "g_menu_append") });
        let g_application_set_app_menu = open_sym_try!(libs, "libgio", GApplicationSetAppMenu, "g_application_set_app_menu").or_else(|| unsafe { sym::<GApplicationSetAppMenu>(glib, "g_application_set_app_menu") });
        let gtk_window_set_application = unsafe { sym::<GtkWindowSetApplication>(gtk, "gtk_window_set_application") };

        Ok(Symbols {
            g_main_loop_new, g_main_loop_run, g_main_loop_quit,
            g_object_ref, g_object_unref, g_object_ref_sink, g_signal_connect_data, g_signal_connect,
            gtk_window_new, gtk_window_set_title, gtk_button_new_with_label, gtk_label_new, gtk_label_set_text,
            gtk_box_new, gtk_box_append, gtk_box_pack_start, gtk_container_add, gtk_window_set_child,
            gtk_widget_show_all, gtk_window_present,
            gtk_grid_new, gtk_grid_attach, gtk_entry_new, gtk_entry_set_text, gtk_entry_get_text,
            gtk_entry_set_width_chars, gtk_widget_set_size_request, gtk_entry_set_has_frame,
            gtk_label_set_markup, gtk_widget_set_visible, gtk_widget_grab_focus,
            gtk_widget_get_style_context, gtk_style_context_add_class, gtk_style_context_remove_class,
            gtk_css_provider_new, gtk_css_provider_load_from_data, gtk_style_context_add_provider,
            gtk_overlay_new, gtk_overlay_add_overlay, gtk_overlay_set_overlay_pass_through,
            gtk_widget_set_margin_start, gtk_widget_set_margin_top,
            gtk_init,
            g_signal_emit_by_name,
            gtk_label_get_text,
            gtk_entry_set_editable,
            gtk_drawing_area_new,
            g_idle_add,
            pango_layout_new, pango_layout_set_text, pango_layout_get_size,
            cairo_create, cairo_font_face_destroy,
            cairo_move_to, cairo_set_source_rgb, cairo_set_source_rgba, cairo_rectangle, cairo_fill, cairo_stroke, cairo_set_line_width, cairo_select_font_face, cairo_set_font_size, cairo_show_text,
            gtk_widget_queue_draw,
            gtk_file_chooser_native_new, gtk_native_dialog_run, gtk_file_chooser_get_filename, gtk_widget_destroy, g_free, gdk_event_get_keyval, gdk_keyval_from_name,
            gtk_application_new, g_application_run, g_simple_action_new, g_action_map_add_action,
            g_menu_new, g_menu_append, g_application_set_app_menu, gtk_window_set_application,
            gtk_label_set_xalign,
            gtk_event_controller_key_new,
            gtk_widget_add_controller,
        })
    }
}
