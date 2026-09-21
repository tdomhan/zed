//! iOS Window implementation using UIWindow and UIViewController.
//!
//! iOS windows are fundamentally different from desktop windows:
//! - Always fullscreen (or split-screen on iPad)
//! - No title bar or window chrome
//! - Touch-based input
//! - Safe area insets for notch/home indicator
//!
//! The window is backed by a UIWindow containing a UIViewController
//! whose view hosts the Metal rendering layer.

use super::{
    display::IosDisplay,
    events::*,
    text_input,
    text_input::{
        UI_KEY_MODIFIER_ALTERNATE, UI_KEY_MODIFIER_COMMAND, UI_KEY_MODIFIER_CONTROL,
        UI_KEY_MODIFIER_SHIFT, create_text_position, create_text_range, get_position_index,
        get_range_indices, set_range_indices,
    },
};
use crate::metal_renderer;
use anyhow::Result;
use block::ConcreteBlock;
use core_graphics::{
    base::CGFloat,
    geometry::{CGRect, CGSize},
};
use gpui::{
    AnyWindowHandle, Bounds, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler,
    PlatformTextAutocapitalization, PlatformTextInputTrait, PlatformTextInputTraits,
    PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions, Scene, ScrollDelta,
    ScrollWheelEvent, SelectableTextHitRegion, SelectionMenuPresentation, Size, TouchPhase,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams,
    px, should_auto_request_soft_keyboard, should_show_keyboard_accessory, size,
};
use objc::{
    Encode, Encoding, class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Sel, YES},
    sel, sel_impl,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, UiKitDisplayHandle, UiKitWindowHandle};
use smallvec::SmallVec;
use std::{
    cell::{Cell, RefCell},
    ffi::{CString, c_void},
    ops::Range,
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
const GPUI_VIEW_IVAR: &str = "gpui_view";
const GPUI_WINDOW_IVAR: &str = "gpui_window_ptr";

const FLING_THRESHOLD: f32 = 50.0;
const TEXT_INTERACTION_NONE: i8 = -1;
const TEXT_INTERACTION_NONEDITABLE: i8 = 0;
const TEXT_INTERACTION_EDITABLE: i8 = 1;
// GPUI still owns the editable surface's long-press action. UIKit separately
// asks whether UITextInteraction may begin, so keep editable native selection
// gated to a matured single-touch press instead of ordinary taps or double taps.
const EDITABLE_NATIVE_SELECTION_LONG_PRESS_MIN_DURATION: Duration = Duration::from_millis(350);
// UIKit handle touches often begin just outside the rects returned by GPUI.
// Keep those touches in the native-selection path instead of clearing selection.
const NATIVE_SELECTION_HANDLE_HIT_SLOP: f32 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditMenuActionPolicy {
    CopySelection,
    DisableNativeMenu,
    DelegateToSystem,
}

fn edit_menu_action_policy(
    interaction_mode: i8,
    input_native_selection_enabled: bool,
    is_copy_action: bool,
    menu_presentation: SelectionMenuPresentation,
) -> EditMenuActionPolicy {
    if menu_presentation == SelectionMenuPresentation::CustomActionsOnly && is_copy_action {
        EditMenuActionPolicy::DisableNativeMenu
    } else if handles_native_touch_selection(interaction_mode, input_native_selection_enabled)
        && is_copy_action
    {
        EditMenuActionPolicy::CopySelection
    } else if interaction_mode == TEXT_INTERACTION_EDITABLE && !input_native_selection_enabled {
        EditMenuActionPolicy::DisableNativeMenu
    } else {
        EditMenuActionPolicy::DelegateToSystem
    }
}

/// Whether a touch that begins away from the active selection should collapse
/// it.
///
/// Dismissing the selection is all this decides. The touch itself is still
/// delivered to GPUI, because a tap that lands on a button has to activate it
/// and a drag has to scroll, exactly as they do when no text is selected —
/// UIKit's own text views behave the same way, moving the caret on the tap that
/// dismisses a selection rather than swallowing it.
fn should_dismiss_selection_for_touch(
    had_selection: bool,
    hit_text: bool,
    hit_selection_area: bool,
) -> bool {
    had_selection && !hit_text && !hit_selection_area
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextInputResponderAction {
    None,
    ShowKeyboard,
    ResignActiveResponder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextInputRefreshPlan {
    keyboard_session_requested: bool,
    target_interaction_mode: i8,
    responder_action: TextInputResponderAction,
}

fn text_input_refresh_plan(
    has_input_handler: bool,
    has_selection_handler: bool,
    input_accepts_text_input: bool,
    input_native_selection_enabled: bool,
    _input_uses_manual_focus: bool,
    keyboard_session_requested: bool,
    software_keyboard_visible: bool,
) -> TextInputRefreshPlan {
    let target_interaction_mode = if has_input_handler && input_accepts_text_input {
        TEXT_INTERACTION_EDITABLE
    } else if has_selection_handler {
        TEXT_INTERACTION_NONEDITABLE
    } else {
        TEXT_INTERACTION_NONE
    };

    let should_show_keyboard =
        has_input_handler && input_accepts_text_input && keyboard_session_requested;
    let keyboard_session_requested = if keyboard_session_requested && !has_input_handler {
        true
    } else {
        should_show_keyboard
    };
    let responder_action = if should_show_keyboard {
        if software_keyboard_visible {
            TextInputResponderAction::None
        } else {
            TextInputResponderAction::ShowKeyboard
        }
    } else if keyboard_session_requested && !has_input_handler {
        TextInputResponderAction::None
    } else if target_interaction_mode == TEXT_INTERACTION_NONEDITABLE {
        if software_keyboard_visible {
            TextInputResponderAction::ResignActiveResponder
        } else {
            TextInputResponderAction::None
        }
    } else if target_interaction_mode == TEXT_INTERACTION_EDITABLE && input_native_selection_enabled
    {
        TextInputResponderAction::None
    } else {
        TextInputResponderAction::ResignActiveResponder
    };

    TextInputRefreshPlan {
        keyboard_session_requested,
        target_interaction_mode,
        responder_action,
    }
}

/// Chooses the active text callback route for the current responder state.
///
/// Editable mode routes keyboard and IME callbacks to the input handler. An
/// input handler can also opt into native touch selection while staying editable.
fn active_text_interaction_mode_for_state(
    target_interaction_mode: i8,
    has_selection_handler: bool,
    input_native_selection_enabled: bool,
    is_first_responder: bool,
) -> i8 {
    match target_interaction_mode {
        TEXT_INTERACTION_EDITABLE if is_first_responder => TEXT_INTERACTION_EDITABLE,
        TEXT_INTERACTION_EDITABLE if input_native_selection_enabled => TEXT_INTERACTION_EDITABLE,
        TEXT_INTERACTION_EDITABLE if has_selection_handler => TEXT_INTERACTION_NONEDITABLE,
        TEXT_INTERACTION_EDITABLE => TEXT_INTERACTION_NONE,
        TEXT_INTERACTION_NONEDITABLE => TEXT_INTERACTION_NONEDITABLE,
        _ => TEXT_INTERACTION_NONE,
    }
}

fn should_clear_keyboard_request_when_clearing_input_handler(
    had_input_handler: bool,
    had_callback_input_handler: bool,
) -> bool {
    had_input_handler || had_callback_input_handler
}

/// Read-only selection is allowed to become first responder for UIKit's copy
/// and selection-handle UI, but only real GPUI input handlers should be allowed
/// to attach the system keyboard.
fn should_use_system_keyboard(
    has_input_handler: bool,
    has_callback_input_handler: bool,
    input_accepts_text_input: bool,
    keyboard_session_requested: bool,
) -> bool {
    keyboard_session_requested
        && input_accepts_text_input
        && (has_input_handler || has_callback_input_handler)
}

fn should_use_keyboard_accessory(
    has_input_handler: bool,
    has_callback_input_handler: bool,
    input_accepts_text_input: bool,
    keyboard_session_requested: bool,
    keyboard_accessory_enabled: bool,
) -> bool {
    (has_input_handler || has_callback_input_handler)
        && should_show_keyboard_accessory(
            input_accepts_text_input,
            keyboard_session_requested,
            keyboard_accessory_enabled,
        )
}

fn handles_native_touch_selection(
    interaction_mode: i8,
    input_native_selection_enabled: bool,
) -> bool {
    interaction_mode == TEXT_INTERACTION_NONEDITABLE
        || (interaction_mode == TEXT_INTERACTION_EDITABLE && input_native_selection_enabled)
}

fn should_report_text_input_range_geometry(
    interaction_mode: i8,
    input_native_selection_enabled: bool,
) -> bool {
    interaction_mode == TEXT_INTERACTION_EDITABLE
        || handles_native_touch_selection(interaction_mode, input_native_selection_enabled)
}

fn should_begin_text_interaction(
    interaction_mode: i8,
    input_native_selection_enabled: bool,
    hit_selectable_text: bool,
) -> bool {
    handles_native_touch_selection(interaction_mode, input_native_selection_enabled)
        && hit_selectable_text
}

#[derive(Clone, Debug, PartialEq)]
struct SelectionGeometry {
    range: Range<usize>,
    bounds: Option<Bounds<Pixels>>,
    rects: Vec<Bounds<Pixels>>,
}

fn selection_geometry_contains_interaction_point(
    geometry: &SelectionGeometry,
    point: Point<Pixels>,
) -> bool {
    // UIKit selection handles can begin outside the text rects we report.
    // Treat nearby touches as selection interaction so handle drags are not cleared.
    let slop = px(NATIVE_SELECTION_HANDLE_HIT_SLOP);
    geometry
        .bounds
        .as_ref()
        .is_some_and(|bounds| bounds.dilate(slop).contains(&point))
        || geometry
            .rects
            .iter()
            .any(|rect| rect.dilate(slop).contains(&point))
}

struct TouchFling {
    velocity_x: f32,
    velocity_y: f32,
    last_time: std::time::Instant,
    position: Point<Pixels>,
}

/// NSRange structure for Objective-C interop
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NSRange {
    location: u64,
    length: u64,
}

// Implement Encode for NSRange to allow it to be used in Objective-C method signatures
unsafe impl Encode for NSRange {
    fn encode() -> Encoding {
        // NSRange is a struct with two unsigned longs: {_NSRange=QQ}
        unsafe { Encoding::from_str("{_NSRange=QQ}") }
    }
}

/// Our own CGRect struct for use in Objective-C method signatures (implements Encode)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IOSCGRect {
    origin: IOSCGPoint,
    size: IOSCGSize,
}

impl IOSCGRect {
    fn new(origin: IOSCGPoint, size: IOSCGSize) -> Self {
        Self { origin, size }
    }
}

unsafe impl Encode for IOSCGRect {
    fn encode() -> Encoding {
        // CGRect is a struct with origin (CGPoint) and size (CGSize): {CGRect={CGPoint=dd}{CGSize=dd}}
        unsafe { Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

/// Our own CGPoint struct for use in Objective-C method signatures (implements Encode)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IOSCGPoint {
    x: f64,
    y: f64,
}

impl IOSCGPoint {
    fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

unsafe impl Encode for IOSCGPoint {
    fn encode() -> Encoding {
        // CGPoint is a struct with x and y doubles: {CGPoint=dd}
        unsafe { Encoding::from_str("{CGPoint=dd}") }
    }
}

/// Our own CGSize struct for use in Objective-C method signatures (implements Encode)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IOSCGSize {
    width: f64,
    height: f64,
}

impl IOSCGSize {
    fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

unsafe impl Encode for IOSCGSize {
    fn encode() -> Encoding {
        // CGSize is a struct with width and height doubles: {CGSize=dd}
        unsafe { Encoding::from_str("{CGSize=dd}") }
    }
}

/// NSNotFound constant (max value indicates "not found")
const NS_NOT_FOUND: u64 = u64::MAX;

static METAL_VIEW_CLASS_REGISTERED: std::sync::Once = std::sync::Once::new();
static TEXT_SELECTION_RECT_CLASS_REGISTERED: std::sync::Once = std::sync::Once::new();

/// Global pointer to the UIView displayed above the software keyboard (inputAccessoryView).
/// Set from Obj-C via `gpui_ios_set_keyboard_accessory_view`.
static KEYBOARD_ACCESSORY_VIEW: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static EMPTY_KEYBOARD_INPUT_VIEW: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Whether the iOS software keyboard is currently visible (set via keyboard notifications).
/// `inputAccessoryView` returns nil when this is false, preventing the toolbar from
/// appearing without a software keyboard (e.g. on simulator with hardware keyboard).
static SOFTWARE_KEYBOARD_VISIBLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// Declare NSLog extern once at module level
unsafe extern "C" {
    fn NSLog(format: *mut Object, ...);
}

/// Helper to log to iOS system log via NSLog.
/// This ensures messages show up in `xcrun simctl spawn ... log stream`.
#[allow(unused)]
fn ios_log(message: &str) {
    unsafe {
        // Create NSString from our message
        let ns_string: *mut Object = msg_send![class!(NSString), alloc];
        let format: *mut Object = msg_send![ns_string, initWithUTF8String: message.as_ptr() as *const std::os::raw::c_char];

        NSLog(format);

        // Release the string
        let _: () = msg_send![format, release];
    }
}

/// Helper to log to iOS system log with a C string literal.
/// This is simpler and doesn't require allocation.
fn ios_log_cstr(message: &std::ffi::CStr) {
    unsafe {
        let ns_string: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: message.as_ptr()];
        NSLog(ns_string);
    }
}

/// Helper to log a formatted string to iOS system log.
/// Creates a proper null-terminated C string.
fn ios_log_format(message: &str) {
    let c_string = std::ffi::CString::new(message)
        .unwrap_or_else(|_| std::ffi::CString::new("GPUI iOS: <invalid log message>").unwrap());
    unsafe {
        let ns_string: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: c_string.as_ptr()];
        NSLog(ns_string);
    }
}

fn register_text_selection_rect_class() -> &'static Class {
    TEXT_SELECTION_RECT_CLASS_REGISTERED.call_once(|| {
        let superclass = class!(UITextSelectionRect);
        let mut decl = ClassDecl::new("GPUITextSelectionRect", superclass).unwrap();

        decl.add_ivar::<f64>("rect_x");
        decl.add_ivar::<f64>("rect_y");
        decl.add_ivar::<f64>("rect_width");
        decl.add_ivar::<f64>("rect_height");
        decl.add_ivar::<bool>("contains_start");
        decl.add_ivar::<bool>("contains_end");

        extern "C" fn rect(this: &Object, _sel: Sel) -> IOSCGRect {
            let x: f64 = unsafe { *this.get_ivar("rect_x") };
            let y: f64 = unsafe { *this.get_ivar("rect_y") };
            let width: f64 = unsafe { *this.get_ivar("rect_width") };
            let height: f64 = unsafe { *this.get_ivar("rect_height") };
            IOSCGRect::new(IOSCGPoint::new(x, y), IOSCGSize::new(width, height))
        }

        extern "C" fn writing_direction(_this: &Object, _sel: Sel) -> i64 {
            0
        }

        extern "C" fn contains_start(this: &Object, _sel: Sel) -> BOOL {
            if unsafe { *this.get_ivar("contains_start") } {
                YES
            } else {
                NO
            }
        }

        extern "C" fn contains_end(this: &Object, _sel: Sel) -> BOOL {
            if unsafe { *this.get_ivar("contains_end") } {
                YES
            } else {
                NO
            }
        }

        extern "C" fn is_vertical(_this: &Object, _sel: Sel) -> BOOL {
            NO
        }

        unsafe {
            decl.add_method(sel!(rect), rect as extern "C" fn(&Object, Sel) -> IOSCGRect);
            decl.add_method(
                sel!(writingDirection),
                writing_direction as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(containsStart),
                contains_start as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(containsEnd),
                contains_end as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(isVertical),
                is_vertical as extern "C" fn(&Object, Sel) -> BOOL,
            );
        }

        decl.register();
    });

    class!(GPUITextSelectionRect)
}

fn create_text_selection_rect(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    contains_start: bool,
    contains_end: bool,
) -> *mut Object {
    unsafe {
        let class = register_text_selection_rect_class();
        let rect: *mut Object = msg_send![class, alloc];
        let rect: *mut Object = msg_send![rect, init];
        (*rect).set_ivar("rect_x", x);
        (*rect).set_ivar("rect_y", y);
        (*rect).set_ivar("rect_width", width);
        (*rect).set_ivar("rect_height", height);
        (*rect).set_ivar("contains_start", contains_start);
        (*rect).set_ivar("contains_end", contains_end);
        rect
    }
}

/// Safely access the input handler for a view.
///
/// IMPORTANT: This function takes the handler OUT of the RefCell before
/// executing the callback, then restores it afterward. This matches the
/// macOS pattern (see mac/window.rs:2492-2506) and prevents borrow conflicts
/// when iOS calls multiple UITextInput methods simultaneously during
/// keyboard initialization.
///
/// The pattern is:
/// 1. Borrow the RefCell and take() the handler out (leaving None)
/// 2. Drop the borrow (releasing the RefCell)
/// 3. Execute callback with exclusive access to the handler
/// 4. Re-borrow and restore the handler
///
/// This ensures no borrow is held during callback execution, allowing
/// re-entrant calls to succeed.
fn with_input_handler<F, R>(view: &Object, f: F) -> Option<R>
where
    F: FnOnce(&mut PlatformInputHandler) -> R,
{
    #[derive(Copy, Clone)]
    enum HandlerSlot {
        Input,
        Selection,
    }

    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return None;
        }

        let window = &*(window_ptr as *const IosWindow);

        let take_handler = |slot: HandlerSlot| -> Option<PlatformInputHandler> {
            match slot {
                HandlerSlot::Input => {
                    let Ok(mut borrow) = window.callback_input_handler.try_borrow_mut() else {
                        return None;
                    };
                    borrow.take()
                }
                HandlerSlot::Selection => {
                    let Ok(mut borrow) = window.callback_selection_handler.try_borrow_mut() else {
                        return None;
                    };
                    borrow.take()
                }
            }
        };

        // Take the handler out of the RefCell in a scoped block.
        // This releases the borrow before callback execution.
        let preferred_slot = match window.active_text_interaction_mode.get() {
            TEXT_INTERACTION_EDITABLE => Some(HandlerSlot::Input),
            TEXT_INTERACTION_NONEDITABLE => Some(HandlerSlot::Selection),
            _ => None,
        };
        let (mut handler, restore_slot) = match preferred_slot {
            Some(slot) => {
                let Some(handler) = take_handler(slot) else {
                    return None;
                };
                (handler, slot)
            }
            None => {
                if let Some(handler) = take_handler(HandlerSlot::Input) {
                    (handler, HandlerSlot::Input)
                } else {
                    let Some(handler) = take_handler(HandlerSlot::Selection) else {
                        return None;
                    };
                    (handler, HandlerSlot::Selection)
                }
            }
        };
        // Borrow is now released - handler is owned, not borrowed

        // Execute callback with exclusive access to handler
        let result = f(&mut handler);

        // Restore handler back into RefCell
        match restore_slot {
            HandlerSlot::Input => {
                let Ok(mut borrow) = window.callback_input_handler.try_borrow_mut() else {
                    return Some(result);
                };
                *borrow = Some(handler);
            }
            HandlerSlot::Selection => {
                let Ok(mut borrow) = window.callback_selection_handler.try_borrow_mut() else {
                    return Some(result);
                };
                *borrow = Some(handler);
            }
        }

        Some(result)
    }
}

fn text_input_delegate(view: &Object) -> *mut Object {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return ptr::null_mut();
        }
        let window = &*(window_ptr as *const IosWindow);
        window.input_delegate.get()
    }
}

fn active_text_interaction_mode(view: &Object) -> i8 {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return TEXT_INTERACTION_NONE;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.active_text_interaction_mode.get()
    }
}

fn view_input_native_selection_enabled(view: &Object) -> bool {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return false;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.input_native_selection_enabled.get()
    }
}

fn view_handles_native_touch_selection(view: &Object) -> bool {
    handles_native_touch_selection(
        active_text_interaction_mode(view),
        view_input_native_selection_enabled(view),
    )
}

