use std::{ffi::c_void, ptr};

use anyhow::{bail, Context, Result};
use core_foundation::{
    array::CFArray,
    base::{CFGetTypeID, CFRelease, CFRetain, CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    string::CFString,
};

use super::model::{WindowFrame, WindowSnapshot};

type AxElementRef = *const c_void;
type AxValueRef = *const c_void;
type AxError = i32;

const AX_SUCCESS: AxError = 0;
const AX_VALUE_CGPOINT: u32 = 1;
const AX_VALUE_CGSIZE: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CgSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CgRect {
    origin: CgPoint,
    size: CgSize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> AxElementRef;
    fn AXUIElementCreateSystemWide() -> AxElementRef;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementCopyAttributeValue(
        element: AxElementRef,
        attribute: *const c_void,
        value: *mut CFTypeRef,
    ) -> AxError;
    fn AXUIElementGetPid(element: AxElementRef, pid: *mut i32) -> AxError;
    fn AXUIElementSetAttributeValue(
        element: AxElementRef,
        attribute: *const c_void,
        value: CFTypeRef,
    ) -> AxError;
    fn AXValueCreate(value_type: u32, value: *const c_void) -> AxValueRef;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: AxValueRef, value_type: u32, output: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayBounds(display: u32) -> CgRect;
    fn CGGetOnlineDisplayList(max_displays: u32, displays: *mut u32, count: *mut u32) -> i32;
}

struct OwnedAx(AxElementRef);

impl OwnedAx {
    fn from_create(reference: AxElementRef) -> Result<Self> {
        if reference.is_null() {
            bail!("macOS returned an empty Accessibility element");
        }
        if unsafe { CFGetTypeID(reference.cast()) } != unsafe { AXUIElementGetTypeID() } {
            unsafe {
                CFRelease(reference.cast());
            }
            bail!("macOS returned a non-Accessibility element");
        }
        Ok(Self(reference))
    }

    fn retain(reference: AxElementRef) -> Result<Self> {
        if reference.is_null() {
            bail!("macOS returned an empty Accessibility window");
        }
        if unsafe { CFGetTypeID(reference.cast()) } != unsafe { AXUIElementGetTypeID() } {
            bail!("macOS returned a non-Accessibility window");
        }
        unsafe {
            CFRetain(reference.cast());
        }
        Ok(Self(reference))
    }
}

impl Drop for OwnedAx {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0.cast());
        }
    }
}

pub(super) struct FocusedWindow {
    snapshot: WindowSnapshot,
}

impl FocusedWindow {
    pub(super) fn capture() -> Result<Self> {
        require_accessibility()?;
        let system = OwnedAx::from_create(unsafe { AXUIElementCreateSystemWide() })?;
        let application = copy_ax(&system, "AXFocusedApplication")
            .context("read the focused macOS application")?;
        let element =
            copy_ax(&application, "AXFocusedWindow").context("read the focused macOS window")?;
        let app_name = string_attribute(&application, "AXTitle")?
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Application".to_owned());
        let snapshot = snapshot(&element, app_name)?;
        if bool_attribute(&element, "AXFullScreen")?.unwrap_or(false) {
            bail!("exit native full screen before sending this window to Stream");
        }
        if snapshot.identifier.is_none() && snapshot.title.is_empty() {
            bail!("the focused {} window has no stable identifier or title", snapshot.app_name);
        }
        Ok(Self { snapshot })
    }

    pub(super) fn snapshot(&self) -> WindowSnapshot {
        self.snapshot.clone()
    }

    pub(super) fn identifies(&self, snapshot: &WindowSnapshot) -> bool {
        snapshot.identifier.is_some() == self.snapshot.identifier.is_some()
            && (snapshot.identifier.is_some() || !snapshot.title.is_empty())
            && snapshot.identifies(&self.snapshot)
    }
}

pub(super) fn restore(snapshot: &WindowSnapshot) -> Result<()> {
    let window = find_window(snapshot)?;
    set_frame(&window, snapshot.original_frame)
        .with_context(|| format!("restore {} window {:?}", snapshot.app_name, snapshot.title))
}

pub(super) fn move_snapshot(snapshot: &WindowSnapshot, target: WindowFrame) -> Result<WindowFrame> {
    let window = find_window(snapshot)?;
    set_frame(&window, target)?;
    frame(&window)
}

