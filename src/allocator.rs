#[cfg(feature = "mimalloc-allocator")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Ask the configured allocator to return unused pages to the operating system.
///
/// This is intentionally a no-op when lazygitrs is built without mimalloc so the
/// rest of the application remains allocator- and platform-agnostic.
#[inline]
pub(crate) fn collect_unused_memory() {
    #[cfg(feature = "mimalloc-allocator")]
    unsafe {
        // `true` requests an aggressive collection. This is only called after
        // the UI has been idle and background allocation-heavy jobs are quiet.
        libmimalloc_sys2::mi_collect(true);
    }
}