fn selection_action_presentations(view: &Object) -> Vec<(String, Option<String>)> {
    if !view_handles_native_touch_selection(view) {
        return Vec::new();
    }
    with_input_handler(view, |handler| {
        handler
            .selection_action_presentations()
            .into_iter()
            .map(|action| {
                (
                    action.name.to_string(),
                    action.image_name.map(|image_name| image_name.to_string()),
                )
            })
            .collect()
    })
    .unwrap_or_default()
}

fn selection_menu_presentation(view: &Object) -> SelectionMenuPresentation {
    if !view_handles_native_touch_selection(view) {
        return SelectionMenuPresentation::default();
    }
    with_input_handler(view, |handler| handler.selection_menu_presentation()).unwrap_or_default()
}

fn perform_selection_menu_action(view: *mut Object, action_index: usize) {
    if view.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        let view = &*view;
        if !view_handles_native_touch_selection(view) {
            return;
        }
        let _ = with_input_handler(view, |handler| {
            handler.perform_selection_action(action_index);
        });
    }));
}

fn ios_window_for_view(view: &Object) -> Option<&IosWindow> {
    unsafe {
        let window_ptr: *mut c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return None;
        }
        (window_ptr as *const IosWindow).as_ref()
    }
}

fn offset_text_position_index(index: usize, offset: isize, document_len: usize) -> Option<usize> {
    let index = isize::try_from(index).ok()?;
    let result = index.checked_add(offset)?;
    if result < 0 {
        return None;
    }

    let result = usize::try_from(result).ok()?;
    (result <= document_len).then_some(result)
}

fn text_position_index_at_range_offset(range: Range<usize>, offset: isize) -> Option<usize> {
    let offset = usize::try_from(offset).ok()?;
    let index = range.start.checked_add(offset)?;
    (index <= range.end).then_some(index)
}

fn text_position_offset_in_range(index: usize, range: Range<usize>) -> Option<usize> {
    (range.start <= index && index <= range.end).then_some(index - range.start)
}

fn text_character_range_by_extending_position(
    index: usize,
    direction: i64,
    document_len: usize,
) -> Option<Range<usize>> {
    if index > document_len {
        return None;
    }

    let range = match direction {
        1 | 2 => index.saturating_sub(1)..index,
        _ => index..(index + 1).min(document_len),
    };
    Some(range)
}

fn text_input_document_utf16_len(view: &Object) -> Option<usize> {
    with_input_handler(view, |handler| handler.text_len_utf16()).flatten()
}

fn text_input_has_text(document_utf16_len: Option<usize>) -> bool {
    document_utf16_len.is_some_and(|len| len > 0)
}

fn adjusted_native_selection_range_for_view(
    view: &Object,
    range: Range<usize>,
    native_touch_selection: bool,
) -> Range<usize> {
    if !native_touch_selection {
        return range;
    }

    with_input_handler(view, |handler| {
        handler.adjusted_native_selection_range(range.clone())
    })
    .flatten()
    .unwrap_or(range)
}

fn cached_active_selection_range_for_view(view: &Object) -> Option<Range<usize>> {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return None;
        }
        let window = &*(window_ptr as *const IosWindow);
        window
            .last_selection_geometry
            .borrow()
            .as_ref()
            .map(|geometry| geometry.range.clone())
    }
}

fn effective_native_selection_range_for_view(
    view: &Object,
    range: Range<usize>,
    native_touch_selection: bool,
) -> Range<usize> {
    let adjusted_range =
        adjusted_native_selection_range_for_view(view, range.clone(), native_touch_selection);
    // UIKit may request rects for the collapsed anchor after it already moved
    // selectedTextRange to a non-empty native selection. Keep drawing that range.
    if native_touch_selection
        && adjusted_range.is_empty()
        && let Some(cached_range) = cached_active_selection_range_for_view(view)
    {
        return cached_range;
    }
    adjusted_range
}

fn has_active_selection_geometry(view: &Object) -> bool {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return false;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.last_selection_geometry.borrow().is_some()
    }
}

fn sync_text_interaction_for_view(view: &Object) {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.sync_text_interaction_for_current_responder_state();
    }
}

fn text_input_uses_system_keyboard_for_gpui(view: &Object) -> bool {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return false;
        }
        let window = &*(window_ptr as *const IosWindow);
        should_use_system_keyboard(
            window.input_handler.borrow().is_some(),
            window.callback_input_handler.borrow().is_some(),
            window.input_accepts_text_input.get(),
            window.keyboard_session_requested.get(),
        )
    }
}

fn text_input_uses_keyboard_accessory_for_gpui(view: &Object) -> bool {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return false;
        }
        let window = &*(window_ptr as *const IosWindow);
        should_use_keyboard_accessory(
            window.input_handler.borrow().is_some(),
            window.callback_input_handler.borrow().is_some(),
            window.input_accepts_text_input.get(),
            window.keyboard_session_requested.get(),
            window.input_keyboard_accessory_enabled.get(),
        )
    }
}

fn active_text_input_traits_for_gpui(view: &Object) -> PlatformTextInputTraits {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return PlatformTextInputTraits::default();
        }
        let window = &*(window_ptr as *const IosWindow);
        if should_use_system_keyboard(
            window.input_handler.borrow().is_some(),
            window.callback_input_handler.borrow().is_some(),
            window.input_accepts_text_input.get(),
            window.keyboard_session_requested.get(),
        ) {
            window.input_text_input_traits.get()
        } else {
            PlatformTextInputTraits::default()
        }
    }
}

fn text_input_trait_value(trait_value: PlatformTextInputTrait) -> i64 {
    match trait_value {
        PlatformTextInputTrait::SystemDefault => 0,
        PlatformTextInputTrait::Disabled => 1,
        PlatformTextInputTrait::Enabled => 2,
    }
}

fn autocapitalization_value(value: PlatformTextAutocapitalization) -> i64 {
    match value {
        PlatformTextAutocapitalization::None => 0,
        PlatformTextAutocapitalization::Words => 1,
        PlatformTextAutocapitalization::Sentences => 2,
        PlatformTextAutocapitalization::AllCharacters => 3,
    }
}

/// Shared zero-sized custom input view used for read-only selection sessions.
/// Returning this from `inputView` keeps UIKit first-responder selection behavior
/// alive without emitting keyboard show notifications for noneditable text.
fn empty_keyboard_input_view() -> *mut Object {
    let ptr = EMPTY_KEYBOARD_INPUT_VIEW.load(std::sync::atomic::Ordering::Acquire);
    if ptr != 0 {
        return ptr as *mut Object;
    }

    unsafe {
        let view: *mut Object = msg_send![class!(UIView), new];
        let view_ptr = view as usize;
        match EMPTY_KEYBOARD_INPUT_VIEW.compare_exchange(
            0,
            view_ptr,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => view,
            Err(existing) => {
                let _: () = msg_send![view, release];
                existing as *mut Object
            }
        }
    }
}

fn allow_gpui_touch_delivery_while_text_interaction_recognizes(view: *mut Object) {
    unsafe {
        let gesture_recognizers: *mut Object = msg_send![view, gestureRecognizers];
        if gesture_recognizers.is_null() {
            return;
        }

        let count: usize = msg_send![gesture_recognizers, count];
        for index in 0..count {
            let recognizer: *mut Object = msg_send![gesture_recognizers, objectAtIndex: index];
            if recognizer.is_null() {
                continue;
            }

            let _: () = msg_send![recognizer, setDelaysTouchesBegan: NO];
            let _: () = msg_send![recognizer, setDelaysTouchesEnded: NO];
        }
    }
}