pub(super) fn online_display_bounds(display_id: u32) -> Result<Option<WindowFrame>> {
    let mut displays = [0_u32; 32];
    let mut count = 0_u32;
    let status =
        unsafe { CGGetOnlineDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count) };
    if status != 0 {
        bail!("list online macOS displays failed with CoreGraphics error {status}");
    }
    let count = usize::try_from(count).context("convert online macOS display count")?;
    if count > displays.len() {
        bail!("macOS returned an invalid online display count {count}");
    }
    if !displays[..count].contains(&display_id) {
        return Ok(None);
    }
    let bounds = unsafe { CGDisplayBounds(display_id) };
    let frame = WindowFrame {
        x: bounds.origin.x,
        y: bounds.origin.y,
        width: bounds.size.width,
        height: bounds.size.height,
    };
    validate_frame(frame).context("validate online macOS display bounds")?;
    Ok(Some(frame))
}

pub(super) fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

pub(super) fn request_accessibility() -> bool {
    if accessibility_granted() {
        return true;
    }
    let prompt_key = CFString::new("AXTrustedCheckOptionPrompt");
    let prompt = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(prompt_key, prompt)]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef().cast()) != 0 }
}

fn require_accessibility() -> Result<()> {
    if request_accessibility() {
        Ok(())
    } else {
        bail!(
            "Kit needs Accessibility access to move windows; enable Kit in System Settings > Privacy & Security > Accessibility, then press the shortcut again"
        )
    }
}

fn snapshot(element: &OwnedAx, app_name: String) -> Result<WindowSnapshot> {
    let mut pid = 0_i32;
    ax_result(unsafe { AXUIElementGetPid(element.0, &mut pid) }, "read window process ID")?;
    let title = string_attribute(element, "AXTitle")?.unwrap_or_default();
    let identifier = string_attribute(element, "AXIdentifier")?.filter(|value| !value.is_empty());
    let original_frame = frame(element)?;
    Ok(WindowSnapshot {
        pid,
        app_name,
        title,
        identifier,
        original_frame,
        streamed_frame: original_frame,
    })
}

