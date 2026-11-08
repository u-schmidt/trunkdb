//! Handles: a box with a tag in front, its address handed out as a
//! pointer to an opaque type (SPEC §177.2, §179).

use crate::error::Fail;
use std::ptr;

/// A database handle's tag, "trdb".
pub(crate) const DATABASE: u32 = 0x7472_6462;
/// A snapshot handle's tag, "trsn".
pub(crate) const SNAPSHOT: u32 = 0x7472_736e;
/// A cursor handle's tag, "trcu".
pub(crate) const CURSOR: u32 = 0x7472_6375;
/// What a handle's tag is overwritten with when it is closed.
const CLOSED: u32 = 0;

/// What a handle points to: the tag first, at the same place in every
/// kind, so it can be read before the kind is known.
#[repr(C)]
struct Handle<T> {
    tag: u32,
    value: T,
}

/// `value` boxed behind `tag`, as a pointer to the opaque type `O`.
pub(crate) fn hand_out<T, O>(tag: u32, value: T) -> *mut O {
    Box::into_raw(Box::new(Handle { tag, value })).cast()
}

fn kind(tag: u32) -> &'static str {
    match tag {
        DATABASE => "database",
        SNAPSHOT => "snapshot",
        _ => "cursor",
    }
}

/// Whether `pointer` is a live handle of `tag`'s kind: not null, and its
/// tag that kind's. The tag is read as a `u32` before anything else, so
/// a handle of another kind is never read as this one's type.
///
/// # Safety
/// `pointer` is null or points to at least four readable bytes: a handle
/// this library handed out, of any kind, open or recently closed.
unsafe fn checked<T, O>(pointer: *const O, tag: u32, what: &str) -> Result<*mut Handle<T>, Fail> {
    if pointer.is_null() {
        return Err(Fail::argument(format!("{what} is null")));
    }
    // SAFETY: by the caller, four bytes to read; `Handle` is `repr(C)`
    // with the tag first.
    let found = unsafe { ptr::read(pointer.cast::<u32>()) };
    if found != tag {
        return Err(Fail::argument(format!(
            "{what} is not an open {} handle",
            kind(tag)
        )));
    }
    Ok(pointer.cast_mut().cast())
}

/// The value behind a live handle of `tag`'s kind.
///
/// # Safety
/// As for `checked`; and a handle with `tag` holds a `T`, which holds as
/// long as each kind has its own tag and type.
pub(crate) unsafe fn value<'a, T, O>(
    pointer: *const O,
    tag: u32,
    what: &str,
) -> Result<&'a T, Fail> {
    // SAFETY: the tag says it is a `Handle<T>`, not freed.
    unsafe { checked::<T, O>(pointer, tag, what).map(|handle| &(*handle).value) }
}

/// A handle of `tag`'s kind closed: its tag overwritten, then freed.
/// Null is nothing to close.
///
/// # Safety
/// As for `value`, and nothing else uses the handle from now on.
pub(crate) unsafe fn close<T, O>(pointer: *mut O, tag: u32, what: &str) -> Result<(), Fail> {
    if pointer.is_null() {
        return Ok(());
    }
    // SAFETY: as for `value`; the box was made by `hand_out`.
    unsafe {
        let handle = checked::<T, O>(pointer, tag, what)?;
        // Volatile, or the store could be dropped as one to memory freed
        // right after.
        ptr::write_volatile(&raw mut (*handle).tag, CLOSED);
        drop(Box::from_raw(handle));
    }
    Ok(())
}
