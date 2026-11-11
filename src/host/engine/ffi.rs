// SPDX-License-Identifier: GPL-3.0-or-later
//! The native Hoshidicts engine (`native/bindings.cpp`), behind a safe interface.
//! This is the crate's only `unsafe` code.
//!
//! The C++ side is one process-wide engine whose replies live in function-local
//! statics until the next call, so every call goes through [`Native::lock`], and
//! each reply is copied out before the lock is released.

#![allow(unsafe_code, reason = "the one FFI module")]

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::{Mutex, MutexGuard, PoisonError};

mod sys {
    use std::ffi::{c_char, c_double, c_int};

    unsafe extern "C" {
        pub(super) fn hdw_init_storage(root: *const c_char) -> c_int;
        pub(super) fn hdw_last_error() -> *const c_char;
        pub(super) fn hdw_reset();
        pub(super) fn hdw_import(zip_path: *const c_char, out_dir: *const c_char, low_ram: c_int) -> *const c_char;
        pub(super) fn hdw_add_dict(path: *const c_char, kind: c_int, paged: c_int, index_paged: c_int) -> c_int;
        pub(super) fn hdw_lookup(text: *const c_char, max_results: c_int, scan_length: c_int, options: *const c_char) -> *const c_char;
        pub(super) fn hdw_lookup_dictionary(
            text: *const c_char,
            dictionary_path: *const c_char,
            max_results: c_int,
            scan_length: usize,
            options: *const c_char,
        ) -> *const c_char;
        pub(super) fn hdw_segment(text: *const c_char, scan_length: c_int, options: *const c_char) -> *const c_char;
        pub(super) fn hdw_kanji(character: *const c_char) -> *const c_char;
        pub(super) fn hdw_kanji_with_options(character: *const c_char, options: *const c_char) -> *const c_char;
        pub(super) fn hdw_language_load(language: *const c_char, transforms: *const c_char) -> c_int;
        #[cfg_attr(not(test), expect(dead_code, reason = "the extension's engine never calls it; the ported language smoke test does"))]
        pub(super) fn hdw_languages() -> *const c_char;
        #[cfg_attr(not(test), expect(dead_code, reason = "the extension's engine never calls it; the ported language smoke test does"))]
        pub(super) fn hdw_deinflect(text: *const c_char, code: *const c_char) -> *const c_char;
        pub(super) fn hdw_styles() -> *const c_char;
        pub(super) fn hdw_tags() -> *const c_char;
        pub(super) fn hdw_media(dictionary: *const c_char, path: *const c_char) -> c_int;
        pub(super) fn hdw_media_data() -> *const u8;
        pub(super) fn hdw_page_cache_bytes() -> c_double;
        pub(super) fn hdw_memory_stats() -> *const c_char;
        pub(super) fn hdw_hash_index_paged(path: *const c_char) -> c_int;
    }
}

/// Which kind of dictionary a package is added as (`hdw_add_dict`'s `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Term = 0,
    Frequency = 1,
    Pitch = 2,
    Kanji = 3,
}

/// What a call reports through `hdw_last_error`.
pub(crate) type Failure = String;

/// The lock on the one native engine. `owner` says whose dictionaries it holds,
/// so two hosts in one process (tests) each reload their own set; `languages`
/// are the shipped packs loaded into it, which no reset unloads.
pub(crate) struct Native {
    pub(crate) owner: Option<(u64, u64)>,
    pub(crate) languages: BTreeSet<String>,
}

static ENGINE: Mutex<Native> = Mutex::new(Native { owner: None, languages: BTreeSet::new() });