fn find_window(snapshot: &WindowSnapshot) -> Result<OwnedAx> {
    let application = OwnedAx::from_create(unsafe { AXUIElementCreateApplication(snapshot.pid) })
        .with_context(|| {
        format!("find running {} process {}", snapshot.app_name, snapshot.pid)
    })?;
    let actual_app_name = string_attribute(&application, "AXTitle")?.unwrap_or_default();
    if snapshot.app_name != "Application" && actual_app_name != snapshot.app_name {
        bail!(
            "process {} is now {:?}, not the original application {:?}",
            snapshot.pid,
            actual_app_name,
            snapshot.app_name
        );
    }
    if snapshot.identifier.is_none() && snapshot.title.is_empty() {
        bail!("the original {} window has no stable identifier or title", snapshot.app_name);
    }
    let values = copy_type(&application, "AXWindows")?;
    let array = values
        .downcast_into::<CFArray<*const c_void>>()
        .context("macOS returned a non-array AXWindows value")?;
    let mut best: Option<(f64, OwnedAx)> = None;
    for raw in array.iter() {
        let Ok(window) = OwnedAx::retain(*raw as AxElementRef) else {
            continue;
        };
        let identifier = string_attribute(&window, "AXIdentifier").ok().flatten();
        let title = string_attribute(&window, "AXTitle").ok().flatten().unwrap_or_default();
        if !window_identity_matches(snapshot, identifier.as_deref(), &title) {
            continue;
        }
        let current_frame = match frame(&window) {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        let score = current_frame.distance_from(snapshot.streamed_frame);
        if best.as_ref().is_none_or(|(best_score, _)| score < *best_score) {
            best = Some((score, window));
        }
    }
    let (_, window) = best.with_context(|| {
        format!("find the streamed {} window {:?}", snapshot.app_name, snapshot.title)
    })?;
    Ok(window)
}

fn window_identity_matches(
    snapshot: &WindowSnapshot,
    identifier: Option<&str>,
    title: &str,
) -> bool {
    match (&snapshot.identifier, identifier) {
        (Some(expected), Some(actual)) => expected == actual,
        (None, None) => !snapshot.title.is_empty() && snapshot.title == title,
        _ => false,
    }
}

fn frame(element: &OwnedAx) -> Result<WindowFrame> {
    let position: CgPoint = ax_attribute(element, "AXPosition", AX_VALUE_CGPOINT)?;
    let size: CgSize = ax_attribute(element, "AXSize", AX_VALUE_CGSIZE)?;
    let frame =
        WindowFrame { x: position.x, y: position.y, width: size.width, height: size.height };
    validate_frame(frame).context("validate macOS window frame")?;
    Ok(frame)
}

fn set_frame(element: &OwnedAx, frame: WindowFrame) -> Result<()> {
    validate_frame(frame).context("validate requested macOS window frame")?;
    let position = CgPoint { x: frame.x, y: frame.y };
    let size = CgSize { width: frame.width, height: frame.height };
    set_ax_value(element, "AXPosition", AX_VALUE_CGPOINT, &position)?;
    set_ax_value(element, "AXSize", AX_VALUE_CGSIZE, &size)?;
    set_ax_value(element, "AXPosition", AX_VALUE_CGPOINT, &position)?;
    Ok(())
}

fn ax_attribute<T: Default>(element: &OwnedAx, name: &str, value_type: u32) -> Result<T> {
    let value = copy_type(element, name)?;
    if unsafe { CFGetTypeID(value.as_CFTypeRef()) } != unsafe { AXValueGetTypeID() } {
        bail!("macOS returned an invalid {name} value");
    }
    let mut output = T::default();
    let copied = unsafe {
        AXValueGetValue(value.as_CFTypeRef().cast(), value_type, (&mut output as *mut T).cast())
    };
    if copied == 0 {
        bail!("decode macOS {name} value");
    }
    Ok(output)
}

fn set_ax_value<T>(element: &OwnedAx, name: &str, value_type: u32, value: &T) -> Result<()> {
    let wrapped = unsafe { AXValueCreate(value_type, (value as *const T).cast()) };
    if wrapped.is_null() {
        bail!("encode macOS {name} value");
    }
    let wrapped = unsafe { CFType::wrap_under_create_rule(wrapped.cast()) };
    let result = unsafe {
        AXUIElementSetAttributeValue(
            element.0,
            CFString::new(name).as_concrete_TypeRef().cast(),
            wrapped.as_CFTypeRef(),
        )
    };
    ax_result(result, &format!("set window {name}"))
}

fn validate_frame(frame: WindowFrame) -> Result<()> {
    if ![frame.x, frame.y, frame.width, frame.height].into_iter().all(f64::is_finite) {
        bail!("macOS returned a non-finite window frame");
    }
    if frame.width <= 0.0 || frame.height <= 0.0 {
        bail!("macOS returned a non-positive window frame");
    }
    Ok(())
}

fn string_attribute(element: &OwnedAx, name: &str) -> Result<Option<String>> {
    let Some(value) = copy_optional_type(element, name)? else {
        return Ok(None);
    };
    let value = value
        .downcast::<CFString>()
        .with_context(|| format!("macOS returned a non-string {name} value"))?;
    Ok(Some(value.to_string()))
}

fn bool_attribute(element: &OwnedAx, name: &str) -> Result<Option<bool>> {
    let Some(value) = copy_optional_type(element, name)? else {
        return Ok(None);
    };
    let value = value
        .downcast::<CFBoolean>()
        .with_context(|| format!("macOS returned a non-boolean {name} value"))?;
    Ok(Some(bool::from(value)))
}

fn copy_ax(element: &OwnedAx, name: &str) -> Result<OwnedAx> {
    let value = copy_raw(element, name)?;
    OwnedAx::from_create(value.cast())
}

fn copy_type(element: &OwnedAx, name: &str) -> Result<CFType> {
    let value = copy_raw(element, name)?;
    Ok(unsafe { CFType::wrap_under_create_rule(value) })
}

fn copy_optional_type(element: &OwnedAx, name: &str) -> Result<Option<CFType>> {
    let attribute = CFString::new(name);
    let mut value: CFTypeRef = ptr::null();
    let result = unsafe {
        AXUIElementCopyAttributeValue(element.0, attribute.as_concrete_TypeRef().cast(), &mut value)
    };
    if result == -25212 || result == -25205 {
        return Ok(None);
    }
    ax_result(result, &format!("read window {name}"))?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(unsafe { CFType::wrap_under_create_rule(value) }))
}

fn copy_raw(element: &OwnedAx, name: &str) -> Result<CFTypeRef> {
    let attribute = CFString::new(name);
    let mut value: CFTypeRef = ptr::null();
    let result = unsafe {
        AXUIElementCopyAttributeValue(element.0, attribute.as_concrete_TypeRef().cast(), &mut value)
    };
    ax_result(result, &format!("read window {name}"))?;
    if value.is_null() {
        bail!("macOS returned an empty {name} value");
    }
    Ok(value)
}

fn ax_result(result: AxError, operation: &str) -> Result<()> {
    if result == AX_SUCCESS {
        Ok(())
    } else {
        bail!("{operation} failed with Accessibility error {result}")
    }
}
