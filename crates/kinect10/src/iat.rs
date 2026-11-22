//! Import Address Table patching of a loaded module.

use pelite::pe::imports::Import;
use pelite::pe::{Pe, PeView};
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};

/// Point the IAT slot of `dll!func` imported by the module at `base` to `new`.
/// Returns the previous target, or `None` if the module does not import it.
///
/// # Safety
/// `base` must be the base address of a loaded PE image.
pub unsafe fn patch(base: *const u8, dll: &str, func: &str, new: usize) -> Option<usize> {
    let view = unsafe { PeView::module(base) };
    let imports = view.imports().ok()?;
    let desc =
        imports.iter().find(|d| d.dll_name().is_ok_and(|n| n.to_str().is_ok_and(|n| n.eq_ignore_ascii_case(dll))))?;
    let index = desc.int().ok()?.position(|i| matches!(i, Ok(Import::ByName { name, .. }) if name == func))?;
    let slot = desc.iat().ok()?.as_slice()[index..].as_ptr().cast::<usize>().cast_mut();
    let size = size_of::<usize>();
    let mut prot = 0;
    unsafe {
        if VirtualProtect(slot.cast(), size, PAGE_READWRITE, &raw mut prot) == 0 {
            return None;
        }
        let old = slot.replace(new);
        VirtualProtect(slot.cast(), size, prot, &raw mut prot);
        Some(old)
    }
}
