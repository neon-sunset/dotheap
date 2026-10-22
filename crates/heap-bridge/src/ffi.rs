use std::{ffi::c_void, panic::AssertUnwindSafe, sync::Arc};

#[repr(C)]
pub struct Slice {
    pub data: *const c_void,
    pub len: usize,
    pub stride: usize,
}

impl Slice {
    pub fn new<T>(slice: &[T]) -> Self {
        Self {
            data: slice.as_ptr().cast(),
            len: slice.len(),
            stride: size_of::<T>(),
        }
    }
}

#[repr(C)]
pub struct Owned {
    pub data: *const c_void,
    pub destroy: Option<unsafe extern "C" fn(*const c_void)>,
}

impl Owned {
    pub const EMPTY: Self = Self {
        data: std::ptr::null(),
        destroy: None,
    };

    pub fn new<T: Send + Sync + 'static>(value: T) -> Self {
        unsafe extern "C" fn destroy<T>(data: *const c_void) {
            unsafe { drop(Box::from_raw(data.cast_mut().cast::<T>())) };
        }
        Self {
            data: Box::into_raw(Box::new(value)).cast(),
            destroy: Some(destroy::<T>),
        }
    }

    pub fn arc<T: Send + Sync + 'static>(value: Arc<T>) -> Self {
        unsafe extern "C" fn destroy<T>(data: *const c_void) {
            unsafe { drop(Arc::from_raw(data.cast::<T>())) };
        }
        Self {
            data: Arc::into_raw(value).cast(),
            destroy: Some(destroy::<T>),
        }
    }
}

#[repr(C)]
pub struct Outcome {
    pub value: Owned,
    pub error: Owned,
}

pub fn call(f: impl FnOnce() -> crate::snapshot::Result<Owned>) -> Outcome {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => Outcome {
            value,
            error: Owned::EMPTY,
        },
        Ok(Err(error)) => Outcome {
            value: Owned::EMPTY,
            error: Owned::new(error.to_string()),
        },
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("Rust analysis panicked");
            Outcome {
                value: Owned::EMPTY,
                error: Owned::new(message.to_owned()),
            }
        }
    }
}

#[repr(C)]
pub struct Progress {
    pub context: *mut c_void,
    pub update: Option<unsafe extern "C" fn(*mut c_void, Slice, f64)>,
    pub cancelled: Option<unsafe extern "C" fn(*mut c_void) -> bool>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

// Swift supplies a Sendable owner and transfers one retain to this value.
unsafe impl Send for Progress {}
unsafe impl Sync for Progress {}

impl crate::snapshot::Progress for Progress {
    fn update(&self, phase: &str, fraction: f64) {
        if let Some(update) = self.update {
            unsafe { update(self.context, Slice::new(phase.as_bytes()), fraction) };
        }
    }
    fn cancelled(&self) -> bool {
        self.cancelled.is_some_and(|f| unsafe { f(self.context) })
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            unsafe { destroy(self.context) };
        }
    }
}

#[repr(C)]
pub struct OptionalString {
    pub present: bool,
    pub value: Slice,
}

/// The caller keeps the input's UTF-8 storage alive for the duration of the call.
pub unsafe fn string<'a>(value: Slice) -> &'a str {
    if value.len == 0 {
        return "";
    }
    unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(value.data.cast(), value.len))
    }
}

pub unsafe fn slice<'a, T>(value: Slice) -> &'a [T] {
    if value.len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(value.data.cast(), value.len) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[repr(align(256))]
    struct Nested {
        _text: String,
        _children: Vec<CountDrop>,
    }

    #[test]
    fn box_destruction_preserves_alignment_and_drops_nested_values() {
        let drops = Arc::new(AtomicUsize::new(0));
        let owned = Owned::new(Nested {
            _text: "owned by Rust and released by Swift".into(),
            _children: vec![CountDrop(drops.clone()), CountDrop(drops.clone())],
        });
        assert_eq!(owned.data as usize % 256, 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        unsafe { owned.destroy.unwrap()(owned.data) };
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn arc_transfer_keeps_the_same_allocation_until_its_final_owner() {
        let drops = Arc::new(AtomicUsize::new(0));
        let value = Arc::new(CountDrop(drops.clone()));
        let pointer = Arc::as_ptr(&value).cast();
        let owned = Owned::arc(value.clone());
        assert_eq!(owned.data, pointer);
        assert_eq!(Arc::strong_count(&value), 2);
        unsafe { owned.destroy.unwrap()(owned.data) };
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&value), 1);
        drop(value);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn errors_and_panics_release_captured_owners() {
        for panic in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let captured = CountDrop(drops.clone());
            let outcome = call(move || {
                let _captured = captured;
                if panic {
                    panic!("test panic")
                }
                Err(crate::snapshot::AnalysisError::Failed {
                    message: "test error".into(),
                })
            });
            assert!(outcome.value.data.is_null());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let message = unsafe { &*outcome.error.data.cast::<String>() };
            assert_eq!(message, if panic { "test panic" } else { "test error" });
            unsafe { outcome.error.destroy.unwrap()(outcome.error.data) };
        }
    }
}
