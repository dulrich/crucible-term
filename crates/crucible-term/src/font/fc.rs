//! Fontconfig discovery: the configured family per style, and fallback faces for characters the
//! primary faces lack. The only FFI in the crate; every call is wrapped here.

#![allow(unsafe_code)]

use super::Style;
use fontconfig_sys as sys;
use fontconfig_sys::constants::{
    FC_CHARSET, FC_COLOR, FC_FAMILY, FC_FILE, FC_INDEX, FC_SLANT, FC_SLANT_ITALIC, FC_WEIGHT,
    FC_WEIGHT_BOLD,
};
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// A located font file: path and face index within it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Located {
    /// The font file.
    pub path: PathBuf,
    /// Face index (non-zero inside `.ttc` collections).
    pub index: u32,
}

/// An initialised fontconfig configuration.
pub struct Fontconfig {
    config: *mut sys::FcConfig,
}

impl Fontconfig {
    /// Load the user's fontconfig configuration and font list. `None` if fontconfig fails.
    pub fn new() -> Option<Fontconfig> {
        // SAFETY: plain constructor; a null return is handled.
        let config = unsafe { sys::FcInitLoadConfigAndFonts() };
        (!config.is_null()).then_some(Fontconfig { config })
    }

    /// The best match for `family` in `style`; with `ch`, only a face that covers `ch`
    /// (fallback); with `color`, prefer colour fonts (emoji).
    pub fn find(
        &self,
        family: &str,
        style: Style,
        ch: Option<char>,
        color: bool,
    ) -> Option<Located> {
        let family = CString::new(family).ok()?;
        // SAFETY: every pointer passed is either freshly created here and checked for null, or a
        // NUL-terminated constant; patterns and charsets are destroyed exactly once below, and
        // strings read out of `m` are copied before `m` is destroyed.
        unsafe {
            let pat = sys::FcPatternCreate();
            if pat.is_null() {
                return None;
            }
            sys::FcPatternAddString(pat, FC_FAMILY.as_ptr(), family.as_ptr().cast());
            if matches!(style, Style::Bold | Style::BoldItalic) {
                sys::FcPatternAddInteger(pat, FC_WEIGHT.as_ptr(), FC_WEIGHT_BOLD);
            }
            if matches!(style, Style::Italic | Style::BoldItalic) {
                sys::FcPatternAddInteger(pat, FC_SLANT.as_ptr(), FC_SLANT_ITALIC);
            }
            if let Some(ch) = ch {
                let cs = sys::FcCharSetCreate();
                if !cs.is_null() {
                    sys::FcCharSetAddChar(cs, ch as u32);
                    sys::FcPatternAddCharSet(pat, FC_CHARSET.as_ptr(), cs);
                    sys::FcCharSetDestroy(cs);
                }
            }
            if color {
                sys::FcPatternAddBool(pat, FC_COLOR.as_ptr(), 1);
            }
            sys::FcConfigSubstitute(self.config, pat, sys::FcMatchPattern);
            sys::FcDefaultSubstitute(pat);
            let mut res = 0;
            let m = sys::FcFontMatch(self.config, pat, &mut res);
            sys::FcPatternDestroy(pat);
            if m.is_null() {
                return None;
            }
            let found = (|| {
                if let Some(ch) = ch {
                    let mut cs = std::ptr::null_mut();
                    if sys::FcPatternGetCharSet(m, FC_CHARSET.as_ptr(), 0, &mut cs)
                        != sys::FcResultMatch
                        || sys::FcCharSetHasChar(cs, ch as u32) == 0
                    {
                        return None;
                    }
                }
                let mut file = std::ptr::null_mut();
                if sys::FcPatternGetString(m, FC_FILE.as_ptr(), 0, &mut file) != sys::FcResultMatch
                {
                    return None;
                }
                let mut index = 0;
                sys::FcPatternGetInteger(m, FC_INDEX.as_ptr(), 0, &mut index);
                let bytes = CStr::from_ptr(file.cast()).to_bytes();
                Some(Located {
                    path: PathBuf::from(OsStr::from_bytes(bytes)),
                    index: u32::try_from(index).unwrap_or(0),
                })
            })();
            sys::FcPatternDestroy(m);
            found
        }
    }
}

impl Drop for Fontconfig {
    fn drop(&mut self) {
        // SAFETY: `config` came from FcInitLoadConfigAndFonts and is destroyed once.
        unsafe { sys::FcConfigDestroy(self.config) }
    }
}