fn can_become_first_responder_for_gpui(view: &Object) -> bool {
    unsafe {
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return false;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.input_accepts_text_input.get()
            || matches!(
                window.target_text_interaction_mode.get(),
                TEXT_INTERACTION_NONEDITABLE | TEXT_INTERACTION_EDITABLE
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextInputMutationSource {
    TextInputSystem,
    External,
}

fn should_notify_text_input_delegate(source: TextInputMutationSource) -> bool {
    source == TextInputMutationSource::External
}

fn notify_selection_change<F>(view: &Object, f: F)
where
    F: FnOnce(),
{
    notify_selection_change_from(view, TextInputMutationSource::External, f);
}

fn notify_selection_change_from<F>(view: &Object, source: TextInputMutationSource, f: F)
where
    F: FnOnce(),
{
    unsafe {
        let delegate = text_input_delegate(view);
        let should_notify = should_notify_text_input_delegate(source);
        if should_notify && !delegate.is_null() {
            let _: () = msg_send![delegate, selectionWillChange: view];
        }
        f();
        if should_notify && !delegate.is_null() {
            let _: () = msg_send![delegate, selectionDidChange: view];
        }
    }
}

fn notify_text_and_selection_change<F>(view: &Object, f: F)
where
    F: FnOnce(),
{
    notify_text_and_selection_change_from(view, TextInputMutationSource::External, f);
}

fn notify_text_and_selection_change_from<F>(view: &Object, source: TextInputMutationSource, f: F)
where
    F: FnOnce(),
{
    unsafe {
        let delegate = text_input_delegate(view);
        let should_notify = should_notify_text_input_delegate(source);
        if should_notify && !delegate.is_null() {
            let _: () = msg_send![delegate, textWillChange: view];
            let _: () = msg_send![delegate, selectionWillChange: view];
        }
        // Critical: UIKit calls insertText/deleteBackward/replaceRange while it
        // is already driving an IME transaction. Delegate change notifications
        // are only for external edits; sending them here can make Telex commit
        // the tone change one keystroke late.
        f();
        if should_notify && !delegate.is_null() {
            let _: () = msg_send![delegate, selectionDidChange: view];
            let _: () = msg_send![delegate, textDidChange: view];
        }
    }
}

fn ns_string_to_rust_string(ns_string: *mut Object) -> Option<String> {
    if ns_string.is_null() {
        return None;
    }

    unsafe {
        let utf8: *const std::os::raw::c_char = msg_send![ns_string, UTF8String];
        if utf8.is_null() {
            return None;
        }

        Some(
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn ns_string_from_str(text: &str) -> *mut Object {
    let Ok(cstr) = CString::new(text) else {
        return ptr::null_mut();
    };
    unsafe { msg_send![class!(NSString), stringWithUTF8String: cstr.as_ptr()] }
}

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    static UIKeyInputUpArrow: *mut Object;
    static UIKeyInputDownArrow: *mut Object;
    static UIKeyInputLeftArrow: *mut Object;
    static UIKeyInputRightArrow: *mut Object;
}

/// HID key codes for the inputs registered in `build_key_commands`.
const KEY_CODE_TAB: u32 = 0x2B;
const KEY_CODE_RIGHT_ARROW: u32 = 0x4F;
const KEY_CODE_LEFT_ARROW: u32 = 0x50;
const KEY_CODE_DOWN_ARROW: u32 = 0x51;
const KEY_CODE_UP_ARROW: u32 = 0x52;
const KEY_CODE_V: u32 = 0x19;

/// Build the `keyCommands` array for GPUIMetalView.
///
/// Scoped deliberately: only the keys UIKit's text-input system claims for the
/// synthetic UITextInput document before `pressesBegan` ever runs. Everything
/// else (escape, function keys, ctrl+letter) still arrives through `pressesBegan`.
unsafe fn build_key_commands() -> *mut Object {
    unsafe {
        let commands: *mut Object = msg_send![class!(NSMutableArray), array];

        // Modifier sets a terminal cares about. Cmd+arrow is left to the system.
        const ARROW_MODIFIERS: [u32; 8] = [
            0,
            UI_KEY_MODIFIER_SHIFT,
            UI_KEY_MODIFIER_CONTROL,
            UI_KEY_MODIFIER_ALTERNATE,
            UI_KEY_MODIFIER_SHIFT | UI_KEY_MODIFIER_CONTROL,
            UI_KEY_MODIFIER_SHIFT | UI_KEY_MODIFIER_ALTERNATE,
            UI_KEY_MODIFIER_CONTROL | UI_KEY_MODIFIER_ALTERNATE,
            UI_KEY_MODIFIER_SHIFT | UI_KEY_MODIFIER_CONTROL | UI_KEY_MODIFIER_ALTERNATE,
        ];
        for input in [
            UIKeyInputUpArrow,
            UIKeyInputDownArrow,
            UIKeyInputLeftArrow,
            UIKeyInputRightArrow,
        ] {
            for modifiers in ARROW_MODIFIERS {
                push_key_command(commands, input, modifiers);
            }
        }

        // Tab: iPadOS hands it to the focus system instead of the app.
        let tab = ns_string_from_str("\t");
        push_key_command(commands, tab, 0);
        push_key_command(commands, tab, UI_KEY_MODIFIER_SHIFT);

        // Cmd+V has no responder-chain path today (`paste:` is unimplemented), so
        // UIKit swallows it. Cmd+C/X/A are left alone: they still reach `copy:`
        // or the UITextInput document, and stealing them would regress those.
        push_key_command(commands, ns_string_from_str("v"), UI_KEY_MODIFIER_COMMAND);

        msg_send![commands, copy]
    }
}

unsafe fn push_key_command(commands: *mut Object, input: *mut Object, modifier_flags: u32) {
    unsafe {
        if input.is_null() {
            return;
        }
        let command: *mut Object = msg_send![
            class!(UIKeyCommand),
            keyCommandWithInput: input
            modifierFlags: modifier_flags as i64
            action: sel!(gpuiHandleKeyCommand:)
        ];
        if command.is_null() {
            return;
        }
        // Without this the system's own text navigation still wins these keys.
        let _: () = msg_send![command, setWantsPriorityOverSystemBehavior: YES];
        let _: () = msg_send![commands, addObject: command];
    }
}

/// Map a `UIKeyCommand.input` string back to the HID code `handle_key_event` expects.
unsafe fn key_command_key_code(input: *mut Object) -> Option<u32> {
    unsafe {
        if input.is_null() {
            return None;
        }
        for (constant, key_code) in [
            (UIKeyInputUpArrow, KEY_CODE_UP_ARROW),
            (UIKeyInputDownArrow, KEY_CODE_DOWN_ARROW),
            (UIKeyInputLeftArrow, KEY_CODE_LEFT_ARROW),
            (UIKeyInputRightArrow, KEY_CODE_RIGHT_ARROW),
        ] {
            let equal: BOOL = msg_send![input, isEqualToString: constant];
            if equal == YES {
                return Some(key_code);
            }
        }

        let utf8: *const std::os::raw::c_char = msg_send![input, UTF8String];
        if utf8.is_null() {
            return None;
        }
        match std::ffi::CStr::from_ptr(utf8).to_str().ok()? {
            "\t" => Some(KEY_CODE_TAB),
            "v" => Some(KEY_CODE_V),
            _ => None,
        }
    }
}

fn ui_image_from_name(name: &str) -> *mut Object {
    let image_name = ns_string_from_str(name);
    if image_name.is_null() {
        return ptr::null_mut();
    }

    unsafe {
        let image: *mut Object = msg_send![class!(UIImage), imageNamed: image_name];
        if !image.is_null() {
            return image;
        }

        msg_send![class!(UIImage), systemImageNamed: image_name]
    }
}

fn dictation_result_text(dictation_result: *mut Object) -> Option<String> {
    if dictation_result.is_null() {
        return None;
    }

    unsafe {
        let count: usize = msg_send![dictation_result, count];
        let mut result = String::new();
        for index in 0..count {
            let phrase: *mut Object = msg_send![dictation_result, objectAtIndex: index];
            if phrase.is_null() {
                continue;
            }
            let text: *mut Object = msg_send![phrase, text];
            if let Some(text) = ns_string_to_rust_string(text) {
                result.push_str(&text);
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

/// Register a custom UIView subclass that uses CAMetalLayer as its backing layer.
/// This is required for Metal rendering on iOS.
fn register_metal_view_class() -> &'static Class {
    METAL_VIEW_CLASS_REGISTERED.call_once(|| {
        let superclass = class!(UIView);
        let mut decl = ClassDecl::new("GPUIMetalView", superclass).unwrap();

        // Add ivar to store window pointer for touch handling
        decl.add_ivar::<*mut std::ffi::c_void>(GPUI_WINDOW_IVAR);

        // CRITICAL: Declare protocol conformance for text input
        // Without this, iOS won't recognize the view as a text input view
        // and won't show the keyboard or send text input events
        // Note: UITextInput inherits from UIKeyInput and UITextInputTraits,
        // so we only need to add UITextInput
        {
            use objc::runtime::Protocol;
            if let Some(protocol) = Protocol::get("UITextInput") {
                decl.add_protocol(protocol);
            }
            if let Some(protocol) = Protocol::get("UITextInteractionDelegate") {
                decl.add_protocol(protocol);
            }
        }

        // Override layerClass to return CAMetalLayer
        extern "C" fn layer_class(_self: &Class, _sel: Sel) -> *const Class {
            class!(CAMetalLayer) as *const Class
        }

        // Touch handling methods
        extern "C" fn touches_began(
            this: &mut Object,
            _sel: Sel,
            touches: *mut Object,
            event: *mut Object,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn touches_moved(
            this: &mut Object,
            _sel: Sel,
            touches: *mut Object,
            event: *mut Object,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn touches_ended(
            this: &mut Object,
            _sel: Sel,
            touches: *mut Object,
            event: *mut Object,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn touches_cancelled(
            this: &mut Object,
            _sel: Sel,
            touches: *mut Object,
            event: *mut Object,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn can_become_first_responder(this: &Object, _sel: Sel) -> bool {
            can_become_first_responder_for_gpui(this)
        }

        // Keep UIKit text interactions in sync after responder transitions, as
        // recommended for custom views that support editable and read-only text.
        extern "C" fn become_first_responder(this: &mut Object, _sel: Sel) -> BOOL {
            unsafe {
                let was_first_responder: BOOL = msg_send![this, isFirstResponder];
                let superclass = class!(UIView);
                let result: BOOL = msg_send![super(this, superclass), becomeFirstResponder];
                let is_first_responder: BOOL = msg_send![this, isFirstResponder];
                if was_first_responder == NO && is_first_responder == YES {
                    sync_text_interaction_for_view(this);
                }
                result
            }
        }

        extern "C" fn resign_first_responder(this: &mut Object, _sel: Sel) -> BOOL {
            unsafe {
                let was_first_responder: BOOL = msg_send![this, isFirstResponder];
                let superclass = class!(UIView);
                let result: BOOL = msg_send![super(this, superclass), resignFirstResponder];
                let is_first_responder: BOOL = msg_send![this, isFirstResponder];
                if was_first_responder == YES && is_first_responder == NO {
                    sync_text_interaction_for_view(this);
                }
                result
            }
        }

        // Return the view shown above the software keyboard only for input handlers
        // that explicitly opt into the app-provided accessory.
        extern "C" fn input_accessory_view(this: &Object, _sel: Sel) -> *mut Object {
            if !text_input_uses_keyboard_accessory_for_gpui(this) {
                return ptr::null_mut();
            }
            let ptr = KEYBOARD_ACCESSORY_VIEW.load(std::sync::atomic::Ordering::Relaxed);
            ptr as *mut Object
        }

        // Returning nil asks UIKit for the default system keyboard. Read-only
        // selection still needs first-responder status for copy/selection UI, so
        // give it an empty custom input view instead of the software keyboard.
        extern "C" fn input_view(this: &Object, _sel: Sel) -> *mut Object {
            if text_input_uses_system_keyboard_for_gpui(this) {
                ptr::null_mut()
            } else {
                empty_keyboard_input_view()
            }
        }

        // UITextInputTraits - keyboard type (default)
        extern "C" fn keyboard_type(_this: &Object, _sel: Sel) -> i64 {
            0 // UIKeyboardTypeDefault
        }

        // UITextInputTraits - return key type
        extern "C" fn return_key_type(_this: &Object, _sel: Sel) -> i64 {
            0 // UIReturnKeyDefault
        }

        // UITextInputTraits - autocapitalization type
        extern "C" fn autocapitalization_type(this: &Object, _sel: Sel) -> i64 {
            autocapitalization_value(active_text_input_traits_for_gpui(this).autocapitalization)
        }

        // UITextInputTraits - autocorrection type
        extern "C" fn autocorrection_type(this: &Object, _sel: Sel) -> i64 {
            text_input_trait_value(active_text_input_traits_for_gpui(this).autocorrection)
        }

        // UITextInputTraits - smart quotes type
        // UITextSmartQuotesType: 0=Default, 1=No, 2=Yes
        extern "C" fn smart_quotes_type(this: &Object, _sel: Sel) -> i64 {
            text_input_trait_value(active_text_input_traits_for_gpui(this).smart_quotes)
        }

        // UITextInputTraits - smart dashes type
        // UITextSmartDashesType: 0=Default, 1=No, 2=Yes
        extern "C" fn smart_dashes_type(this: &Object, _sel: Sel) -> i64 {
            text_input_trait_value(active_text_input_traits_for_gpui(this).smart_dashes)
        }

        // UITextInputTraits - smart insert delete type
        // UITextSmartInsertDeleteType: 0=Default, 1=No, 2=Yes
        extern "C" fn smart_insert_delete_type(this: &Object, _sel: Sel) -> i64 {
            text_input_trait_value(active_text_input_traits_for_gpui(this).smart_insert_delete)
        }

        // UITextInputTraits - spell checking type
        // UITextSpellCheckingType: 0=Default, 1=No, 2=Yes
        extern "C" fn spell_checking_type(this: &Object, _sel: Sel) -> i64 {
            text_input_trait_value(active_text_input_traits_for_gpui(this).spell_checking)
        }

        // UITextInputTraits - inline prediction type
        // UITextInlinePredictionType: 0=Default, 1=No, 2=Yes
        extern "C" fn inline_prediction_type(this: &Object, _sel: Sel) -> i64 {
            text_input_trait_value(active_text_input_traits_for_gpui(this).inline_prediction)
        }

        // Tell iOS we want to receive keyboard input
        extern "C" fn is_user_interaction_enabled(_this: &Object, _sel: Sel) -> bool {
            true
        }

        // UITextInteractionDelegate - interactionShouldBegin:atPoint:
        // Keep UIKit's full-view selection gestures from stealing ordinary GPUI
        // taps. Native text interaction starts only on actual text; GPUI clears
        // existing selection on outside touches before forwarding the press.
        extern "C" fn interaction_should_begin(
            this: &Object,
            _sel: Sel,
            _interaction: *mut Object,
            point: IOSCGPoint,
        ) -> BOOL {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let interaction_mode = active_text_interaction_mode(this);
                let point = Point::new(px(point.x as f32), px(point.y as f32));
                unsafe {
                    let window_ptr: *mut std::ffi::c_void = *this.get_ivar(GPUI_WINDOW_IVAR);
                    if window_ptr.is_null() {
                        false
                    } else {
                        let window = &*(window_ptr as *const IosWindow);
                        let input_native_selection_enabled =
                            window.input_native_selection_enabled.get();
                        let input_native_tap_count = window.last_touch_tap_count.get();
                        let input_native_long_press =
                            window.primary_touch_began_at.borrow().as_ref().is_some_and(
                                |began_at| {
                                    began_at.elapsed()
                                        >= EDITABLE_NATIVE_SELECTION_LONG_PRESS_MIN_DURATION
                                },
                            );
                        let hit_selectable_text = if input_native_selection_enabled {
                            if input_native_tap_count != 1 || !input_native_long_press {
                                false
                            } else {
                                // Occlusion first: a layer painted above the surface
                                // (e.g. a drawer) suppresses selection beneath it.
                                with_input_handler(this, |handler| {
                                    handler.native_selection_allowed_at(point)
                                        && handler.character_index_for_point(point).is_some()
                                })
                                .unwrap_or(false)
                            }
                        } else {
                            window.point_hits_selectable_text(point)
                        };
                        should_begin_text_interaction(
                            interaction_mode,
                            input_native_selection_enabled,
                            hit_selectable_text,
                        )
                    }
                }
            }))
            .unwrap_or(false);

            if result { YES } else { NO }
        }

        // UIKeyInput protocol - hasText
        extern "C" fn has_text(this: &Object, _sel: Sel) -> bool {
            // Critical: this must reflect the synthetic UITextInput document.
            // Returning true for an empty editable input confuses native delete
            // and replay decisions during IME rewrites.
            text_input_has_text(text_input_document_utf16_len(this))
        }

        // UIKeyInput protocol - insertText:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        // Note: iOS sends printable characters ONLY through insertText, not pressesBegan,
        // so we always process insertText (no hardware key blocking needed here).
        extern "C" fn insert_text(this: &mut Object, _sel: Sel, text: *mut Object) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                unsafe {
                    // Get the string from the NSString first (before any handler access)
                    let utf8: *const std::os::raw::c_char = msg_send![text, UTF8String];
                    if utf8.is_null() {
                        return;
                    }

                    let text_str = std::ffi::CStr::from_ptr(utf8).to_string_lossy();
                    let has_input_handler = with_input_handler(this, |_handler| ()).is_some();

                    // First try the input handler directly (for text fields)
                    // This is the preferred path for software keyboard input.
                    // Uses with_input_handler which releases the borrow during callback
                    // to prevent conflicts when iOS queries multiple UITextInput methods.
                    if has_input_handler {
                        notify_text_and_selection_change_from(
                            this,
                            TextInputMutationSource::TextInputSystem,
                            || {
                                let _ = with_input_handler(this, |handler| {
                                    handler.insert_text(&text_str);
                                });
                            },
                        );
                        return;
                    }

                    // Fallback: with_input_handler returned None (no handler set)
                    // Send as key events for non-input-handler scenarios
                    let window_ptr: *mut std::ffi::c_void = *this.get_ivar(GPUI_WINDOW_IVAR);
                    if window_ptr.is_null() {
                        return;
                    }
                    let window = &*(window_ptr as *const IosWindow);

                    for ch in text_str.chars() {
                        match ch {
                            '\n' | '\r' => {
                                window.handle_key_event(40, 0, true); // Return key code
                            }
                            _ => {
                                // Send as individual character key event
                                let keystroke = gpui::Keystroke {
                                    modifiers: Modifiers::default(),
                                    key: ch.to_string(),
                                    key_char: Some(ch.to_string()),
                                };

                                let event = PlatformInput::KeyDown(gpui::KeyDownEvent {
                                    keystroke,
                                    is_held: false,
                                    prefer_character_input: true,
                                });

                                if let Some(callback) = window.input_callback.borrow_mut().as_mut()
                                {
                                    callback(event);
                                }
                            }
                        }
                    }
                }
            }));
        }

        // UITextInput - insertText:alternatives:style:
        // Some iOS text services use this richer insertion selector before
        // falling back to plain insertText:. Route it through the same guarded path.
        extern "C" fn insert_text_with_alternatives(
            this: &mut Object,
            _sel: Sel,
            text: *mut Object,
            _alternatives: *mut Object,
            _style: i64,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                insert_text(this, sel!(insertText:), text);
            }));
        }

        // UIKeyInput protocol - deleteBackward
        // This is the ONLY path for backspace deletion - we skip backspace in pressesBegan
        // to avoid duplicate handling.
        //
        extern "C" fn delete_backward(this: &mut Object, _sel: Sel) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                // Route software-keyboard deletes through the active GPUI input handler.
                // The callback mirror keeps this available across GPUI's per-frame take/set cycle.
                let has_input_handler = with_input_handler(this, |_handler| ()).is_some();
                if has_input_handler {
                    notify_text_and_selection_change_from(
                        this,
                        TextInputMutationSource::TextInputSystem,
                        || {
                            let _ = with_input_handler(this, |handler| {
                                handler.delete_backward();
                            });
                        },
                    );
                }
            }));
        }

        // Hardware keyboard handling
        extern "C" fn presses_began(
            this: &mut Object,
            _sel: Sel,
            presses: *mut Object,
            event: *mut Object,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                handle_presses(this, presses, true);
            }));
            // Call super
            unsafe {
                let superclass = class!(UIView);
                let _: () =
                    msg_send![super(this, superclass), pressesBegan: presses withEvent: event];
            }
        }

        extern "C" fn presses_ended(
            this: &mut Object,
            _sel: Sel,
            presses: *mut Object,
            event: *mut Object,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                handle_presses(this, presses, false);
            }));
            // Call super
            unsafe {
                let superclass = class!(UIView);
                let _: () =
                    msg_send![super(this, superclass), pressesEnded: presses withEvent: event];
            }
        }

        // ============================================
        // UITextInput Protocol - Core Properties
        // ============================================

        // UITextInput - beginningOfDocument
        extern "C" fn beginning_of_document(_this: &Object, _sel: Sel) -> *mut Object {
            create_text_position(0)
        }

        // UITextInput - endOfDocument
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn end_of_document(this: &Object, _sel: Sel) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let len = with_input_handler(this, |handler| {
                    let mut adjusted = None;
                    handler
                        .text_for_range(0..usize::MAX, &mut adjusted)
                        .map(|s| s.encode_utf16().count())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
                create_text_position(len)
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => create_text_position(0),
            }
        }

        // UITextInput - selectedTextRange
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn selected_text_range(this: &Object, _sel: Sel) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let range = with_input_handler(this, |handler| handler.selected_text_range(false))
                    .flatten();
                let interaction_mode = active_text_interaction_mode(this);
                match range {
                    Some(selection) => {
                        create_text_range(selection.range.start, selection.range.end)
                    }
                    None if interaction_mode == TEXT_INTERACTION_NONEDITABLE => ptr::null_mut(),
                    None => create_text_range(0, 0),
                }
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => {
                    if active_text_interaction_mode(this) == TEXT_INTERACTION_NONEDITABLE {
                        ptr::null_mut()
                    } else {
                        create_text_range(0, 0)
                    }
                }
            }
        }

        // UITextInput - setSelectedTextRange:
        extern "C" fn set_selected_text_range(this: &mut Object, _sel: Sel, range: *mut Object) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let interaction_mode = active_text_interaction_mode(this);
                let input_native_selection_enabled = view_input_native_selection_enabled(this);
                let report_geometry = should_report_text_input_range_geometry(
                    interaction_mode,
                    input_native_selection_enabled,
                );
                if !report_geometry {
                    return;
                }
                if let Some((start, end)) = get_range_indices(range) {
                    let requested_range = start..end;
                    let native_touch_selection = handles_native_touch_selection(
                        interaction_mode,
                        input_native_selection_enabled,
                    );
                    let adjusted_range = adjusted_native_selection_range_for_view(
                        this,
                        requested_range.clone(),
                        native_touch_selection,
                    );
                    if adjusted_range != requested_range {
                        let adjusted_native_range = adjusted_range.clone();
                        let _ = set_range_indices(
                            range,
                            adjusted_native_range.start,
                            adjusted_native_range.end,
                        );
                    }
                    // Critical: UIKit can move the caret inside an active IME
                    // composition. Ignoring this for editable inputs leaves the
                    // native tokenizer and GPUI shadow document out of sync.
                    notify_selection_change_from(
                        this,
                        TextInputMutationSource::TextInputSystem,
                        || {
                            let adjusted_range = adjusted_range.clone();
                            let _ = with_input_handler(this, |handler| {
                                handler.set_selected_text_range(adjusted_range);
                            });
                        },
                    );
                    if native_touch_selection {
                        unsafe {
                            let window_ptr: *mut std::ffi::c_void =
                                *this.get_ivar(GPUI_WINDOW_IVAR);
                            if !window_ptr.is_null() {
                                let window = &*(window_ptr as *const IosWindow);
                                window.cache_active_selection_range(adjusted_range);
                            }
                        }
                    }
                }
            }));
        }

        // UIResponder - copy:
        // Called by UIKit's edit menu when the user taps "Copy".
        extern "C" fn copy_action(this: &Object, _sel: Sel, _sender: *mut Object) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                with_input_handler(this, |handler| {
                    let selection = handler.selected_text_range(false)?;
                    if selection.range.is_empty() {
                        return None;
                    }
                    let mut adj = None;
                    let selection_range = selection.range.clone();
                    let text = handler.text_for_range(selection_range.clone(), &mut adj)?;
                    unsafe {
                        use std::ffi::CString;
                        if let Ok(cstr) = CString::new(text) {
                            let ns_string: *mut Object =
                                msg_send![class!(NSString), stringWithUTF8String: cstr.as_ptr()];
                            if !ns_string.is_null() {
                                let pb: *mut Object =
                                    msg_send![class!(UIPasteboard), generalPasteboard];
                                let _: () = msg_send![pb, setString: ns_string];
                            }
                        }
                    }
                    Some(())
                });
            }));
        }

        // UIKeyCommand target for the keys UIKit's text-input system would
        // otherwise consume before `pressesBegan` (arrows, tab, cmd editing).
        extern "C" fn handle_key_command(this: &mut Object, _sel: Sel, command: *mut Object) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                if command.is_null() {
                    return;
                }
                let input: *mut Object = msg_send![command, input];
                let Some(key_code) = key_command_key_code(input) else {
                    return;
                };
                let modifier_flags: i64 = msg_send![command, modifierFlags];

                let window_ptr: *mut std::ffi::c_void = *this.get_ivar(GPUI_WINDOW_IVAR);
                if window_ptr.is_null() {
                    return;
                }
                let window = &*(window_ptr as *const IosWindow);
                window.handle_key_event(key_code, modifier_flags as u32, true);
            }));
        }

        // UIResponder - keyCommands
        extern "C" fn key_commands(_this: &Object, _sel: Sel) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                // UIKit re-queries this on every responder-chain rebuild; the set
                // is constant, so build the retained array once.
                static COMMANDS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
                *COMMANDS.get_or_init(|| unsafe { build_key_commands() as usize }) as *mut Object
            }));
            result.unwrap_or(ptr::null_mut())
        }

        // UITextInput - editMenuForTextRange:suggestedActions:
        // Append GPUI selection-area actions to UIKit's native edit menu.
        extern "C" fn edit_menu_for_text_range(
            this: &Object,
            _sel: Sel,
            _text_range: *mut Object,
            suggested_actions: *mut Object,
        ) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let menu_presentation = selection_menu_presentation(this);
                let action_presentations = selection_action_presentations(this);
                if action_presentations.is_empty()
                    && menu_presentation == SelectionMenuPresentation::SystemAndCustomActions
                {
                    return ptr::null_mut();
                }

                unsafe {
                    let children: *mut Object = msg_send![class!(NSMutableArray), array];
                    if menu_presentation == SelectionMenuPresentation::SystemAndCustomActions
                        && !suggested_actions.is_null()
                    {
                        let _: () = msg_send![children, addObjectsFromArray: suggested_actions];
                    }

                    let view = this as *const Object as *mut Object;
                    for (action_index, (action_name, image_name)) in
                        action_presentations.into_iter().enumerate()
                    {
                        let title = ns_string_from_str(&action_name);
                        if title.is_null() {
                            continue;
                        }
                        let image = image_name
                            .as_deref()
                            .map(ui_image_from_name)
                            .unwrap_or(ptr::null_mut());

                        // UIAction handlers can run after this menu is built, so dispatch by index.
                        let block = ConcreteBlock::new(move |_action: *mut Object| {
                            perform_selection_menu_action(view, action_index);
                        });
                        let block = block.copy();
                        let action: *mut Object = msg_send![
                            class!(UIAction),
                            actionWithTitle: title
                            image: image
                            identifier: ptr::null_mut::<Object>()
                            handler: &*block
                        ];
                        if !action.is_null() {
                            let _: () = msg_send![children, addObject: action];
                        }
                    }

                    msg_send![class!(UIMenu), menuWithChildren: children]
                }
            }));
            result.unwrap_or(ptr::null_mut())
        }

        // UIResponder - canPerformAction:withSender:
        // Current GPUI input handlers do not support native text selection, so
        // editable mode suppresses UIKit's edit menu. Read-only selection keeps
        // native copy support for selected GPUI text.
        extern "C" fn can_perform_action(
            this: &mut Object,
            _sel: Sel,
            action: Sel,
            sender: *mut Object,
        ) -> BOOL {
            // Key commands are validated through this selector too; the edit-menu
            // policy below would otherwise veto them in editable mode.
            if action == sel!(gpuiHandleKeyCommand:) {
                return YES;
            }

            let interaction_mode = active_text_interaction_mode(this);
            let input_native_selection_enabled = view_input_native_selection_enabled(this);
            let policy = edit_menu_action_policy(
                interaction_mode,
                input_native_selection_enabled,
                action == sel!(copy:),
                selection_menu_presentation(this),
            );
            let result = match policy {
                EditMenuActionPolicy::CopySelection => {
                    let result = panic::catch_unwind(AssertUnwindSafe(|| {
                        let has_selection = with_input_handler(this, |handler| {
                            handler
                                .selected_text_range(false)
                                .map(|s| !s.range.is_empty())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                        if has_selection { YES } else { NO }
                    }));
                    result.unwrap_or(NO)
                }
                EditMenuActionPolicy::DelegateToSystem => unsafe {
                    let superclass = class!(UIView);
                    msg_send![super(this, superclass), canPerformAction: action withSender: sender]
                },
                EditMenuActionPolicy::DisableNativeMenu => NO,
            };
            result
        }

        // UITextInput - markedTextRange (returns nil when no marked text)
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn marked_text_range(this: &Object, _sel: Sel) -> *mut Object {
            // Wrap in catch_unwind to prevent panics from unwinding through FFI boundary
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let range =
                    with_input_handler(this, |handler| handler.marked_text_range()).flatten();

                match range {
                    Some(r) => create_text_range(r.start, r.end),
                    None => std::ptr::null_mut(),
                }
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => std::ptr::null_mut(),
            }
        }

        // UITextInput - markedTextStyle (not used, return nil)
        extern "C" fn marked_text_style(_this: &Object, _sel: Sel) -> *mut Object {
            std::ptr::null_mut()
        }

        // UITextInput - setMarkedTextStyle: (not used)
        extern "C" fn set_marked_text_style(_this: &mut Object, _sel: Sel, _style: *mut Object) {
            // No-op
        }

        // UITextInput - selectionAffinity
        extern "C" fn selection_affinity(_this: &Object, _sel: Sel) -> i64 {
            0 // UITextStorageDirectionForward
        }

        // UITextInput - setSelectionAffinity:
        extern "C" fn set_selection_affinity(_this: &mut Object, _sel: Sel, _affinity: i64) {
            // No-op. GPUI text handlers expose logical UTF-16 ranges only.
        }

        // UITextInput - inputDelegate (store reference to delegate)
        extern "C" fn input_delegate(this: &Object, _sel: Sel) -> *mut Object {
            text_input_delegate(this)
        }

        // UITextInput - setInputDelegate:
        extern "C" fn set_input_delegate(this: &mut Object, _sel: Sel, delegate: *mut Object) {
            unsafe {
                let window_ptr: *mut std::ffi::c_void = *this.get_ivar(GPUI_WINDOW_IVAR);
                if window_ptr.is_null() {
                    return;
                }
                let window = &*(window_ptr as *const IosWindow);
                window.input_delegate.set(delegate);
            }
        }

        // UITextInput - tokenizer (use default string tokenizer)
        extern "C" fn tokenizer(this: &Object, _sel: Sel) -> *mut Object {
            unsafe {
                // Use UITextInputStringTokenizer as default
                let tokenizer: *mut Object = msg_send![class!(UITextInputStringTokenizer), alloc];
                let tokenizer: *mut Object = msg_send![tokenizer, initWithTextInput: this];
                tokenizer
            }
        }

        // ============================================
        // UITextInput Protocol - Text Manipulation
        // ============================================

        // UITextInput - textInRange:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn text_in_range(this: &Object, _sel: Sel, range: *mut Object) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let Some((start, end)) = get_range_indices(range) else {
                    return std::ptr::null_mut();
                };

                let mut adjusted = None;
                let handler_result = with_input_handler(this, |handler| {
                    handler.text_for_range(start..end, &mut adjusted)
                });
                let text = match handler_result {
                    Some(text) => text,
                    None => None,
                };

                match text {
                    Some(s) => unsafe {
                        let c_str = std::ffi::CString::new(s).unwrap_or_default();
                        let ns_string: *mut Object =
                            msg_send![class!(NSString), stringWithUTF8String: c_str.as_ptr()];
                        ns_string
                    },
                    None => std::ptr::null_mut(),
                }
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => std::ptr::null_mut(),
            }
        }

        // UITextInput - replaceRange:withText:
        // This is called by iOS for smart punctuation and autocorrect
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn replace_range_with_text(
            this: &mut Object,
            _sel: Sel,
            range: *mut Object,
            text: *mut Object,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let Some((start, end)) = get_range_indices(range) else {
                    return;
                };

                unsafe {
                    let utf8: *const std::os::raw::c_char = msg_send![text, UTF8String];
                    if utf8.is_null() {
                        return;
                    }
                    let text_str = std::ffi::CStr::from_ptr(utf8).to_string_lossy();
                    notify_text_and_selection_change_from(
                        this,
                        TextInputMutationSource::TextInputSystem,
                        || {
                            let _ = with_input_handler(this, |handler| {
                                handler.replace_range(start..end, &text_str);
                            });
                        },
                    );
                }
            }));
        }

        // UITextInput - shouldChangeTextInRange:replacementText:
        extern "C" fn should_change_text_in_range(
            this: &Object,
            _sel: Sel,
            range: *mut Object,
            text: *mut Object,
        ) -> BOOL {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let range = get_range_indices(range).map(|(start, end)| start..end);
                let text_str = ns_string_to_rust_string(text).unwrap_or_default();
                let should_change = with_input_handler(this, |handler| {
                    handler.should_change_text_in_range(range.clone(), &text_str)
                })
                .unwrap_or(true);
                // Critical: UIKit's text system asks this before native IME
                // rewrites. Let the handler observe that selector while keeping
                // replacement validation on the platform path by default.
                if should_change { YES } else { NO }
            }));

            result.unwrap_or(YES)
        }

        // UITextInput - replaceRange:withAttributedText:
        // Keep attributed native replacements on the same context-rewrite path
        // as plain replacements so IMEs and suggestions do not bypass the model.
        extern "C" fn replace_range_with_attributed_text(
            this: &mut Object,
            _sel: Sel,
            range: *mut Object,
            attributed_text: *mut Object,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                let text: *mut Object = msg_send![attributed_text, string];
                replace_range_with_text(this, sel!(replaceRange:withText:), range, text);
            }));
        }

        // UITextInput - setMarkedText:selectedRange:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn set_marked_text(
            this: &mut Object,
            _sel: Sel,
            marked_text: *mut Object,
            selected_range: NSRange,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                unsafe {
                    if marked_text.is_null() {
                        // Unmark text
                        notify_text_and_selection_change_from(
                            this,
                            TextInputMutationSource::TextInputSystem,
                            || {
                                let _ = with_input_handler(this, |handler| {
                                    handler.unmark_text();
                                });
                            },
                        );
                        return;
                    }

                    // Check if it's NSAttributedString
                    let is_attributed: BOOL =
                        msg_send![marked_text, isKindOfClass: class!(NSAttributedString)];
                    let text_obj: *mut Object = if is_attributed == YES {
                        msg_send![marked_text, string]
                    } else {
                        marked_text
                    };

                    let utf8: *const std::os::raw::c_char = msg_send![text_obj, UTF8String];
                    if utf8.is_null() {
                        return;
                    }
                    let text_str = std::ffi::CStr::from_ptr(utf8).to_string_lossy();

                    let selected = if selected_range.location != NS_NOT_FOUND {
                        Some(
                            selected_range.location as usize
                                ..(selected_range.location + selected_range.length) as usize,
                        )
                    } else {
                        None
                    };

                    notify_text_and_selection_change_from(
                        this,
                        TextInputMutationSource::TextInputSystem,
                        || {
                            let _ = with_input_handler(this, |handler| {
                                handler.set_marked_text(&text_str, selected, None);
                            });
                        },
                    );
                }
            }));
        }

        // UITextInput - setAttributedMarkedText:selectedRange:
        // Some IMEs use attributed marked text for live composition. Preserve
        // the same marked-text path instead of falling back to delete/insert.
        extern "C" fn set_attributed_marked_text(
            this: &mut Object,
            _sel: Sel,
            marked_text: *mut Object,
            selected_range: NSRange,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                if marked_text.is_null() {
                    set_marked_text(
                        this,
                        sel!(setMarkedText:selectedRange:),
                        ptr::null_mut(),
                        selected_range,
                    );
                    return;
                }
                let text: *mut Object = msg_send![marked_text, string];
                set_marked_text(
                    this,
                    sel!(setMarkedText:selectedRange:),
                    text,
                    selected_range,
                );
            }));
        }

        // UITextInput - unmarkText
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn unmark_text(this: &mut Object, _sel: Sel) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                notify_text_and_selection_change_from(
                    this,
                    TextInputMutationSource::TextInputSystem,
                    || {
                        let _ = with_input_handler(this, |handler| {
                            handler.unmark_text();
                        });
                    },
                );
            }));
        }

        // UITextInput - insertDictationResult:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn insert_dictation_result(
            this: &mut Object,
            _sel: Sel,
            dictation_result: *mut Object,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let Some(text) = dictation_result_text(dictation_result) else {
                    return;
                };

                let _ = with_input_handler(this, |handler| {
                    handler.insert_dictation_result(&text);
                });
            }));
        }

        // UITextInput - dictationRecordingDidEnd
        // This only means the microphone stopped recording. UIKit may still
        // query and replace its live hypothesis before delivering the final
        // insertDictationResult: or removing the placeholder, so keep the
        // synthetic marked text alive here.
        extern "C" fn dictation_recording_did_end(this: &mut Object, _sel: Sel) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = with_input_handler(this, |handler| {
                    handler.dictation_recording_did_end();
                });
            }));
        }

        // UITextInput - dictationRecognitionFailed
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn dictation_recognition_failed(this: &mut Object, _sel: Sel) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = with_input_handler(this, |handler| {
                    handler.dictation_recognition_failed();
                });
            }));
        }

        // UITextInput - insertDictationResultPlaceholder
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn insert_dictation_result_placeholder(this: &Object, _sel: Sel) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = with_input_handler(this, |handler| {
                    handler.insert_dictation_result_placeholder();
                });

                this as *const Object as *mut Object
            }));

            result.unwrap_or(ptr::null_mut())
        }

        // UITextInput - frameForDictationResultPlaceholder:
        extern "C" fn frame_for_dictation_result_placeholder(
            this: &Object,
            _sel: Sel,
            _placeholder: *mut Object,
        ) -> IOSCGRect {
            let default_rect = IOSCGRect::new(IOSCGPoint::new(0.0, 0.0), IOSCGSize::new(0.0, 0.0));
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let bounds = with_input_handler(this, |handler| {
                    let range = handler
                        .selected_text_range(false)
                        .map(|selection| {
                            if selection.reversed {
                                selection.range.start..selection.range.start
                            } else {
                                selection.range.end..selection.range.end
                            }
                        })
                        .unwrap_or(0..0);
                    handler.bounds_for_range(range)
                })
                .flatten();

                match bounds {
                    Some(bounds) => IOSCGRect::new(
                        IOSCGPoint::new(
                            bounds.origin.x.as_f32() as f64,
                            bounds.origin.y.as_f32() as f64,
                        ),
                        IOSCGSize::new(
                            bounds.size.width.as_f32() as f64,
                            bounds.size.height.as_f32() as f64,
                        ),
                    ),
                    None => default_rect,
                }
            }));

            result.unwrap_or(default_rect)
        }

        // UITextInput - removeDictationResultPlaceholder:willInsertResult:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn remove_dictation_result_placeholder(
            this: &mut Object,
            _sel: Sel,
            _placeholder: *mut Object,
            will_insert_result: BOOL,
        ) {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = with_input_handler(this, |handler| {
                    handler.remove_dictation_result_placeholder(will_insert_result == YES);
                });
            }));
        }

        // UITextInput - attributedSubstringFromRange: (for copy/paste preview)
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn attributed_substring_from_range(
            this: &Object,
            _sel: Sel,
            range: *mut Object,
        ) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let Some((start, end)) = get_range_indices(range) else {
                    return std::ptr::null_mut();
                };

                let text = with_input_handler(this, |handler| {
                    let mut adjusted = None;
                    handler.text_for_range(start..end, &mut adjusted)
                })
                .flatten();

                match text {
                    Some(s) => unsafe {
                        let c_str = std::ffi::CString::new(s).unwrap_or_default();
                        let ns_string: *mut Object =
                            msg_send![class!(NSString), stringWithUTF8String: c_str.as_ptr()];
                        let attributed: *mut Object = msg_send![class!(NSAttributedString), alloc];
                        let attributed: *mut Object =
                            msg_send![attributed, initWithString: ns_string];
                        attributed
                    },
                    None => std::ptr::null_mut(),
                }
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => std::ptr::null_mut(),
            }
        }

        // ============================================
        // UITextInput Protocol - Position/Range Calculation
        // ============================================

        // UITextInput - positionFromPosition:offset:
        extern "C" fn position_from_position_offset(
            this: &Object,
            _sel: Sel,
            position: *mut Object,
            offset: isize,
        ) -> *mut Object {
            let Some(index) = get_position_index(position) else {
                return std::ptr::null_mut();
            };

            let Some(document_len) = text_input_document_utf16_len(this) else {
                return std::ptr::null_mut();
            };
            let Some(new_index) = offset_text_position_index(index, offset, document_len) else {
                return std::ptr::null_mut();
            };
            create_text_position(new_index)
        }

        // UITextInput - positionFromPosition:inDirection:offset:
        extern "C" fn position_from_position_in_direction(
            this: &Object,
            _sel: Sel,
            position: *mut Object,
            direction: i64, // UITextLayoutDirection
            offset: isize,
        ) -> *mut Object {
            let Some(index) = get_position_index(position) else {
                return std::ptr::null_mut();
            };

            // Direction: 0=right, 1=left, 2=up, 3=down
            // For now, treat up/down same as left/right (simplified)
            let effective_offset = match direction {
                1 | 2 => -offset.abs(), // left/up = negative
                _ => offset.abs(),      // right/down = positive
            };

            let Some(document_len) = text_input_document_utf16_len(this) else {
                return std::ptr::null_mut();
            };
            let Some(new_index) = offset_text_position_index(index, effective_offset, document_len)
            else {
                return std::ptr::null_mut();
            };
            create_text_position(new_index)
        }

        // UITextInput - textRangeFromPosition:toPosition:
        extern "C" fn text_range_from_position_to_position(
            this: &Object,
            _sel: Sel,
            from: *mut Object,
            to: *mut Object,
        ) -> *mut Object {
            let Some(start) = get_position_index(from) else {
                return std::ptr::null_mut();
            };
            let Some(end) = get_position_index(to) else {
                return std::ptr::null_mut();
            };

            if let Some(document_len) = text_input_document_utf16_len(this) {
                if start > document_len || end > document_len {
                    return std::ptr::null_mut();
                }
            }
            create_text_range(start.min(end), start.max(end))
        }

        // UITextInput - comparePosition:toPosition:
        extern "C" fn compare_position(
            _this: &Object,
            _sel: Sel,
            position: *mut Object,
            other: *mut Object,
        ) -> i64 {
            // NSComparisonResult
            let Some(a) = get_position_index(position) else {
                return 0; // NSOrderedSame
            };
            let Some(b) = get_position_index(other) else {
                return 0;
            };

            let result = match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,   // NSOrderedAscending
                std::cmp::Ordering::Equal => 0,   // NSOrderedSame
                std::cmp::Ordering::Greater => 1, // NSOrderedDescending
            };
            result
        }

        // UITextInput - offsetFromPosition:toPosition:
        extern "C" fn offset_from_position(
            _this: &Object,
            _sel: Sel,
            from: *mut Object,
            to: *mut Object,
        ) -> isize {
            let Some(start) = get_position_index(from) else {
                return 0;
            };
            let Some(end) = get_position_index(to) else {
                return 0;
            };

            let result = (end as isize) - (start as isize);
            result
        }

        // UITextInput - positionWithinRange:farthestInDirection:
        extern "C" fn position_within_range_farthest(
            this: &Object,
            _sel: Sel,
            range: *mut Object,
            direction: i64,
        ) -> *mut Object {
            let Some((start, end)) = get_range_indices(range) else {
                return std::ptr::null_mut();
            };
            if let Some(document_len) = text_input_document_utf16_len(this) {
                if start > document_len || end > document_len {
                    return std::ptr::null_mut();
                }
            }

            // Direction: 0=right, 1=left, 2=up, 3=down
            let index = match direction {
                1 | 2 => start, // left/up = start
                _ => end,       // right/down = end
            };

            create_text_position(index)
        }

        // UITextInput - positionWithinRange:atCharacterOffset:
        extern "C" fn position_within_range_at_character_offset(
            _this: &Object,
            _sel: Sel,
            range: *mut Object,
            offset: isize,
        ) -> *mut Object {
            let Some((start, end)) = get_range_indices(range) else {
                return std::ptr::null_mut();
            };
            let Some(index) = text_position_index_at_range_offset(start..end, offset) else {
                return std::ptr::null_mut();
            };
            create_text_position(index)
        }

        // UITextInput - characterOffsetOfPosition:withinRange:
        extern "C" fn character_offset_of_position_within_range(
            _this: &Object,
            _sel: Sel,
            position: *mut Object,
            range: *mut Object,
        ) -> isize {
            let Some(index) = get_position_index(position) else {
                return -1;
            };
            let Some((start, end)) = get_range_indices(range) else {
                return -1;
            };
            text_position_offset_in_range(index, start..end)
                .and_then(|offset| isize::try_from(offset).ok())
                .unwrap_or(-1)
        }

        // UITextInput - characterRangeByExtendingPosition:inDirection:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn character_range_by_extending(
            this: &Object,
            _sel: Sel,
            position: *mut Object,
            direction: i64,
        ) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let Some(index) = get_position_index(position) else {
                    return std::ptr::null_mut();
                };

                // Get document length
                let doc_end = with_input_handler(this, |handler| {
                    let mut adjusted = None;
                    handler
                        .text_for_range(0..usize::MAX, &mut adjusted)
                        .map(|s| s.encode_utf16().count())
                        .unwrap_or(0)
                })
                .unwrap_or(0);

                let Some(range) =
                    text_character_range_by_extending_position(index, direction, doc_end)
                else {
                    return std::ptr::null_mut();
                };
                create_text_range(range.start, range.end)
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => std::ptr::null_mut(),
            }
        }

        // ============================================
        // UITextInput Protocol - Geometry Methods
        // ============================================

        // UITextInput - caretRectForPosition:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn caret_rect_for_position(
            _this: &Object,
            _sel: Sel,
            _position: *mut Object,
        ) -> IOSCGRect {
            let default_rect = IOSCGRect::new(IOSCGPoint::new(0.0, 0.0), IOSCGSize::new(0.0, 0.0));

            // GPUI inputs paint their own caret; returning a native caret rect here
            // enables UIKit selection UI that current input handlers do not support.
            let result = panic::catch_unwind(AssertUnwindSafe(|| default_rect));

            match result {
                Ok(rect) => rect,
                Err(_) => default_rect,
            }
        }

        // UITextInput - firstRectForRange:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn first_rect_for_range(
            this: &Object,
            _sel: Sel,
            range: *mut Object,
        ) -> IOSCGRect {
            let default_rect =
                IOSCGRect::new(IOSCGPoint::new(20.0, 100.0), IOSCGSize::new(100.0, 20.0));
            let empty_rect = IOSCGRect::new(IOSCGPoint::new(0.0, 0.0), IOSCGSize::new(0.0, 0.0));

            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                // IME candidate UI needs firstRectForRange even when GPUI draws
                // its own editable caret and selection handles.
                let interaction_mode = active_text_interaction_mode(this);
                let input_native_selection_enabled = view_input_native_selection_enabled(this);
                let report_geometry = should_report_text_input_range_geometry(
                    interaction_mode,
                    input_native_selection_enabled,
                );
                if !report_geometry {
                    return empty_rect;
                }
                let Some((start, end)) = get_range_indices(range) else {
                    return default_rect;
                };
                let requested_range = start..end;
                let native_touch_selection = handles_native_touch_selection(
                    interaction_mode,
                    input_native_selection_enabled,
                );
                let effective_range = effective_native_selection_range_for_view(
                    this,
                    requested_range.clone(),
                    native_touch_selection,
                );

                let bounds = with_input_handler(this, |handler| {
                    handler.bounds_for_range(effective_range.clone())
                })
                .flatten();

                match bounds {
                    Some(b) => IOSCGRect::new(
                        IOSCGPoint::new(b.origin.x.as_f32() as f64, b.origin.y.as_f32() as f64),
                        IOSCGSize::new(b.size.width.as_f32() as f64, b.size.height.as_f32() as f64),
                    ),
                    None => default_rect,
                }
            }));

            match result {
                Ok(rect) => rect,
                Err(_) => default_rect,
            }
        }

        // UITextInput - selectionRectsForRange: (for selection handles)
        extern "C" fn selection_rects_for_range(
            this: &Object,
            _sel: Sel,
            range: *mut Object,
        ) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                let interaction_mode = active_text_interaction_mode(this);
                let input_native_selection_enabled = view_input_native_selection_enabled(this);
                let native_touch_selection = handles_native_touch_selection(
                    interaction_mode,
                    input_native_selection_enabled,
                );
                if !native_touch_selection {
                    return msg_send![class!(NSArray), array];
                }
                let Some((start, end)) = get_range_indices(range) else {
                    return msg_send![class!(NSArray), array];
                };
                let requested_range = start..end;
                let effective_range = effective_native_selection_range_for_view(
                    this,
                    requested_range.clone(),
                    native_touch_selection,
                );
                if effective_range.is_empty() {
                    return msg_send![class!(NSArray), array];
                }

                let rects = with_input_handler(this, |handler| {
                    handler.rects_for_range(effective_range.clone())
                })
                .unwrap_or_default();

                if rects.is_empty() {
                    return msg_send![class!(NSArray), array];
                }

                if native_touch_selection {
                    let window_ptr: *mut std::ffi::c_void = *this.get_ivar(GPUI_WINDOW_IVAR);
                    if !window_ptr.is_null() {
                        let window = &*(window_ptr as *const IosWindow);
                        // UIKit asks for rects before later handle touches arrive.
                        // Cache them so the GPUI touch bridge does not clear selection first.
                        window.cache_active_selection_geometry(SelectionGeometry {
                            range: effective_range.clone(),
                            bounds: with_input_handler(this, |handler| {
                                handler.bounds_for_range(effective_range.clone())
                            })
                            .flatten(),
                            rects: rects.iter().cloned().collect(),
                        });
                    }
                }

                let array: *mut Object = msg_send![class!(NSMutableArray), array];
                let rect_count = rects.len();
                for (ix, bounds) in rects.into_iter().enumerate() {
                    let rect = create_text_selection_rect(
                        bounds.origin.x.as_f32() as f64,
                        bounds.origin.y.as_f32() as f64,
                        bounds.size.width.as_f32() as f64,
                        bounds.size.height.as_f32() as f64,
                        ix == 0,
                        ix + 1 == rect_count,
                    );
                    let _: () = msg_send![array, addObject: rect];
                    let _: () = msg_send![rect, release];
                }
                array
            }));

            match result {
                Ok(array) => array,
                Err(_) => unsafe { msg_send![class!(NSArray), array] },
            }
        }

        // UITextInput - closestPositionToPoint:
        // IMPORTANT: Uses catch_unwind because panics cannot unwind through extern "C"
        extern "C" fn closest_position_to_point(
            this: &Object,
            _sel: Sel,
            point: IOSCGPoint,
        ) -> *mut Object {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                if !view_handles_native_touch_selection(this) {
                    return ptr::null_mut();
                }
                let point = Point::new(px(point.x as f32), px(point.y as f32));
                let has_active_selection = has_active_selection_geometry(this);
                let (direct_index, nearest_index) = with_input_handler(this, |handler| {
                    let direct_index = handler.character_index_for_point(point);
                    let nearest_index = if direct_index.is_none() && has_active_selection {
                        handler.nearest_character_index_for_point(point)
                    } else {
                        None
                    };
                    (direct_index, nearest_index)
                })
                .unwrap_or((None, None));
                let index = direct_index.or(nearest_index);

                if let Some(index) = index {
                    create_text_position(index)
                } else {
                    ptr::null_mut()
                }
            }));

            match result {
                Ok(ptr) => ptr,
                Err(_) => ptr::null_mut(),
            }
        }

        // UITextInput - closestPositionToPoint:withinRange:
        extern "C" fn closest_position_to_point_within_range(
            this: &Object,
            _sel: Sel,
            point: IOSCGPoint,
            range: *mut Object,
        ) -> *mut Object {
            let pos = closest_position_to_point(this, _sel, point);

            // Clamp to range if valid
            if let (Some(index), Some((start, end))) =
                (get_position_index(pos), get_range_indices(range))
            {
                let clamped = index.clamp(start, end);
                return create_text_position(clamped);
            }

            pos
        }

        // UITextInput - characterRangeAtPoint:
        extern "C" fn character_range_at_point(
            this: &Object,
            _sel: Sel,
            point: IOSCGPoint,
        ) -> *mut Object {
            if !view_handles_native_touch_selection(this) {
                return std::ptr::null_mut();
            }
            let pos = closest_position_to_point(this, _sel, point);
            let Some(index) = get_position_index(pos) else {
                return std::ptr::null_mut();
            };

            // Return single character range
            create_text_range(index, index.saturating_add(1))
        }

        // UITextInput - baseWritingDirectionForPosition:inDirection:
        extern "C" fn base_writing_direction(
            _this: &Object,
            _sel: Sel,
            _position: *mut Object,
            _direction: i64,
        ) -> i64 {
            0 // UITextWritingDirectionNatural / LeftToRight
        }

        // UITextInput - setBaseWritingDirection:forRange:
        extern "C" fn set_base_writing_direction(
            _this: &mut Object,
            _sel: Sel,
            _direction: i64,
            _range: *mut Object,
        ) {
            // No-op - we only support LTR for now
        }

        unsafe {
            // Add class method for layerClass
            decl.add_class_method(
                sel!(layerClass),
                layer_class as extern "C" fn(&Class, Sel) -> *const Class,
            );

            // Add touch handling instance methods
            decl.add_method(
                sel!(touchesBegan:withEvent:),
                touches_began as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            decl.add_method(
                sel!(touchesMoved:withEvent:),
                touches_moved as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            decl.add_method(
                sel!(touchesEnded:withEvent:),
                touches_ended as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            decl.add_method(
                sel!(touchesCancelled:withEvent:),
                touches_cancelled as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );

            // Add keyboard handling methods
            decl.add_method(
                sel!(canBecomeFirstResponder),
                can_become_first_responder as extern "C" fn(&Object, Sel) -> bool,
            );
            decl.add_method(
                sel!(becomeFirstResponder),
                become_first_responder as extern "C" fn(&mut Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(resignFirstResponder),
                resign_first_responder as extern "C" fn(&mut Object, Sel) -> BOOL,
            );

            // Add UITextInputTraits protocol methods
            decl.add_method(
                sel!(keyboardType),
                keyboard_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(returnKeyType),
                return_key_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(autocapitalizationType),
                autocapitalization_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(autocorrectionType),
                autocorrection_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(smartQuotesType),
                smart_quotes_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(smartDashesType),
                smart_dashes_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(smartInsertDeleteType),
                smart_insert_delete_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(spellCheckingType),
                spell_checking_type as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(inlinePredictionType),
                inline_prediction_type as extern "C" fn(&Object, Sel) -> i64,
            );

            // Add UIKeyInput protocol methods for text input
            decl.add_method(
                sel!(hasText),
                has_text as extern "C" fn(&Object, Sel) -> bool,
            );
            decl.add_method(
                sel!(insertText:),
                insert_text as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(insertText:alternatives:style:),
                insert_text_with_alternatives
                    as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object, i64),
            );
            decl.add_method(
                sel!(deleteBackward),
                delete_backward as extern "C" fn(&mut Object, Sel),
            );

            // Add hardware keyboard press handling
            decl.add_method(
                sel!(pressesBegan:withEvent:),
                presses_began as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            decl.add_method(
                sel!(pressesEnded:withEvent:),
                presses_ended as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );

            // Tap-to-dismiss: resign first responder (hides keyboard) when tapped.
            // Triggered by the UITapGestureRecognizer added in IosWindow::new.
            extern "C" fn dismiss_keyboard_on_tap(
                this: &mut Object,
                _sel: Sel,
                _recognizer: *mut Object,
            ) {
                unsafe {
                    let _: BOOL = msg_send![this, resignFirstResponder];
                }
            }
            decl.add_method(
                sel!(dismissKeyboardOnTap:),
                dismiss_keyboard_on_tap as extern "C" fn(&mut Object, Sel, *mut Object),
            );

            // ============================================
            // UITextInput Protocol - Core Properties
            // ============================================
            decl.add_method(
                sel!(beginningOfDocument),
                beginning_of_document as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(endOfDocument),
                end_of_document as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(selectedTextRange),
                selected_text_range as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(setSelectedTextRange:),
                set_selected_text_range as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(markedTextRange),
                marked_text_range as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(markedTextStyle),
                marked_text_style as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(setMarkedTextStyle:),
                set_marked_text_style as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(selectionAffinity),
                selection_affinity as extern "C" fn(&Object, Sel) -> i64,
            );
            decl.add_method(
                sel!(setSelectionAffinity:),
                set_selection_affinity as extern "C" fn(&mut Object, Sel, i64),
            );
            decl.add_method(
                sel!(inputDelegate),
                input_delegate as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(setInputDelegate:),
                set_input_delegate as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(tokenizer),
                tokenizer as extern "C" fn(&Object, Sel) -> *mut Object,
            );

            // ============================================
            // UITextInput Protocol - Text Manipulation
            // ============================================
            decl.add_method(
                sel!(textInRange:),
                text_in_range as extern "C" fn(&Object, Sel, *mut Object) -> *mut Object,
            );
            decl.add_method(
                sel!(replaceRange:withText:),
                replace_range_with_text
                    as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            decl.add_method(
                sel!(shouldChangeTextInRange:replacementText:),
                should_change_text_in_range
                    as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> BOOL,
            );
            decl.add_method(
                sel!(replaceRange:withAttributedText:),
                replace_range_with_attributed_text
                    as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            decl.add_method(
                sel!(setMarkedText:selectedRange:),
                set_marked_text as extern "C" fn(&mut Object, Sel, *mut Object, NSRange),
            );
            decl.add_method(
                sel!(setAttributedMarkedText:selectedRange:),
                set_attributed_marked_text as extern "C" fn(&mut Object, Sel, *mut Object, NSRange),
            );
            decl.add_method(
                sel!(unmarkText),
                unmark_text as extern "C" fn(&mut Object, Sel),
            );
            decl.add_method(
                sel!(insertDictationResult:),
                insert_dictation_result as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(dictationRecordingDidEnd),
                dictation_recording_did_end as extern "C" fn(&mut Object, Sel),
            );
            decl.add_method(
                sel!(dictationRecognitionFailed),
                dictation_recognition_failed as extern "C" fn(&mut Object, Sel),
            );
            decl.add_method(
                sel!(insertDictationResultPlaceholder),
                insert_dictation_result_placeholder as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(frameForDictationResultPlaceholder:),
                frame_for_dictation_result_placeholder
                    as extern "C" fn(&Object, Sel, *mut Object) -> IOSCGRect,
            );
            decl.add_method(
                sel!(removeDictationResultPlaceholder:willInsertResult:),
                remove_dictation_result_placeholder
                    as extern "C" fn(&mut Object, Sel, *mut Object, BOOL),
            );
            decl.add_method(
                sel!(attributedSubstringFromRange:),
                attributed_substring_from_range
                    as extern "C" fn(&Object, Sel, *mut Object) -> *mut Object,
            );

            // ============================================
            // UITextInput Protocol - Position/Range Calculation
            // ============================================
            decl.add_method(
                sel!(positionFromPosition:offset:),
                position_from_position_offset
                    as extern "C" fn(&Object, Sel, *mut Object, isize) -> *mut Object,
            );
            decl.add_method(
                sel!(positionFromPosition:inDirection:offset:),
                position_from_position_in_direction
                    as extern "C" fn(&Object, Sel, *mut Object, i64, isize) -> *mut Object,
            );
            decl.add_method(
                sel!(textRangeFromPosition:toPosition:),
                text_range_from_position_to_position
                    as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> *mut Object,
            );
            decl.add_method(
                sel!(comparePosition:toPosition:),
                compare_position as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> i64,
            );
            decl.add_method(
                sel!(offsetFromPosition:toPosition:),
                offset_from_position
                    as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> isize,
            );
            decl.add_method(
                sel!(positionWithinRange:farthestInDirection:),
                position_within_range_farthest
                    as extern "C" fn(&Object, Sel, *mut Object, i64) -> *mut Object,
            );
            decl.add_method(
                sel!(positionWithinRange:atCharacterOffset:),
                position_within_range_at_character_offset
                    as extern "C" fn(&Object, Sel, *mut Object, isize) -> *mut Object,
            );
            decl.add_method(
                sel!(characterOffsetOfPosition:withinRange:),
                character_offset_of_position_within_range
                    as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> isize,
            );
            decl.add_method(
                sel!(characterRangeByExtendingPosition:inDirection:),
                character_range_by_extending
                    as extern "C" fn(&Object, Sel, *mut Object, i64) -> *mut Object,
            );

            // ============================================
            // UITextInput Protocol - Geometry Methods
            // ============================================
            decl.add_method(
                sel!(caretRectForPosition:),
                caret_rect_for_position as extern "C" fn(&Object, Sel, *mut Object) -> IOSCGRect,
            );
            decl.add_method(
                sel!(firstRectForRange:),
                first_rect_for_range as extern "C" fn(&Object, Sel, *mut Object) -> IOSCGRect,
            );
            decl.add_method(
                sel!(selectionRectsForRange:),
                selection_rects_for_range
                    as extern "C" fn(&Object, Sel, *mut Object) -> *mut Object,
            );
            decl.add_method(
                sel!(closestPositionToPoint:),
                closest_position_to_point as extern "C" fn(&Object, Sel, IOSCGPoint) -> *mut Object,
            );
            decl.add_method(
                sel!(closestPositionToPoint:withinRange:),
                closest_position_to_point_within_range
                    as extern "C" fn(&Object, Sel, IOSCGPoint, *mut Object) -> *mut Object,
            );
            decl.add_method(
                sel!(characterRangeAtPoint:),
                character_range_at_point as extern "C" fn(&Object, Sel, IOSCGPoint) -> *mut Object,
            );
            decl.add_method(
                sel!(baseWritingDirectionForPosition:inDirection:),
                base_writing_direction as extern "C" fn(&Object, Sel, *mut Object, i64) -> i64,
            );
            decl.add_method(
                sel!(setBaseWritingDirection:forRange:),
                set_base_writing_direction as extern "C" fn(&mut Object, Sel, i64, *mut Object),
            );

            decl.add_method(
                sel!(inputAccessoryView),
                input_accessory_view as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(inputView),
                input_view as extern "C" fn(&Object, Sel) -> *mut Object,
            );

            decl.add_method(
                sel!(interactionShouldBegin:atPoint:),
                interaction_should_begin
                    as extern "C" fn(&Object, Sel, *mut Object, IOSCGPoint) -> BOOL,
            );
            // UIResponder - copy action and capability query for selection copy menu.
            decl.add_method(
                sel!(copy:),
                copy_action as extern "C" fn(&Object, Sel, *mut Object),
            );
            // UIResponder - hardware keyboard keys claimed back from UIKit.
            decl.add_method(
                sel!(keyCommands),
                key_commands as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(gpuiHandleKeyCommand:),
                handle_key_command as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(editMenuForTextRange:suggestedActions:),
                edit_menu_for_text_range
                    as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> *mut Object,
            );
            decl.add_method(
                sel!(canPerformAction:withSender:),
                can_perform_action as extern "C" fn(&mut Object, Sel, Sel, *mut Object) -> BOOL,
            );
        }

        decl.register();
    });

    class!(GPUIMetalView)
}

/// Store the UIView pointer to use as `inputAccessoryView` on GPUIMetalView.
///
/// Called from `gpui_ios_set_keyboard_accessory_view` in ffi.rs.
pub(super) fn set_keyboard_accessory_view(view_ptr: *mut c_void) {
    KEYBOARD_ACCESSORY_VIEW.store(view_ptr as usize, std::sync::atomic::Ordering::Relaxed);
}

/// Track whether the iOS software keyboard is visible.
///
/// Called from ObjC keyboard notifications (`keyboardWillShow` / `keyboardWillHide`).
/// When false, `inputAccessoryView` returns nil so the toolbar doesn't appear
/// without a software keyboard (e.g. on simulator with hardware keyboard attached).
pub(super) fn set_software_keyboard_visible(visible: bool) {
    SOFTWARE_KEYBOARD_VISIBLE.store(visible, std::sync::atomic::Ordering::Relaxed);
}

fn software_keyboard_visible() -> bool {
    SOFTWARE_KEYBOARD_VISIBLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Handle touch events from the GPUIMetalView
fn handle_touches(view: &mut Object, touches: *mut Object, event: *mut Object) {
    unsafe {
        // Get the window pointer from the view's ivar
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            log::warn!("GPUI iOS: Touch event but no window pointer set");
            return;
        }

        let window = &*(window_ptr as *const IosWindow);

        // Get all touches from the set
        let all_touches: *mut Object = msg_send![touches, allObjects];
        let count: usize = msg_send![all_touches, count];

        for i in 0..count {
            let touch: *mut Object = msg_send![all_touches, objectAtIndex: i];
            window.handle_touch(touch, event);
        }
    }
}

/// Handle hardware keyboard press events from the GPUIMetalView
fn handle_presses(view: &mut Object, presses: *mut Object, is_key_down: bool) {
    unsafe {
        // Get the window pointer from the view's ivar
        let window_ptr: *mut std::ffi::c_void = *view.get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            return;
        }

        let window = &*(window_ptr as *const IosWindow);

        // Get all presses from the set
        let all_presses: *mut Object = msg_send![presses, allObjects];
        let count: usize = msg_send![all_presses, count];

        for i in 0..count {
            let press: *mut Object = msg_send![all_presses, objectAtIndex: i];

            // Get the UIKey from the press
            let key: *mut Object = msg_send![press, key];
            if key.is_null() {
                continue;
            }

            // Get key code
            let key_code: i64 = msg_send![key, keyCode];

            // Get modifier flags
            // Raw UIKeyModifierFlags: `handle_key_event` decodes them with
            // `modifier_flags_to_modifiers`, so they must not be re-encoded here.
            let modifier_flags: u64 = msg_send![key, modifierFlags];
            let modifiers = modifier_flags as u32;

            // When the software keyboard is visible, backspace/delete are delivered
            // via deleteBackward. Keep skipping here to avoid duplicate deletion.
            // When the software keyboard is hidden (hardware keyboard path), do not
            // skip so key-repeat can flow through normal key event dispatch.
            if (key_code == 0x2A || key_code == 0x4C)
                && SOFTWARE_KEYBOARD_VISIBLE.load(std::sync::atomic::Ordering::Relaxed)
            {
                continue;
            }

            // Skip printable keys without ctrl/alt/cmd modifiers.
            // These arrive through insertText; handling them here too would
            // produce duplicate input (especially visible on simulator with
            // hardware keyboard). Only non-printable keys (arrows, escape,
            // function keys, etc.) and modified keys (ctrl+c) go through here.
            let has_action_modifier = modifiers
                & (UI_KEY_MODIFIER_CONTROL | UI_KEY_MODIFIER_ALTERNATE | UI_KEY_MODIFIER_COMMAND)
                != 0;
            if !has_action_modifier && !is_non_printable_key(key_code) {
                ios_log_cstr(
                    c"GPUI iOS: handle_presses - skipping printable key (handled by insertText)",
                );
                continue;
            }

            window.handle_key_event(key_code as u32, modifiers, is_key_down);
        }
    }
}

/// Returns true for key codes that do NOT generate `insertText` calls
/// (non-printable keys). Only these should be handled in `pressesBegan`.
fn is_non_printable_key(key_code: i64) -> bool {
    matches!(
        key_code,
        0x29          // Escape
        | 0x39          // CapsLock
        | 0x3A..=0x45  // F1-F12
        | 0x46..=0x48  // PrintScreen, ScrollLock, Pause
        | 0x49          // Insert
        | 0x4A          // Home
        | 0x4B          // PageUp
        | 0x4D          // End
        | 0x4E          // PageDown
        | 0x4F..=0x52  // Arrow keys (Right, Left, Down, Up)
        | 0x68..=0x73  // F13-F24
        | 0xE0..=0xE7  // Modifier keys
    )
}

/// iOS Window backed by UIWindow + UIViewController.
pub(crate) struct IosWindow {
    /// Handle used by GPUI to identify this window
    handle: AnyWindowHandle,
    /// The UIWindow object
    window: *mut Object,
    /// The UIViewController
    view_controller: *mut Object,
    /// The Metal-backed UIView (also handles UITextInput)
    view: *mut Object,
    /// Current bounds in pixels
    bounds: Cell<Bounds<Pixels>>,
    /// Scale factor
    scale_factor: Cell<f32>,
    /// Appearance (light/dark mode)
    appearance: Cell<WindowAppearance>,
    /// Input handler for text input
    input_handler: RefCell<Option<PlatformInputHandler>>,
    /// Stable mirror used by UIKit callbacks while GPUI is between frame phases.
    callback_input_handler: RefCell<Option<PlatformInputHandler>>,
    /// Selection-only handler for non-editable text surfaces.
    selection_handler: RefCell<Option<PlatformInputHandler>>,
    /// Stable mirror used by UIKit callbacks for selection-only surfaces.
    callback_selection_handler: RefCell<Option<PlatformInputHandler>>,
    /// Whether the active editable handler currently accepts inserted text.
    input_accepts_text_input: Cell<bool>,
    /// Whether the active input handler belongs to a manual-focus surface.
    input_uses_manual_focus: Cell<bool>,
    /// Cached policy bit from the active input handler. UIKit callbacks use
    /// this to distinguish normal editable input from editable surfaces that
    /// expose their own native selection geometry.
    input_native_selection_enabled: Cell<bool>,
    /// Cached policy bit from the active editable handler. When true, UIKit may
    /// expose the app-provided inputAccessoryView for this keyboard session.
    input_keyboard_accessory_enabled: Cell<bool>,
    /// Cached text input traits for the active editable handler.
    input_text_input_traits: Cell<PlatformTextInputTraits>,
    /// Whether UIKit should keep the software keyboard up for the active input handler.
    keyboard_session_requested: Cell<bool>,
    /// Weak text input delegate assigned by UIKit.
    input_delegate: Cell<*mut Object>,
    /// Native text interactions retained for the modes that need UIKit selection UI.
    editable_text_interaction: *mut Object,
    noneditable_text_interaction: *mut Object,
    /// GPUI's desired interaction mode from the current input/selection handlers.
    target_text_interaction_mode: Cell<i8>,
    /// The active route for UIKit text callbacks.
    active_text_interaction_mode: Cell<i8>,
    /// Last UIKit tap count observed for the primary touch.
    last_touch_tap_count: Cell<usize>,
    selectable_text_hit_regions: RefCell<SmallVec<[SelectableTextHitRegion; 8]>>,
    last_selection_geometry: RefCell<Option<SelectionGeometry>>,
    /// Callback for frame requests
    /// Note: pub(super) to allow ffi.rs to access this for the display link callback
    pub(super) request_frame_callback: RefCell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    /// Callback for input events
    input_callback: RefCell<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    /// Callback for active status changes
    active_status_callback: RefCell<Option<Box<dyn FnMut(bool)>>>,
    /// Callback for hover status changes (not really applicable on iOS)
    hover_status_callback: RefCell<Option<Box<dyn FnMut(bool)>>>,
    /// Callback for resize events
    resize_callback: RefCell<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    /// Callback for move events (not applicable on iOS)
    moved_callback: RefCell<Option<Box<dyn FnMut()>>>,
    /// Callback for should close
    should_close_callback: RefCell<Option<Box<dyn FnMut() -> bool>>>,
    /// Callback for hit test
    hit_test_callback: RefCell<Option<Box<dyn FnMut() -> Option<WindowControlArea>>>>,
    /// Callback for close
    close_callback: RefCell<Option<Box<dyn FnOnce()>>>,
    /// Callback for appearance changes
    appearance_changed_callback: RefCell<Option<Box<dyn FnMut()>>>,
    /// Current mouse position (from touch)
    mouse_position: Cell<Point<Pixels>>,
    /// Current modifiers
    modifiers: Cell<Modifiers>,
    /// Metal renderer for GPU rendering
    renderer: RefCell<metal_renderer::Renderer>,
    /// Track if a touch is currently pressed
    touch_pressed: Cell<bool>,
    /// Raw UITouch pointer value of the primary (first-down) finger.
    /// 0 means no primary touch is active. Secondary touches are suppressed
    /// to prevent double-scroll events when two fingers are on screen.
    primary_touch_ptr: Cell<usize>,
    /// Exponentially-smoothed touch velocity (logical px/s) for fling detection
    touch_velocity_x: Cell<f32>,
    touch_velocity_y: Cell<f32>,
    touch_last_time: RefCell<Option<std::time::Instant>>,
    primary_touch_began_at: RefCell<Option<std::time::Instant>>,
    /// Active fling state after finger lift
    fling: RefCell<Option<TouchFling>>,
    /// Position where the primary touch began (used as scroll event origin so
    /// DrawerHost's edge-zone check reflects the gesture start, not current pos)
    touch_down_position: Cell<Point<Pixels>>,
    /// True once a pointer handler claims the active touch stream and suppresses
    /// the platform's synthetic scroll events for that stream.
    touch_scroll_suppressed: Cell<bool>,
    /// Timestamp of the last dispatched Moved event (UITouch.timestamp, seconds since boot).
    /// Used to skip duplicate Moved callbacks that UIKit fires for the same touch sample.
    last_move_ts: Cell<f64>,
}

// Required for raw_window_handle
unsafe impl Send for IosWindow {}
unsafe impl Sync for IosWindow {}

impl IosWindow {
    pub fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        renderer_context: metal_renderer::Context,
    ) -> Result<Self> {
        // Pre-register text input classes early to avoid race conditions.
        // iOS may query UITextInput methods (markedTextRange, selectedTextRange)
        // immediately when the keyboard is shown, before our classes are ready.
        text_input::ensure_text_input_classes_registered();

        // Create the window on the main screen
        let screen = IosDisplay::main();
        let screen_bounds = screen.bounds();
        let scale_factor = screen.scale();

        unsafe {
            // Create UIWindow
            let screen_obj: *mut Object = msg_send![class!(UIScreen), mainScreen];
            let screen_bounds_cg: CGRect = msg_send![screen_obj, bounds];
            let window: *mut Object = msg_send![class!(UIWindow), alloc];
            let window: *mut Object = msg_send![window, initWithFrame: screen_bounds_cg];

            // Create UIViewController
            let view_controller: *mut Object = msg_send![class!(UIViewController), alloc];
            let view_controller: *mut Object = msg_send![view_controller, init];

            // Create our custom Metal view using the registered class
            let metal_view_class = register_metal_view_class();
            let view: *mut Object = msg_send![metal_view_class, alloc];
            let view: *mut Object = msg_send![view, initWithFrame: screen_bounds_cg];

            // Configure the Metal layer
            let layer: *mut Object = msg_send![view, layer];

            // Get the Metal device using the Metal framework function
            #[link(name = "Metal", kind = "framework")]
            unsafe extern "C" {
                fn MTLCreateSystemDefaultDevice() -> *mut Object;
            }
            let device = MTLCreateSystemDefaultDevice();
            if !device.is_null() {
                let _: () = msg_send![layer, setDevice: device];
            }
            let _: () = msg_send![layer, setPixelFormat: 80_u64]; // MTLPixelFormatBGRA8Unorm
            let _: () = msg_send![layer, setFramebufferOnly: NO];
            let scale: CGFloat = msg_send![screen_obj, scale];
            let _: () = msg_send![layer, setContentsScale: scale];
            let drawable_size = CGSize {
                width: screen_bounds_cg.size.width * scale,
                height: screen_bounds_cg.size.height * scale,
            };
            let _: () = msg_send![layer, setDrawableSize: drawable_size];

            // Enable user interaction on the Metal view for touch handling
            let _: () = msg_send![view, setUserInteractionEnabled: YES];
            let _: () = msg_send![view, setMultipleTouchEnabled: YES];

            let editable_text_interaction: *mut Object =
                msg_send![class!(UITextInteraction), textInteractionForMode: 0_i64];
            let editable_text_interaction: *mut Object =
                msg_send![editable_text_interaction, retain];
            let _: () = msg_send![editable_text_interaction, setTextInput: view];
            let _: () = msg_send![editable_text_interaction, setDelegate: view];
            let noneditable_text_interaction: *mut Object =
                msg_send![class!(UITextInteraction), textInteractionForMode: 1_i64];
            let noneditable_text_interaction: *mut Object =
                msg_send![noneditable_text_interaction, retain];
            let _: () = msg_send![noneditable_text_interaction, setTextInput: view];
            let _: () = msg_send![noneditable_text_interaction, setDelegate: view];

            // Set the view as the view controller's view
            let _: () = msg_send![view_controller, setView: view];

            // Set the root view controller
            let _: () = msg_send![window, setRootViewController: view_controller];

            // Make the window visible
            let _: () = msg_send![window, makeKeyAndVisible];

            // Create the Metal renderer
            // Note: Blade expects size in pixels (device pixels), not points
            let renderer = metal_renderer::new_renderer(
                renderer_context,
                window as *mut c_void,
                view as *mut c_void,
                gpui::Size {
                    width: drawable_size.width as f32,
                    height: drawable_size.height as f32,
                },
                false, // not transparent
            );

            let ios_window = Self {
                handle,
                window,
                view_controller,
                view,
                bounds: Cell::new(screen_bounds),
                scale_factor: Cell::new(scale_factor),
                appearance: Cell::new(WindowAppearance::Light),
                input_handler: RefCell::new(None),
                callback_input_handler: RefCell::new(None),
                selection_handler: RefCell::new(None),
                callback_selection_handler: RefCell::new(None),
                input_accepts_text_input: Cell::new(false),
                input_uses_manual_focus: Cell::new(false),
                input_native_selection_enabled: Cell::new(false),
                input_keyboard_accessory_enabled: Cell::new(false),
                input_text_input_traits: Cell::new(PlatformTextInputTraits::default()),
                keyboard_session_requested: Cell::new(false),
                input_delegate: Cell::new(ptr::null_mut()),
                editable_text_interaction,
                noneditable_text_interaction,
                target_text_interaction_mode: Cell::new(TEXT_INTERACTION_NONE),
                active_text_interaction_mode: Cell::new(TEXT_INTERACTION_NONE),
                last_touch_tap_count: Cell::new(0),
                selectable_text_hit_regions: RefCell::new(SmallVec::new()),
                last_selection_geometry: RefCell::new(None),
                request_frame_callback: RefCell::new(None),
                input_callback: RefCell::new(None),
                active_status_callback: RefCell::new(None),
                hover_status_callback: RefCell::new(None),
                resize_callback: RefCell::new(None),
                moved_callback: RefCell::new(None),
                should_close_callback: RefCell::new(None),
                hit_test_callback: RefCell::new(None),
                close_callback: RefCell::new(None),
                appearance_changed_callback: RefCell::new(None),
                mouse_position: Cell::new(Point::default()),
                modifiers: Cell::new(Modifiers::default()),
                renderer: RefCell::new(renderer),
                touch_pressed: Cell::new(false),
                primary_touch_ptr: Cell::new(0),
                touch_velocity_x: Cell::new(0.0),
                touch_velocity_y: Cell::new(0.0),
                touch_last_time: RefCell::new(None),
                primary_touch_began_at: RefCell::new(None),
                fling: RefCell::new(None),
                touch_down_position: Cell::new(Point::default()),
                touch_scroll_suppressed: Cell::new(false),
                last_move_ts: Cell::new(0.0),
            };

            Ok(ios_window)
        }
    }

    pub fn new_embedded(
        handle: AnyWindowHandle,
        _params: WindowParams,
        renderer_context: metal_renderer::Context,
        parent_view: *mut Object,
        width_pts: f32,
        height_pts: f32,
    ) -> Result<Self> {
        text_input::ensure_text_input_classes_registered();

        let screen = IosDisplay::main();
        let scale_factor = screen.scale();
        let embedded_bounds = Bounds {
            origin: Point::default(),
            size: size(px(width_pts), px(height_pts)),
        };

        unsafe {
            let frame = CGRect {
                origin: core_graphics::geometry::CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: width_pts as CGFloat,
                    height: height_pts as CGFloat,
                },
            };

            let view_controller: *mut Object = msg_send![class!(UIViewController), alloc];
            let view_controller: *mut Object = msg_send![view_controller, init];

            let metal_view_class = register_metal_view_class();
            let view: *mut Object = msg_send![metal_view_class, alloc];
            let view: *mut Object = msg_send![view, initWithFrame: frame];

            let layer: *mut Object = msg_send![view, layer];
            #[link(name = "Metal", kind = "framework")]
            unsafe extern "C" {
                fn MTLCreateSystemDefaultDevice() -> *mut Object;
            }
            let device = MTLCreateSystemDefaultDevice();
            if !device.is_null() {
                let _: () = msg_send![layer, setDevice: device];
            }
            let _: () = msg_send![layer, setPixelFormat: 80_u64];
            let _: () = msg_send![layer, setFramebufferOnly: NO];

            let screen_obj: *mut Object = msg_send![class!(UIScreen), mainScreen];
            let scale: CGFloat = msg_send![screen_obj, scale];
            let _: () = msg_send![layer, setContentsScale: scale];
            let drawable_size = CGSize {
                width: width_pts as CGFloat * scale,
                height: height_pts as CGFloat * scale,
            };
            let _: () = msg_send![layer, setDrawableSize: drawable_size];
            let _: () = msg_send![view, setUserInteractionEnabled: YES];
            let _: () = msg_send![view, setMultipleTouchEnabled: YES];
            let editable_text_interaction: *mut Object =
                msg_send![class!(UITextInteraction), textInteractionForMode: 0_i64];
            let editable_text_interaction: *mut Object =
                msg_send![editable_text_interaction, retain];
            let _: () = msg_send![editable_text_interaction, setTextInput: view];
            let _: () = msg_send![editable_text_interaction, setDelegate: view];
            let noneditable_text_interaction: *mut Object =
                msg_send![class!(UITextInteraction), textInteractionForMode: 1_i64];
            let noneditable_text_interaction: *mut Object =
                msg_send![noneditable_text_interaction, retain];
            let _: () = msg_send![noneditable_text_interaction, setTextInput: view];
            let _: () = msg_send![noneditable_text_interaction, setDelegate: view];
            let _: () = msg_send![view_controller, setView: view];

            // Flexible width/height so UIKit layout changes on the parent propagate to the
            // attached Metal view without requiring Auto Layout bindings from Rust.
            let flexible_width: u64 = 1 << 1;
            let flexible_height: u64 = 1 << 4;
            let _: () = msg_send![view, setAutoresizingMask: flexible_width | flexible_height];
            let _: () = msg_send![parent_view, addSubview: view];

            let renderer = metal_renderer::new_renderer(
                renderer_context,
                ptr::null_mut(),
                view as *mut c_void,
                gpui::Size {
                    width: drawable_size.width as f32,
                    height: drawable_size.height as f32,
                },
                true,
            );

            Ok(Self {
                handle,
                window: ptr::null_mut(),
                view_controller,
                view,
                bounds: Cell::new(embedded_bounds),
                scale_factor: Cell::new(scale_factor),
                appearance: Cell::new(WindowAppearance::Light),
                input_handler: RefCell::new(None),
                callback_input_handler: RefCell::new(None),
                selection_handler: RefCell::new(None),
                callback_selection_handler: RefCell::new(None),
                input_accepts_text_input: Cell::new(false),
                input_uses_manual_focus: Cell::new(false),
                input_native_selection_enabled: Cell::new(false),
                input_keyboard_accessory_enabled: Cell::new(false),
                input_text_input_traits: Cell::new(PlatformTextInputTraits::default()),
                keyboard_session_requested: Cell::new(false),
                input_delegate: Cell::new(ptr::null_mut()),
                editable_text_interaction,
                noneditable_text_interaction,
                target_text_interaction_mode: Cell::new(TEXT_INTERACTION_NONE),
                active_text_interaction_mode: Cell::new(TEXT_INTERACTION_NONE),
                last_touch_tap_count: Cell::new(0),
                selectable_text_hit_regions: RefCell::new(SmallVec::new()),
                last_selection_geometry: RefCell::new(None),
                request_frame_callback: RefCell::new(None),
                input_callback: RefCell::new(None),
                active_status_callback: RefCell::new(None),
                hover_status_callback: RefCell::new(None),
                resize_callback: RefCell::new(None),
                moved_callback: RefCell::new(None),
                should_close_callback: RefCell::new(None),
                hit_test_callback: RefCell::new(None),
                close_callback: RefCell::new(None),
                appearance_changed_callback: RefCell::new(None),
                mouse_position: Cell::new(Point::default()),
                modifiers: Cell::new(Modifiers::default()),
                renderer: RefCell::new(renderer),
                touch_pressed: Cell::new(false),
                primary_touch_ptr: Cell::new(0),
                touch_velocity_x: Cell::new(0.0),
                touch_velocity_y: Cell::new(0.0),
                touch_last_time: RefCell::new(None),
                primary_touch_began_at: RefCell::new(None),
                fling: RefCell::new(None),
                touch_down_position: Cell::new(Point::default()),
                touch_scroll_suppressed: Cell::new(false),
                last_move_ts: Cell::new(0.0),
            })
        }
    }

    pub fn attach_to_parent(&self, parent_view: *mut Object, width_pts: f32, height_pts: f32) {
        unsafe {
            let frame = CGRect {
                origin: core_graphics::geometry::CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: width_pts as CGFloat,
                    height: height_pts as CGFloat,
                },
            };
            let _: () = msg_send![self.view, setFrame: frame];
            let flexible_width: u64 = 1 << 1;
            let flexible_height: u64 = 1 << 4;
            let _: () = msg_send![self.view, setAutoresizingMask: flexible_width | flexible_height];
            let superview: *mut Object = msg_send![self.view, superview];
            if superview != parent_view {
                if !superview.is_null() {
                    let _: () = msg_send![self.view, removeFromSuperview];
                }
                let _: () = msg_send![parent_view, addSubview: self.view];
            }
        }
        self.handle_resize(width_pts, height_pts);
    }

    pub fn detach_from_parent(&self) {
        unsafe {
            let superview: *mut Object = msg_send![self.view, superview];
            if !superview.is_null() {
                let _: () = msg_send![self.view, removeFromSuperview];
            }
        }
    }

    /// Register this window with the FFI layer after it's been stored.
    /// This must be called after the window is placed at a stable address
    /// (e.g., in a Box or Arc).
    pub(crate) fn register_with_ffi(&self) {
        super::ffi::register_window(self as *const Self);

        // Set the window pointer on the view so touch events can find us
        unsafe {
            let window_ptr = self as *const Self as *mut std::ffi::c_void;
            (*self.view).set_ivar(GPUI_WINDOW_IVAR, window_ptr);
            log::info!(
                "GPUI iOS: Set window pointer {:p} on view {:p}",
                window_ptr,
                self.view
            );
        }
    }

    /// Handle a touch event from UIKit
    /// Get the UIWindow pointer for this window.
    pub fn ui_window(&self) -> *mut Object {
        self.window
    }

    fn input_responder_view(&self) -> *mut Object {
        self.view
    }

    fn active_first_responder_view(&self) -> Option<*mut Object> {
        unsafe {
            let view_is_fr: BOOL = msg_send![self.view, isFirstResponder];
            if view_is_fr == YES {
                return Some(self.view);
            }
        }
        None
    }

    /// Applies the active text mode without changing GPUI's desired mode.
    ///
    /// Editable input installs UIKit's text interaction for keyboard, IME, and
    /// dictation plumbing. The delegate only allows touch selection when the
    /// active input handler explicitly opts into native selection geometry.
    fn install_text_interaction_mode(&self, mode: i8) {
        let previous_mode = self.active_text_interaction_mode.get();
        if previous_mode == mode {
            return;
        }
        if previous_mode == TEXT_INTERACTION_NONEDITABLE
            || previous_mode == TEXT_INTERACTION_EDITABLE
        {
            self.last_selection_geometry.borrow_mut().take();
        }

        unsafe {
            if previous_mode == TEXT_INTERACTION_EDITABLE {
                let _: () = msg_send![self.view, removeInteraction: self.editable_text_interaction];
            } else if previous_mode == TEXT_INTERACTION_NONEDITABLE {
                let _: () =
                    msg_send![self.view, removeInteraction: self.noneditable_text_interaction];
            }

            match mode {
                TEXT_INTERACTION_EDITABLE => {
                    let _: () =
                        msg_send![self.view, addInteraction: self.editable_text_interaction];
                    allow_gpui_touch_delivery_while_text_interaction_recognizes(self.view);
                }
                TEXT_INTERACTION_NONEDITABLE => {
                    let _: () =
                        msg_send![self.view, addInteraction: self.noneditable_text_interaction];
                    allow_gpui_touch_delivery_while_text_interaction_recognizes(self.view);
                }
                _ => {}
            }
        }

        self.active_text_interaction_mode.set(mode);
    }

    /// Applies the desired interaction mode to UIKit for the current responder state.
    fn sync_text_interaction_for_current_responder_state(&self) {
        let target_interaction_mode = self.target_text_interaction_mode.get();
        let has_selection_handler = self.selection_handler.borrow().is_some();
        let input_native_selection_enabled = self.input_native_selection_enabled.get();
        let is_first_responder = self.active_first_responder_view().is_some();
        let active_mode = active_text_interaction_mode_for_state(
            target_interaction_mode,
            has_selection_handler,
            input_native_selection_enabled,
            is_first_responder,
        );
        self.install_text_interaction_mode(active_mode);
    }

    fn resign_first_responder_preserving_handler(&self) {
        let responder = self.active_first_responder_view();
        let Some(responder) = responder else {
            self.sync_text_interaction_for_current_responder_state();
            return;
        };
        let _: BOOL = unsafe { msg_send![responder, resignFirstResponder] };
        self.sync_text_interaction_for_current_responder_state();
    }

    fn reload_text_input_views_if_first_responder(&self) {
        if let Some(responder) = self.active_first_responder_view() {
            let _: () = unsafe { msg_send![responder, reloadInputViews] };
        }
    }

    fn refresh_text_input_state(&self) {
        let has_input_handler = self.input_handler.borrow().is_some();
        let has_selection_handler = self.selection_handler.borrow().is_some();
        let input_accepts_text_input = self.input_accepts_text_input.get();
        let input_native_selection_enabled = self.input_native_selection_enabled.get();
        let input_uses_manual_focus = self.input_uses_manual_focus.get();
        let keyboard_session_requested = self.keyboard_session_requested.get();
        let software_keyboard_visible = self.is_software_keyboard_visible();

        let plan = text_input_refresh_plan(
            has_input_handler,
            has_selection_handler,
            input_accepts_text_input,
            input_native_selection_enabled,
            input_uses_manual_focus,
            keyboard_session_requested,
            software_keyboard_visible,
        );
        self.keyboard_session_requested
            .set(plan.keyboard_session_requested);

        self.target_text_interaction_mode
            .set(plan.target_interaction_mode);

        match plan.responder_action {
            TextInputResponderAction::ShowKeyboard => {
                self.show_keyboard();
            }
            TextInputResponderAction::ResignActiveResponder => {
                self.resign_first_responder_preserving_handler();
            }
            TextInputResponderAction::None => {}
        }

        self.sync_text_interaction_for_current_responder_state();
    }

    fn refresh_selection_geometry(&self) {
        if !handles_native_touch_selection(
            self.active_text_interaction_mode.get(),
            self.input_native_selection_enabled.get(),
        ) {
            let cleared = self.last_selection_geometry.borrow_mut().take().is_some();
            if cleared {
                let view = unsafe { &*self.view };
                notify_text_and_selection_change(view, || {});
            }
            return;
        }

        let view = unsafe { &*self.view };
        let selection =
            with_input_handler(view, |handler| handler.selected_text_range(false)).flatten();
        let Some(selection) = selection else {
            return;
        };
        if selection.range.is_empty() {
            self.last_selection_geometry.borrow_mut().take();
            return;
        }

        let Some((bounds, rects)) = with_input_handler(view, |handler| {
            (
                handler.bounds_for_range(selection.range.clone()),
                handler.rects_for_range(selection.range.clone()),
            )
        }) else {
            return;
        };
        let geometry = SelectionGeometry {
            range: selection.range.clone(),
            bounds,
            rects: rects.iter().cloned().collect(),
        };
        let geometry_changed = self.last_selection_geometry.borrow().as_ref() != Some(&geometry);

        if geometry_changed {
            *self.last_selection_geometry.borrow_mut() = Some(geometry);
            notify_text_and_selection_change(view, || {});
        }
    }

    fn cache_active_selection_range(&self, range: Range<usize>) {
        if range.is_empty() {
            self.last_selection_geometry.borrow_mut().take();
            return;
        }

        let mut last_selection_geometry = self.last_selection_geometry.borrow_mut();
        if last_selection_geometry
            .as_ref()
            .is_some_and(|geometry| geometry.range == range)
        {
            return;
        }
        *last_selection_geometry = Some(SelectionGeometry {
            range,
            bounds: None,
            rects: Vec::new(),
        });
    }

    fn cache_active_selection_geometry(&self, geometry: SelectionGeometry) {
        if geometry.range.is_empty() {
            self.last_selection_geometry.borrow_mut().take();
            return;
        }

        let mut last_selection_geometry = self.last_selection_geometry.borrow_mut();
        if last_selection_geometry.as_ref() == Some(&geometry) {
            return;
        }
        *last_selection_geometry = Some(geometry);
    }

    fn point_hits_selectable_text(&self, point: Point<Pixels>) -> bool {
        self.selectable_text_hit_regions
            .borrow()
            .iter()
            .any(|region| region.contains_text(point))
    }

    fn point_hits_selectable_area(&self, point: Point<Pixels>) -> bool {
        self.selectable_text_hit_regions
            .borrow()
            .iter()
            .any(|region| region.contains_selection_area(point))
    }

    fn point_hits_active_selection_geometry(&self, point: Point<Pixels>) -> bool {
        self.last_selection_geometry
            .borrow()
            .as_ref()
            .is_some_and(|geometry| selection_geometry_contains_interaction_point(geometry, point))
    }

    fn clear_active_text_selection(&self, clear_handler: bool) {
        let had_geometry = self.last_selection_geometry.borrow_mut().take().is_some();
        if !had_geometry && !clear_handler {
            return;
        }

        let view = unsafe { &*self.view };
        notify_selection_change(view, || {
            if clear_handler {
                let _ = with_input_handler(view, |handler| {
                    handler.clear_selected_text_range();
                });
            }
        });
    }

    pub fn handle_touch(&self, touch: *mut Object, _event: *mut Object) {
        let touch_ptr = touch as usize;
        let position = touch_location_in_view(touch, self.view);
        let phase = touch_phase(touch);
        let modifiers = self.modifiers.get();

        self.mouse_position.set(position);

        if phase == UITouchPhase::Stationary {
            return;
        }

        let prev_position = touch_previous_location_in_view(touch, self.view);
        let delta = Point::new(position.x - prev_position.x, position.y - prev_position.y);

        let platform_input = match phase {
            UITouchPhase::Began => {
                if self.primary_touch_ptr.get() == 0 {
                    // First finger down — register as primary and start tracking.
                    self.primary_touch_ptr.set(touch_ptr);
                    self.touch_down_position.set(position);
                    self.touch_velocity_x.set(0.0);
                    self.touch_velocity_y.set(0.0);
                    self.last_move_ts.set(0.0);
                    let tap_count: usize = unsafe { msg_send![touch, tapCount] };
                    self.last_touch_tap_count.set(tap_count);
                    self.touch_scroll_suppressed.set(false);
                    let now = std::time::Instant::now();
                    *self.touch_last_time.borrow_mut() = Some(now);
                    *self.primary_touch_began_at.borrow_mut() = Some(now);
                    *self.fling.borrow_mut() = None;
                    let had_selection = self.last_selection_geometry.borrow().is_some();
                    let hit_text = self.point_hits_selectable_text(position);
                    let hit_registered_selection_area = self.point_hits_selectable_area(position);
                    let hit_active_selection_area =
                        self.point_hits_active_selection_geometry(position);
                    let hit_selection_area =
                        hit_registered_selection_area || hit_active_selection_area;
                    self.touch_pressed.set(true);
                    if should_dismiss_selection_for_touch(
                        had_selection,
                        hit_text,
                        hit_selection_area,
                    ) {
                        self.clear_active_text_selection(true);
                    }
                    if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                        callback(touch_began_to_pointer_down(
                            position,
                            touch_ptr as u64,
                            modifiers,
                        ));
                    }
                    None
                } else {
                    // Secondary finger down — cancel fling/velocity to prevent
                    // cross-contamination with the primary finger's scroll state.
                    *self.fling.borrow_mut() = None;
                    self.touch_velocity_x.set(0.0);
                    self.touch_velocity_y.set(0.0);
                    return;
                }
            }
            UITouchPhase::Moved => {
                if touch_ptr != self.primary_touch_ptr.get() {
                    return;
                }
                // Skip duplicate Moved callbacks: UIKit sometimes delivers the same touch
                // sample twice (e.g. due to UITextInput protocol interaction). Deduplicate
                // by comparing UITouch.timestamp — identical timestamps mean the same sample.
                let ts: f64 = unsafe { msg_send![touch, timestamp] };
                if ts == self.last_move_ts.get() {
                    return;
                }
                self.last_move_ts.set(ts);
                // Track velocity using exponential smoothing: v = 0.7*v_old + 0.3*(delta/dt)
                let now = std::time::Instant::now();
                let dt = self
                    .touch_last_time
                    .borrow()
                    .map(|t| now.duration_since(t).as_secs_f32().max(0.001))
                    .unwrap_or(0.016);
                let instant_vx = f32::from(delta.x) / dt;
                let instant_vy = f32::from(delta.y) / dt;
                self.touch_velocity_x
                    .set(self.touch_velocity_x.get() * 0.7 + instant_vx * 0.3);
                self.touch_velocity_y
                    .set(self.touch_velocity_y.get() * 0.7 + instant_vy * 0.3);
                *self.touch_last_time.borrow_mut() = Some(now);
                let pointer_result =
                    if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                        callback(touch_moved_to_pointer_move(
                            position,
                            touch_ptr as u64,
                            modifiers,
                        ))
                    } else {
                        DispatchEventResult::default()
                    };
                if pointer_result.default_prevented {
                    self.touch_scroll_suppressed.set(true);
                }
                if self.touch_scroll_suppressed.get() {
                    return;
                }
                // Use the DOWN position so DrawerHost's edge-zone check (pos_x < EDGE_ZONE)
                // sees where the gesture started, not where the finger currently is.
                let down_pos = self.touch_down_position.get();
                Some(pan_gesture_to_scroll(
                    down_pos,
                    delta,
                    modifiers,
                    phase.into(),
                ))
            }
            UITouchPhase::Ended | UITouchPhase::Cancelled => {
                if touch_ptr != self.primary_touch_ptr.get() {
                    return;
                }
                self.primary_touch_ptr.set(0);
                self.primary_touch_began_at.borrow_mut().take();
                self.touch_pressed.set(false);
                let pointer_result =
                    if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                        if phase == UITouchPhase::Cancelled {
                            callback(touch_cancelled_to_pointer_cancel(
                                position,
                                touch_ptr as u64,
                                modifiers,
                            ))
                        } else {
                            callback(touch_ended_to_pointer_up(
                                position,
                                touch_ptr as u64,
                                modifiers,
                            ))
                        }
                    } else {
                        DispatchEventResult::default()
                    };
                if pointer_result.default_prevented {
                    self.touch_scroll_suppressed.set(true);
                }
                // Start fling if velocity exceeds threshold
                if phase == UITouchPhase::Ended && !self.touch_scroll_suppressed.get() {
                    let vx = self.touch_velocity_x.get();
                    let vy = self.touch_velocity_y.get();
                    if vx.abs() > FLING_THRESHOLD || vy.abs() > FLING_THRESHOLD {
                        *self.fling.borrow_mut() = Some(TouchFling {
                            velocity_x: vx,
                            velocity_y: vy,
                            last_time: std::time::Instant::now(),
                            position,
                        });
                    }
                }
                self.touch_scroll_suppressed.set(false);
                None
            }
            UITouchPhase::Stationary => unreachable!(),
        };

        if let Some(platform_input) = platform_input
            && let Some(callback) = self.input_callback.borrow_mut().as_mut()
        {
            callback(platform_input);
        }
    }

    /// Advance the fling animation one frame. Called from `gpui_ios_request_frame`
    /// before invoking the render callback so fling scroll events are processed
    /// in the same frame they are generated.
    pub(super) fn process_fling(&self) {
        let fling_data = {
            let fling = self.fling.borrow();
            fling
                .as_ref()
                .map(|f| (f.velocity_x, f.velocity_y, f.last_time, f.position))
        };
        let Some((vx, vy, last_time, position)) = fling_data else {
            return;
        };

        let now = std::time::Instant::now();
        let dt = now.duration_since(last_time).as_secs_f32();
        let friction = 0.95_f32.powf(dt * 60.0);
        let new_vx = vx * friction;
        let new_vy = vy * friction;

        if new_vx.abs() < FLING_THRESHOLD && new_vy.abs() < FLING_THRESHOLD {
            *self.fling.borrow_mut() = None;
            if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                callback(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(Point::new(px(0.0), px(0.0))),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Ended,
                }));
            }
            return;
        }

        {
            let mut fling = self.fling.borrow_mut();
            if let Some(ref mut f) = *fling {
                f.velocity_x = new_vx;
                f.velocity_y = new_vy;
                f.last_time = now;
            }
        }

        if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
            callback(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(Point::new(px(new_vx * dt), px(new_vy * dt))),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
            }));
        }
    }

    /// Drain devtool-queued gesture events and dispatch each through the
    /// same `input_callback` real touches use. Called from
    /// `gpui_ios_request_frame` every tick, mirroring Android's
    /// `AndroidPlatform::process_devtool_gestures`. A gesture (long-press,
    /// swipe) spans multiple calls; each call fires whatever steps are due.
    #[cfg(feature = "devtool")]
    pub(super) fn process_devtool_gestures(&self) {
        let events = gpui_devtool::drain_gesture_events();
        if events.is_empty() {
            return;
        }
        for event in events {
            let (platform_input, x, y) = match event {
                gpui_devtool::GestureEvent::Down(x, y) => (
                    touch_began_to_pointer_down(
                        Point::new(px(x), px(y)),
                        999,
                        Modifiers::default(),
                    ),
                    x,
                    y,
                ),
                gpui_devtool::GestureEvent::Move(x, y) => (
                    touch_moved_to_pointer_move(
                        Point::new(px(x), px(y)),
                        999,
                        Modifiers::default(),
                    ),
                    x,
                    y,
                ),
                gpui_devtool::GestureEvent::Up(x, y) => (
                    touch_ended_to_pointer_up(Point::new(px(x), px(y)), 999, Modifiers::default()),
                    x,
                    y,
                ),
            };
            if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                callback(platform_input);
            }
            log::info!("devtool: synthetic gesture event at ({:.1},{:.1})", x, y);
        }
    }

    pub(super) fn inject_scroll(
        &self,
        position: Point<Pixels>,
        delta: Point<Pixels>,
        velocity: Point<Pixels>,
        touch_phase: TouchPhase,
    ) {
        match touch_phase {
            TouchPhase::Moved => {
                *self.fling.borrow_mut() = None;
            }
            TouchPhase::Ended => {
                let vx = f32::from(velocity.x);
                let vy = f32::from(velocity.y);
                if vx.abs() > FLING_THRESHOLD || vy.abs() > FLING_THRESHOLD {
                    *self.fling.borrow_mut() = Some(TouchFling {
                        velocity_x: vx,
                        velocity_y: vy,
                        last_time: std::time::Instant::now(),
                        position,
                    });
                } else {
                    *self.fling.borrow_mut() = None;
                }
            }
            _ => {}
        }

        if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
            callback(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(delta),
                modifiers: Modifiers::default(),
                touch_phase,
            }));
        }
    }

    /// Get the safe area insets
    pub fn safe_area_insets(&self) -> (f32, f32, f32, f32) {
        unsafe {
            // UIEdgeInsets struct
            #[repr(C)]
            struct UIEdgeInsets {
                top: f64,
                left: f64,
                bottom: f64,
                right: f64,
            }

            let insets: UIEdgeInsets = msg_send![self.view, safeAreaInsets];
            (
                insets.top as f32,
                insets.left as f32,
                insets.bottom as f32,
                insets.right as f32,
            )
        }
    }

    /// Whether the software keyboard is currently visible (view is first responder).
    pub fn is_keyboard_shown(&self) -> bool {
        self.active_first_responder_view().is_some()
    }

    /// Whether UIKit reports the software keyboard itself as visible.
    pub fn is_software_keyboard_visible(&self) -> bool {
        software_keyboard_visible()
    }

    /// Show the software keyboard
    pub fn show_keyboard(&self) {
        let responder = self.input_responder_view();
        let _: BOOL = unsafe { msg_send![responder, becomeFirstResponder] };
        self.sync_text_interaction_for_current_responder_state();
        // Selection-only sessions use an empty inputView. Reload after the
        // responder exists so UIKit asks again with the keyboard request active.
        self.reload_text_input_views_if_first_responder();
    }

    /// Hide the software keyboard
    pub fn hide_keyboard(&self) {
        let responder = self
            .active_first_responder_view()
            .unwrap_or_else(|| self.input_responder_view());
        let _: BOOL = unsafe { msg_send![responder, resignFirstResponder] };
        self.sync_text_interaction_for_current_responder_state();
    }

    /// Handle text input from the software keyboard
    /// Note: This is a fallback path. Primary text input goes through insert_text.
    pub fn handle_text_input(&self, text: *mut Object) {
        if text.is_null() {
            return;
        }

        unsafe {
            let utf8: *const i8 = msg_send![text, UTF8String];
            if utf8.is_null() {
                return;
            }

            let text_str = std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned();

            // Send as key events
            for c in text_str.chars() {
                let keystroke = gpui::Keystroke {
                    modifiers: Modifiers::default(),
                    key: c.to_string(),
                    key_char: Some(c.to_string()),
                };

                let event = PlatformInput::KeyDown(gpui::KeyDownEvent {
                    keystroke,
                    is_held: false,
                    prefer_character_input: true,
                });

                if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                    callback(event);
                }
            }
        }
    }

    /// Handle a key event from an external keyboard
    pub fn handle_key_event(&self, key_code: u32, modifier_flags: u32, is_key_down: bool) {
        use super::text_input::{key_code_to_key_down, key_code_to_key_up};

        let event = if is_key_down {
            key_code_to_key_down(key_code, modifier_flags)
        } else {
            key_code_to_key_up(key_code, modifier_flags)
        };

        if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
            callback(event);
        } else {
            ios_log_cstr(c"GPUI iOS: handle_key_event - NO input_callback set!");
        }
    }

    pub fn handle_keyboard_accessory_action(&self, action: &str) -> bool {
        if !should_use_keyboard_accessory(
            self.input_handler.borrow().is_some(),
            self.callback_input_handler.borrow().is_some(),
            self.input_accepts_text_input.get(),
            self.keyboard_session_requested.get(),
            self.input_keyboard_accessory_enabled.get(),
        ) {
            return false;
        }

        unsafe {
            let view = &*(self.view as *const Object);
            with_input_handler(view, |handler| {
                handler.handle_keyboard_accessory_action(action)
            })
            .unwrap_or(false)
        }
    }

    /// Dispatch an accessory action from a key bar that is not attached to the
    /// software keyboard, so it works with no keyboard session in flight.
    pub fn handle_key_bar_action(&self, action: &str) -> bool {
        if !(self.input_handler.borrow().is_some()
            || self.callback_input_handler.borrow().is_some())
            || !self.input_accepts_text_input.get()
            || !self.input_keyboard_accessory_enabled.get()
        {
            return false;
        }

        unsafe {
            let view = &*(self.view as *const Object);
            with_input_handler(view, |handler| {
                handler.handle_keyboard_accessory_action(action)
            })
            .unwrap_or(false)
        }
    }

    /// Insert composed text straight into the focused input handler.
    ///
    /// `handle_text_input` dispatches per-character key events through GPUI's
    /// keymap, which a key bar hosted outside the keyboard cannot rely on; this
    /// takes the same route as `handle_key_bar_action`.
    pub fn handle_key_bar_text(&self, text: &str) -> bool {
        if !(self.input_handler.borrow().is_some()
            || self.callback_input_handler.borrow().is_some())
            || !self.input_accepts_text_input.get()
        {
            return false;
        }

        unsafe {
            let view = &*(self.view as *const Object);
            with_input_handler(view, |handler| {
                handler.replace_text_in_range(None, text);
            })
            .is_some()
        }
    }

    /// Notify the window of active status changes (foreground/background).
    ///
    /// This is called by the FFI layer when the app transitions between
    /// foreground and background states.
    pub fn notify_active_status_change(&self, is_active: bool) {
        log::info!("GPUI iOS: Window active status changed to: {}", is_active);

        if let Some(callback) = self.active_status_callback.borrow_mut().as_mut() {
            callback(is_active);
        }
    }

    /// Update window bounds and Metal drawable size after a rotation or resize event.
    ///
    /// `width_pts` and `height_pts` are the new dimensions in logical points
    /// (as reported by `[UIScreen mainScreen].bounds`).
    pub fn handle_resize(&self, width_pts: f32, height_pts: f32) {
        let scale = self.scale_factor.get();
        let new_size = size(px(width_pts), px(height_pts));
        let new_bounds = Bounds {
            origin: Point::default(),
            size: new_size,
        };
        self.bounds.set(new_bounds);

        let physical_size = Size {
            width: DevicePixels((width_pts * scale) as i32),
            height: DevicePixels((height_pts * scale) as i32),
        };
        self.renderer
            .borrow_mut()
            .update_drawable_size(physical_size);

        let mut callback = self.resize_callback.borrow_mut().take();
        if let Some(ref mut cb) = callback {
            cb(new_size, scale);
        }
        *self.resize_callback.borrow_mut() = callback;
    }
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        unsafe {
            if !self.editable_text_interaction.is_null() {
                let _: () = msg_send![self.editable_text_interaction, release];
            }
            if !self.noneditable_text_interaction.is_null() {
                let _: () = msg_send![self.noneditable_text_interaction, release];
            }
        }
        super::ffi::unregister_window(self as *const Self);
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let view = NonNull::new(self.view as *mut c_void)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = UiKitWindowHandle::new(view);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let handle = UiKitDisplayHandle::new();
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(handle.into()) })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn is_maximized(&self) -> bool {
        true // iOS windows are always "maximized"
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds.get())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.get().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // iOS windows cannot be resized programmatically
    }

    fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        unsafe {
            let trait_collection: *mut Object = msg_send![self.view, traitCollection];
            let style: i64 = msg_send![trait_collection, userInterfaceStyle];
            match style {
                2 => WindowAppearance::Dark,
                _ => WindowAppearance::Light,
            }
        }
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::main()))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.modifiers.get()
    }

    fn capslock(&self) -> gpui::Capslock {
        // Would need to check UIKeyModifierFlags
        gpui::Capslock { on: false }
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        let had_input_handler = self.callback_input_handler.borrow().is_some();
        let mut input_handler = input_handler;
        let accepts_text_input = input_handler.query_accepts_text_input();
        let uses_manual_focus = input_handler.query_uses_manual_focus();
        let native_selection_enabled = input_handler.query_handles_native_selection();
        let keyboard_accessory_enabled = input_handler.query_keyboard_accessory();
        let text_input_traits = input_handler.query_text_input_traits();
        let should_auto_request_keyboard = should_auto_request_soft_keyboard(
            accepts_text_input,
            uses_manual_focus,
            had_input_handler,
        );

        *self.callback_input_handler.borrow_mut() = Some(input_handler.clone());
        *self.input_handler.borrow_mut() = Some(input_handler);
        self.input_accepts_text_input.set(accepts_text_input);
        self.input_uses_manual_focus.set(uses_manual_focus);
        self.input_native_selection_enabled
            .set(native_selection_enabled);
        let previous_keyboard_accessory_enabled = self
            .input_keyboard_accessory_enabled
            .replace(keyboard_accessory_enabled);
        let previous_text_input_traits = self.input_text_input_traits.replace(text_input_traits);
        if previous_text_input_traits != text_input_traits
            || previous_keyboard_accessory_enabled != keyboard_accessory_enabled
        {
            self.reload_text_input_views_if_first_responder();
        }
        if should_auto_request_keyboard {
            self.keyboard_session_requested.set(true);
        }
        self.refresh_text_input_state();
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn clear_input_handler(&mut self) {
        let had_input_handler = self.input_handler.borrow_mut().take().is_some();
        let had_callback_input_handler = self.callback_input_handler.borrow_mut().take().is_some();
        self.input_accepts_text_input.set(false);
        self.input_uses_manual_focus.set(false);
        self.input_native_selection_enabled.set(false);
        let previous_keyboard_accessory_enabled =
            self.input_keyboard_accessory_enabled.replace(false);
        let previous_text_input_traits = self
            .input_text_input_traits
            .replace(PlatformTextInputTraits::default());
        if previous_text_input_traits != PlatformTextInputTraits::default()
            || previous_keyboard_accessory_enabled
        {
            self.reload_text_input_views_if_first_responder();
        }
        if should_clear_keyboard_request_when_clearing_input_handler(
            had_input_handler,
            had_callback_input_handler,
        ) {
            self.keyboard_session_requested.set(false);
        }
        self.refresh_text_input_state();
    }

    fn set_selection_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.callback_selection_handler.borrow_mut() = Some(input_handler.clone());
        *self.selection_handler.borrow_mut() = Some(input_handler);
        self.refresh_text_input_state();
    }

    fn take_selection_handler(&mut self) -> Option<PlatformInputHandler> {
        self.selection_handler.borrow_mut().take()
    }

    fn clear_selection_handler(&mut self) {
        self.selection_handler.borrow_mut().take();
        self.callback_selection_handler.borrow_mut().take();
        self.selectable_text_hit_regions.borrow_mut().clear();
        self.refresh_text_input_state();
    }

    fn clear_active_selection(&self) {
        self.clear_active_text_selection(false);
    }

    fn set_selectable_text_hit_regions(&self, regions: SmallVec<[SelectableTextHitRegion; 8]>) {
        *self.selectable_text_hit_regions.borrow_mut() = regions;
    }

    fn show_soft_keyboard(&self) {
        self.keyboard_session_requested.set(true);
        self.reload_text_input_views_if_first_responder();
        self.refresh_text_input_state();
    }

    fn hide_soft_keyboard(&self) {
        self.keyboard_session_requested.set(false);
        self.hide_keyboard();
    }

    fn is_soft_keyboard_visible(&self) -> bool {
        self.is_software_keyboard_visible()
    }

    fn has_active_keyboard_accessory(&self) -> bool {
        should_use_keyboard_accessory(
            self.input_handler.borrow().is_some(),
            self.callback_input_handler.borrow().is_some(),
            self.input_accepts_text_input.get(),
            self.keyboard_session_requested.get(),
            self.input_keyboard_accessory_enabled.get(),
        )
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        // Would use UIAlertController
        let (_tx, rx) = futures::channel::oneshot::channel();

        unsafe {
            // Create UIAlertController
            let title = msg;
            let message = detail.unwrap_or("");

            let alert_style: i64 = 1; // UIAlertControllerStyleAlert

            let title_str: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: title.as_ptr()];
            let message_str: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: message.as_ptr()];

            let alert: *mut Object = msg_send![
                class!(UIAlertController),
                alertControllerWithTitle: title_str
                message: message_str
                preferredStyle: alert_style
            ];

            // Add buttons
            for (_index, button) in answers.iter().enumerate() {
                let button_title: *mut Object = msg_send![
                    class!(NSString),
                    stringWithUTF8String: button.label().as_str().as_ptr()
                ];

                let action_style: i64 = if button.is_cancel() { 1 } else { 0 }; // UIAlertActionStyleCancel or Default

                // Note: In production, this would need a block that calls tx.send(index)
                let action: *mut Object = msg_send![
                    class!(UIAlertAction),
                    actionWithTitle: button_title
                    style: action_style
                    handler: ptr::null::<Object>()
                ];

                let _: () = msg_send![alert, addAction: action];
            }

            // Present the alert
            if !self.view_controller.is_null() {
                let _: () = msg_send![
                    self.view_controller,
                    presentViewController: alert
                    animated: YES
                    completion: ptr::null::<Object>()
                ];
            }
        }

        Some(rx)
    }

    fn activate(&self) {
        unsafe {
            if !self.window.is_null() {
                let _: () = msg_send![self.window, makeKeyAndVisible];
            }
        }
    }

    fn is_active(&self) -> bool {
        if self.window.is_null() {
            return true;
        }
        unsafe {
            let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
            let key_window: *mut Object = msg_send![app, keyWindow];
            self.window == key_window
        }
    }

    fn is_hovered(&self) -> bool {
        // Hover isn't really applicable on iOS
        false
    }

    fn set_title(&mut self, _title: &str) {
        // iOS apps don't have window titles
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {
        // Could adjust view background color
    }

    fn minimize(&self) {
        // iOS apps cannot be minimized
    }

    fn zoom(&self) {
        // iOS apps cannot be zoomed
    }

    fn toggle_fullscreen(&self) {
        // iOS apps are always fullscreen
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.request_frame_callback.borrow_mut() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.input_callback.borrow_mut() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.active_status_callback.borrow_mut() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.hover_status_callback.borrow_mut() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.resize_callback.borrow_mut() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.moved_callback.borrow_mut() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.should_close_callback.borrow_mut() = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        *self.hit_test_callback.borrow_mut() = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.close_callback.borrow_mut() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.appearance_changed_callback.borrow_mut() = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.renderer.borrow_mut().draw(scene);
    }

    fn set_render_effect(&self, effect: Option<Box<dyn std::any::Any>>) {
        let effect = match effect {
            Some(effect) => match effect.downcast::<crate::render_effect::IosRenderEffect>() {
                Ok(effect) => Some(effect.0),
                Err(_) => {
                    log::error!("set_render_effect: expected IosRenderEffect, dropping effect");
                    None
                }
            },
            None => None,
        };
        self.renderer.borrow_mut().set_render_effect(effect);
    }

    fn completed_frame(&self) {
        self.refresh_selection_geometry();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.borrow().sprite_atlas().clone()
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        true
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        // Would query Metal device capabilities
        None
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // iOS handles IME positioning automatically
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_context_rewrite_reports_text_input_range_geometry() {
        assert!(should_report_text_input_range_geometry(
            TEXT_INTERACTION_EDITABLE,
            false
        ));
    }

    #[test]
    fn editable_edit_menu_actions_are_disabled_for_current_inputs() {
        assert_eq!(
            edit_menu_action_policy(TEXT_INTERACTION_EDITABLE, false, true, Default::default()),
            EditMenuActionPolicy::DisableNativeMenu
        );
        assert_eq!(
            edit_menu_action_policy(TEXT_INTERACTION_EDITABLE, false, false, Default::default()),
            EditMenuActionPolicy::DisableNativeMenu
        );
    }

    #[test]
    fn editable_native_selection_copy_uses_selection_policy() {
        assert_eq!(
            edit_menu_action_policy(TEXT_INTERACTION_EDITABLE, true, true, Default::default()),
            EditMenuActionPolicy::CopySelection
        );
        assert_eq!(
            edit_menu_action_policy(TEXT_INTERACTION_EDITABLE, true, false, Default::default()),
            EditMenuActionPolicy::DelegateToSystem
        );
    }

    #[test]
    fn noneditable_copy_uses_selection_policy_but_other_actions_delegate() {
        assert_eq!(
            edit_menu_action_policy(
                TEXT_INTERACTION_NONEDITABLE,
                false,
                true,
                Default::default(),
            ),
            EditMenuActionPolicy::CopySelection
        );
        assert_eq!(
            edit_menu_action_policy(
                TEXT_INTERACTION_NONEDITABLE,
                false,
                false,
                Default::default(),
            ),
            EditMenuActionPolicy::DelegateToSystem
        );
    }

    #[test]
    fn custom_only_selection_disables_responder_copy() {
        assert_eq!(
            edit_menu_action_policy(
                TEXT_INTERACTION_NONEDITABLE,
                false,
                true,
                SelectionMenuPresentation::CustomActionsOnly,
            ),
            EditMenuActionPolicy::DisableNativeMenu
        );
    }

    #[test]
    fn touch_outside_the_selection_dismisses_it() {
        assert!(should_dismiss_selection_for_touch(true, false, false));
        assert!(!should_dismiss_selection_for_touch(true, true, false));
        assert!(!should_dismiss_selection_for_touch(true, false, true));
        assert!(!should_dismiss_selection_for_touch(false, false, false));
    }

    #[test]
    fn native_touch_selection_is_disabled_without_text_interaction() {
        assert!(!handles_native_touch_selection(
            TEXT_INTERACTION_NONE,
            false
        ));
    }

    #[test]
    fn native_touch_selection_is_enabled_for_read_only_selection() {
        assert!(handles_native_touch_selection(
            TEXT_INTERACTION_NONEDITABLE,
            false
        ));
    }

    #[test]
    fn native_touch_selection_is_disabled_for_current_input_handlers() {
        assert!(!handles_native_touch_selection(
            TEXT_INTERACTION_EDITABLE,
            false
        ));
    }

    #[test]
    fn native_touch_selection_can_be_enabled_for_editable_handlers() {
        assert!(handles_native_touch_selection(
            TEXT_INTERACTION_EDITABLE,
            true
        ));
    }

    #[test]
    fn active_selection_geometry_hit_test_includes_handle_slop() {
        let rect = Bounds::new(Point::new(px(218.1), px(363.7)), size(px(57.6), px(16.0)));
        let geometry = SelectionGeometry {
            range: 471..479,
            bounds: Some(rect),
            rects: vec![rect],
        };

        assert!(selection_geometry_contains_interaction_point(
            &geometry,
            Point::new(px(224.7), px(361.0))
        ));
        assert!(!selection_geometry_contains_interaction_point(
            &geometry,
            Point::new(px(24.0), px(361.0))
        ));
    }

    #[test]
    fn editable_input_still_reports_ime_range_geometry() {
        assert!(should_report_text_input_range_geometry(
            TEXT_INTERACTION_EDITABLE,
            false
        ));
        assert!(should_report_text_input_range_geometry(
            TEXT_INTERACTION_NONEDITABLE,
            false
        ));
        assert!(!should_report_text_input_range_geometry(
            TEXT_INTERACTION_NONE,
            false
        ));
    }

    #[test]
    fn text_input_system_mutations_do_not_notify_the_input_delegate() {
        assert!(!should_notify_text_input_delegate(
            TextInputMutationSource::TextInputSystem
        ));
        assert!(should_notify_text_input_delegate(
            TextInputMutationSource::External
        ));
    }

    #[test]
    fn has_text_follows_the_current_text_input_document() {
        assert!(!text_input_has_text(None));
        assert!(!text_input_has_text(Some(0)));
        assert!(text_input_has_text(Some(1)));
    }

    #[test]
    fn text_position_offsets_stay_inside_document() {
        assert_eq!(offset_text_position_index(3, -3, 3), Some(0));
        assert_eq!(offset_text_position_index(3, 0, 3), Some(3));
        assert_eq!(offset_text_position_index(0, 3, 3), Some(3));

        // Critical: UITextInputStringTokenizer probes large offsets while
        // computing language context. Returning impossible positions causes
        // native IMEs to make decisions against invalid text ranges.
        assert_eq!(offset_text_position_index(3, 300, 3), None);
        assert_eq!(offset_text_position_index(0, -1, 3), None);
    }

    #[test]
    fn tokenizer_offsets_stay_inside_requested_range() {
        assert_eq!(text_position_index_at_range_offset(2..5, 0), Some(2));
        assert_eq!(text_position_index_at_range_offset(2..5, 3), Some(5));
        assert_eq!(text_position_index_at_range_offset(2..5, 4), None);
        assert_eq!(text_position_index_at_range_offset(2..5, -1), None);

        assert_eq!(text_position_offset_in_range(2, 2..5), Some(0));
        assert_eq!(text_position_offset_in_range(5, 2..5), Some(3));
        assert_eq!(text_position_offset_in_range(6, 2..5), None);
    }

    #[test]
    fn character_range_by_extending_returns_adjacent_character() {
        assert_eq!(
            text_character_range_by_extending_position(2, 0, 4),
            Some(2..3)
        );
        assert_eq!(
            text_character_range_by_extending_position(2, 1, 4),
            Some(1..2)
        );
        assert_eq!(
            text_character_range_by_extending_position(0, 1, 4),
            Some(0..0)
        );
        assert_eq!(
            text_character_range_by_extending_position(4, 0, 4),
            Some(4..4)
        );
        assert_eq!(text_character_range_by_extending_position(5, 0, 4), None);
    }

    #[test]
    fn text_interaction_begin_requires_native_selection_hit() {
        assert!(should_begin_text_interaction(
            TEXT_INTERACTION_NONEDITABLE,
            false,
            true
        ));
        assert!(!should_begin_text_interaction(
            TEXT_INTERACTION_NONEDITABLE,
            false,
            false
        ));
        assert!(!should_begin_text_interaction(
            TEXT_INTERACTION_EDITABLE,
            false,
            true
        ));
        assert!(should_begin_text_interaction(
            TEXT_INTERACTION_EDITABLE,
            true,
            true
        ));
        assert!(!should_begin_text_interaction(
            TEXT_INTERACTION_NONE,
            true,
            true
        ));
    }

    #[test]
    fn explicit_keyboard_request_survives_until_input_handler_arrives() {
        let plan = text_input_refresh_plan(false, false, false, false, false, true, false);

        assert!(plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_NONE);
        assert_eq!(plan.responder_action, TextInputResponderAction::None);
    }

    #[test]
    fn stale_keyboard_request_resigns_when_handler_is_cleared() {
        let plan = text_input_refresh_plan(false, false, false, false, false, false, true);

        assert!(!plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_NONE);
        assert_eq!(
            plan.responder_action,
            TextInputResponderAction::ResignActiveResponder
        );
    }

    #[test]
    fn clearing_existing_input_handler_clears_keyboard_request() {
        assert!(should_clear_keyboard_request_when_clearing_input_handler(
            true, false
        ));
        assert!(should_clear_keyboard_request_when_clearing_input_handler(
            false, true
        ));
    }

    #[test]
    fn clearing_without_existing_input_handler_preserves_pre_handler_request() {
        assert!(!should_clear_keyboard_request_when_clearing_input_handler(
            false, false
        ));
    }

    #[test]
    fn system_keyboard_requires_text_input_and_explicit_request() {
        assert!(should_use_system_keyboard(true, false, true, true));
        assert!(should_use_system_keyboard(false, true, true, true));
        assert!(!should_use_system_keyboard(false, false, false, false));
        assert!(!should_use_system_keyboard(false, false, false, true));
        assert!(!should_use_system_keyboard(false, false, true, true));
        assert!(!should_use_system_keyboard(true, false, false, true));
        assert!(!should_use_system_keyboard(true, false, true, false));
    }

    #[test]
    fn keyboard_accessory_requires_active_text_input_and_opt_in() {
        assert!(should_use_keyboard_accessory(true, false, true, true, true));
        assert!(should_use_keyboard_accessory(false, true, true, true, true));
        assert!(!should_use_keyboard_accessory(
            false, false, true, true, true
        ));
        assert!(!should_use_keyboard_accessory(
            true, false, false, true, true
        ));
        assert!(!should_use_keyboard_accessory(
            true, false, true, false, true
        ));
        assert!(!should_use_keyboard_accessory(
            true, false, true, true, false
        ));
    }

    #[test]
    fn input_owned_native_selection_stays_editable_without_keyboard_request() {
        let plan = text_input_refresh_plan(true, false, true, true, true, false, false);

        assert!(!plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_EDITABLE);
        assert_eq!(plan.responder_action, TextInputResponderAction::None);
    }

    #[test]
    fn keyboard_request_shows_for_input_owned_native_selection() {
        let plan = text_input_refresh_plan(true, false, true, true, true, true, false);

        assert!(plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_EDITABLE);
        assert_eq!(
            plan.responder_action,
            TextInputResponderAction::ShowKeyboard
        );
    }

    #[test]
    fn keyboard_request_does_not_reopen_for_visible_input_owned_native_selection() {
        let plan = text_input_refresh_plan(true, false, true, true, true, true, true);

        assert!(plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_EDITABLE);
        assert_eq!(plan.responder_action, TextInputResponderAction::None);
    }

    #[test]
    fn explicit_keyboard_request_shows_when_manual_focus_handler_arrives() {
        let plan = text_input_refresh_plan(true, false, true, false, true, true, false);

        assert!(plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_EDITABLE);
        assert_eq!(
            plan.responder_action,
            TextInputResponderAction::ShowKeyboard
        );
    }

    #[test]
    fn explicit_keyboard_request_does_not_reopen_when_keyboard_is_visible() {
        let plan = text_input_refresh_plan(true, false, true, false, true, true, true);

        assert!(plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_EDITABLE);
        assert_eq!(plan.responder_action, TextInputResponderAction::None);
    }

    #[test]
    fn keyboard_request_clears_when_non_text_handler_arrives() {
        let plan = text_input_refresh_plan(true, false, false, false, false, true, false);

        assert!(!plan.keyboard_session_requested);
        assert_eq!(plan.target_interaction_mode, TEXT_INTERACTION_NONE);
        assert_eq!(
            plan.responder_action,
            TextInputResponderAction::ResignActiveResponder
        );
    }

    #[test]
    fn editable_mode_routes_input_only_while_first_responder() {
        assert_eq!(
            active_text_interaction_mode_for_state(TEXT_INTERACTION_EDITABLE, true, false, true),
            TEXT_INTERACTION_EDITABLE
        );
        assert_eq!(
            active_text_interaction_mode_for_state(TEXT_INTERACTION_EDITABLE, true, false, false),
            TEXT_INTERACTION_NONEDITABLE
        );
        assert_eq!(
            active_text_interaction_mode_for_state(TEXT_INTERACTION_EDITABLE, false, false, false),
            TEXT_INTERACTION_NONE
        );
    }

    #[test]
    fn editable_native_selection_remains_installed_outside_first_responder() {
        assert_eq!(
            active_text_interaction_mode_for_state(TEXT_INTERACTION_EDITABLE, false, true, false),
            TEXT_INTERACTION_EDITABLE
        );
    }

    #[test]
    fn noneditable_interaction_remains_installed_outside_first_responder() {
        assert_eq!(
            active_text_interaction_mode_for_state(
                TEXT_INTERACTION_NONEDITABLE,
                true,
                false,
                false
            ),
            TEXT_INTERACTION_NONEDITABLE
        );
        assert_eq!(
            active_text_interaction_mode_for_state(TEXT_INTERACTION_NONEDITABLE, true, false, true),
            TEXT_INTERACTION_NONEDITABLE
        );
    }

    #[test]
    fn text_input_trait_values_match_uikit_enums() {
        assert_eq!(
            text_input_trait_value(PlatformTextInputTrait::SystemDefault),
            0
        );
        assert_eq!(text_input_trait_value(PlatformTextInputTrait::Disabled), 1);
        assert_eq!(text_input_trait_value(PlatformTextInputTrait::Enabled), 2);
    }

    #[test]
    fn keyboard_suggestion_traits_map_to_uikit_values_without_smart_mutation() {
        let traits = PlatformTextInputTraits::keyboard_suggestions();

        assert_eq!(autocapitalization_value(traits.autocapitalization), 0);
        assert_eq!(text_input_trait_value(traits.autocorrection), 2);
        assert_eq!(text_input_trait_value(traits.inline_prediction), 2);
        assert_eq!(text_input_trait_value(traits.spell_checking), 1);
        assert_eq!(text_input_trait_value(traits.smart_quotes), 1);
        assert_eq!(text_input_trait_value(traits.smart_dashes), 1);
        assert_eq!(text_input_trait_value(traits.smart_insert_delete), 1);
    }

    #[test]
    fn autocapitalization_values_match_uikit_enums() {
        assert_eq!(
            autocapitalization_value(PlatformTextAutocapitalization::None),
            0
        );
        assert_eq!(
            autocapitalization_value(PlatformTextAutocapitalization::Words),
            1
        );
        assert_eq!(
            autocapitalization_value(PlatformTextAutocapitalization::Sentences),
            2
        );
        assert_eq!(
            autocapitalization_value(PlatformTextAutocapitalization::AllCharacters),
            3
        );
    }
}