/// A C string from text the caller has already checked for NUL; an interior NUL
/// becomes an empty string, which every native call treats as "nothing".
fn c(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

/// Copies a native reply out. The pointer is a function-local static, valid
/// until the next call into the engine, which the lock rules out.
fn owned(reply: *const c_char) -> String {
    if reply.is_null() {
        return String::new();
    }
    // SAFETY: every hdw_* string return is a NUL-terminated `std::string::c_str()`
    // that outlives this copy (see above).
    unsafe { CStr::from_ptr(reply) }.to_string_lossy().into_owned()
}

impl Native {
    /// Takes the engine for one batch of calls.
    pub(crate) fn lock() -> MutexGuard<'static, Self> {
        ENGINE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[allow(clippy::unused_self, reason = "the receiver proves the lock is held")]
    fn error(&self) -> Option<Failure> {
        // SAFETY: no arguments; returns a static string.
        let error = owned(unsafe { sys::hdw_last_error() });
        (!error.is_empty()).then_some(error)
    }

    /// The reply of a call that reports failure only through `hdw_last_error`.
    fn checked(&self, reply: String) -> Result<String, Failure> {
        self.error().map_or(Ok(reply), Err)
    }

    /// Finishes an import interrupted under `root`, the dictionary directory.
    #[allow(clippy::needless_pass_by_ref_mut, reason = "the engine is global; `&mut` holds its lock")]
    pub(crate) fn init_storage(&mut self, root: &str) -> Result<(), Failure> {
        let root = c(root);
        // SAFETY: a valid C string for the duration of the call.
        let ok = unsafe { sys::hdw_init_storage(root.as_ptr()) };
        if ok == 1 { Ok(()) } else { Err(self.error().unwrap_or_default()) }
    }

    /// `hdw_import`: imports the archive at `zip` into `out_dir`, the folder its
    /// package is installed in under its title. Returns the JSON import report,
    /// which carries any failure itself.
    #[allow(clippy::needless_pass_by_ref_mut, clippy::unused_self, reason = "the engine is global; `&mut` holds its lock")]
    pub(crate) fn import(&mut self, zip: &str, out_dir: &str) -> String {
        let (zip, out_dir) = (c(zip), c(out_dir));
        // SAFETY: valid C strings for the duration of the call.
        owned(unsafe { sys::hdw_import(zip.as_ptr(), out_dir.as_ptr(), 0) })
    }

    /// Drops every loaded dictionary.
    pub(crate) fn reset(&mut self) {
        // SAFETY: no arguments.
        unsafe { sys::hdw_reset() };
        self.owner = None;
    }

    /// Adds the package at `path` as `kind`.
    #[allow(clippy::needless_pass_by_ref_mut, reason = "the engine is global; `&mut` holds its lock")]
    pub(crate) fn add_dict(&mut self, path: &str, kind: Kind, paged: bool, index_paged: bool) -> Result<(), Failure> {
        let path = c(path);
        // SAFETY: a valid C string for the duration of the call.
        let ok = unsafe { sys::hdw_add_dict(path.as_ptr(), kind as c_int, c_int::from(paged), c_int::from(index_paged)) };
        if ok == 1 { Ok(()) } else { Err(self.error().unwrap_or_default()) }
    }

    /// `hdw_lookup`: the JSON `{results, dictionaryCount}`.
    pub(crate) fn lookup(&self, text: &str, max_results: i32, scan_length: i32, options: &str) -> Result<String, Failure> {
        let (text, options) = (c(text), c(options));
        // SAFETY: valid C strings for the duration of the call.
        self.checked(owned(unsafe { sys::hdw_lookup(text.as_ptr(), max_results, scan_length, options.as_ptr()) }))
    }

    /// `hdw_lookup_dictionary`: a lookup in the one term package at `path`.
    pub(crate) fn lookup_dictionary(
        &self,
        text: &str,
        path: &str,
        max_results: i32,
        scan_length: usize,
        options: &str,
    ) -> Result<String, Failure> {
        let (text, path, options) = (c(text), c(path), c(options));
        // SAFETY: valid C strings for the duration of the call.
        let reply = unsafe { sys::hdw_lookup_dictionary(text.as_ptr(), path.as_ptr(), max_results, scan_length, options.as_ptr()) };
        self.checked(owned(reply))
    }

    /// `hdw_segment`: the JSON `{spans}`.
    pub(crate) fn segment(&self, text: &str, scan_length: i32, options: &str) -> Result<String, Failure> {
        let (text, options) = (c(text), c(options));
        // SAFETY: valid C strings for the duration of the call.
        self.checked(owned(unsafe { sys::hdw_segment(text.as_ptr(), scan_length, options.as_ptr()) }))
    }

    /// `hdw_kanji`: the JSON `{character, entries, frequencies}`.
    pub(crate) fn kanji(&self, character: &str) -> Result<String, Failure> {
        let character = c(character);
        // SAFETY: a valid C string for the duration of the call.
        self.checked(owned(unsafe { sys::hdw_kanji(character.as_ptr()) }))
    }

    /// `hdw_kanji_with_options`: `hdw_kanji` over the `dictionaries` the options name.
    pub(crate) fn kanji_with_options(&self, character: &str, options: &str) -> Result<String, Failure> {
        let (character, options) = (c(character), c(options));
        // SAFETY: valid C strings for the duration of the call.
        self.checked(owned(unsafe { sys::hdw_kanji_with_options(character.as_ptr(), options.as_ptr()) }))
    }

    /// `hdw_language_load`: adds a language pack, its `language.json` and the
    /// transforms file it names (empty when it names none). Packs outlive [`Self::reset`].
    #[allow(clippy::needless_pass_by_ref_mut, reason = "the engine is global; `&mut` holds its lock")]
    pub(crate) fn language_load(&mut self, language: &str, transforms: &str) -> Result<(), Failure> {
        let (language, transforms) = (c(language), c(transforms));
        // SAFETY: valid C strings for the duration of the call.
        let ok = unsafe { sys::hdw_language_load(language.as_ptr(), transforms.as_ptr()) };
        if ok == 1 { Ok(()) } else { Err(self.error().unwrap_or_default()) }
    }

    /// `hdw_languages`: the JSON array of the loaded packs' codes, sorted.
    #[cfg_attr(not(test), expect(dead_code, reason = "the extension's engine never calls it; the ported language smoke test does"))]
    pub(crate) fn languages(&self) -> Result<String, Failure> {
        // SAFETY: no arguments.
        self.checked(owned(unsafe { sys::hdw_languages() }))
    }

    /// `hdw_deinflect`: the JSON `{input, results}` of `text`'s deinflections in `code`.
    #[cfg_attr(not(test), expect(dead_code, reason = "the extension's engine never calls it; the ported language smoke test does"))]
    pub(crate) fn deinflect(&self, text: &str, code: &str) -> Result<String, Failure> {
        let (text, code) = (c(text), c(code));
        // SAFETY: valid C strings for the duration of the call.
        self.checked(owned(unsafe { sys::hdw_deinflect(text.as_ptr(), code.as_ptr()) }))
    }

    /// `hdw_styles`: the JSON array of each dictionary's styles.
    pub(crate) fn styles(&self) -> Result<String, Failure> {
        // SAFETY: no arguments.
        self.checked(owned(unsafe { sys::hdw_styles() }))
    }

    /// `hdw_tags`: the JSON array of each dictionary's tag bank.
    pub(crate) fn tags(&self) -> Result<String, Failure> {
        // SAFETY: no arguments.
        self.checked(owned(unsafe { sys::hdw_tags() }))
    }

    /// The bytes of one media file, `None` when the dictionary has no such file.
    pub(crate) fn media(&self, dictionary: &str, path: &str) -> Result<Option<Vec<u8>>, Failure> {
        let (dictionary, path) = (c(dictionary), c(path));
        // SAFETY: valid C strings for the duration of the call.
        let length = unsafe { sys::hdw_media(dictionary.as_ptr(), path.as_ptr()) };
        if let Some(error) = self.error() {
            return Err(error);
        }
        let Ok(length @ 1..) = usize::try_from(length) else { return Ok(None) };
        // SAFETY: no arguments; the buffer holds `length` bytes until the next call.
        let data = unsafe { sys::hdw_media_data() };
        if data.is_null() {
            return Ok(None);
        }
        // SAFETY: `data` points at `length` initialised bytes (hdw_media's reply).
        Ok(Some(unsafe { std::slice::from_raw_parts(data, length) }.to_vec()))
    }

    /// Bytes of dictionary pages the engine caches.
    #[allow(clippy::unused_self, reason = "the receiver proves the lock is held")]
    pub(crate) fn page_cache_bytes(&self) -> f64 {
        // SAFETY: no arguments.
        unsafe { sys::hdw_page_cache_bytes() }
    }

    /// `hdw_memory_stats`: the JSON page-cache and allocator statistics.
    #[allow(clippy::unused_self, reason = "the receiver proves the lock is held")]
    pub(crate) fn memory_stats(&self) -> String {
        // SAFETY: no arguments.
        owned(unsafe { sys::hdw_memory_stats() })
    }

    /// Whether the package at `path` reads its hash table from disk.
    #[allow(clippy::unused_self, reason = "the receiver proves the lock is held")]
    pub(crate) fn hash_index_paged(&self, path: &str) -> bool {
        let path = c(path);
        // SAFETY: a valid C string for the duration of the call.
        unsafe { sys::hdw_hash_index_paged(path.as_ptr()) == 1 }
    }
}
