use crate::report::{ProbeError, ProbeResult, Reporter};

pub fn run(reporter: &mut Reporter, font: Option<&str>) -> ProbeResult<()> {
    ffi::run(reporter, font.unwrap_or("monospace"))
}

#[allow(unsafe_code)]
mod ffi {
    use std::ffi::{CStr, CString, c_int};
    use std::ptr;

    use fontconfig_sys as fc;
    use freetype_sys as ft;
    use harfbuzz_sys as hb;

    use super::{ProbeError, ProbeResult, Reporter};

    struct Pattern(&'static fc::Fc, *mut fc::FcPattern);
    impl Drop for Pattern {
        fn drop(&mut self) {
            if !self.1.is_null() {
                unsafe { (self.0.FcPatternDestroy)(self.1) }
            }
        }
    }
    struct FontconfigSession(&'static fc::Fc);
    impl Drop for FontconfigSession {
        fn drop(&mut self) {
            unsafe { (self.0.FcFini)() }
        }
    }
    struct CharSet(&'static fc::Fc, *mut fc::FcCharSet);
    impl Drop for CharSet {
        fn drop(&mut self) {
            if !self.1.is_null() {
                unsafe { (self.0.FcCharSetDestroy)(self.1) }
            }
        }
    }
    struct Library(ft::FT_Library);
    impl Drop for Library {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { ft::FT_Done_FreeType(self.0) };
            }
        }
    }
    struct Face(ft::FT_Face);
    impl Drop for Face {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { ft::FT_Done_Face(self.0) };
            }
        }
    }
    struct Font(*mut hb::hb_font_t);
    impl Drop for Font {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { hb::hb_font_destroy(self.0) }
            }
        }
    }
    struct HbBlob(*mut hb::hb_blob_t);
    impl Drop for HbBlob {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { hb::hb_blob_destroy(self.0) }
            }
        }
    }
    struct HbFace(*mut hb::hb_face_t);
    impl Drop for HbFace {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { hb::hb_face_destroy(self.0) }
            }
        }
    }
    struct Buffer(*mut hb::hb_buffer_t);
    impl Drop for Buffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { hb::hb_buffer_destroy(self.0) }
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct BitmapSummary {
        width: usize,
        rows: usize,
        bytes: usize,
        advance: i64,
    }

    unsafe fn font_path(
        fc: &'static fc::Fc,
        requested: &CString,
        character: Option<char>,
    ) -> ProbeResult<String> {
        let pattern = Pattern(fc, unsafe { (fc.FcNameParse)(requested.as_ptr().cast()) });
        if pattern.1.is_null() {
            return Err(ProbeError::internal(
                "text.fontconfig",
                "FcNameParse returned null",
            ));
        }
        let charset = character.map(|character| {
            let charset = CharSet(fc, unsafe { (fc.FcCharSetCreate)() });
            if !charset.1.is_null() {
                unsafe { (fc.FcCharSetAddChar)(charset.1, character as u32) };
            }
            charset
        });
        if let Some(charset) = charset.as_ref()
            && (charset.1.is_null()
                || unsafe {
                    (fc.FcPatternAddCharSet)(
                        pattern.1,
                        fc::constants::FC_CHARSET.as_ptr(),
                        charset.1,
                    )
                } == 0)
        {
            return Err(ProbeError::internal(
                "text.fallback",
                "could not add fallback charset",
            ));
        }
        if unsafe { (fc.FcConfigSubstitute)(ptr::null_mut(), pattern.1, fc::FcMatchPattern) } == 0 {
            return Err(ProbeError::internal(
                "text.fontconfig",
                "FcConfigSubstitute failed",
            ));
        }
        unsafe { (fc.FcDefaultSubstitute)(pattern.1) };
        let mut result = fc::FcResultMatch;
        let matched = Pattern(fc, unsafe {
            (fc.FcFontMatch)(ptr::null_mut(), pattern.1, &raw mut result)
        });
        if matched.1.is_null() {
            return Err(ProbeError::unsuitable(
                "text.font-match",
                "no matching font",
                "install an Ubuntu monospace font with the required character coverage",
            ));
        }
        let mut path = ptr::null_mut();
        if unsafe {
            (fc.FcPatternGetString)(matched.1, fc::constants::FC_FILE.as_ptr(), 0, &raw mut path)
        } != fc::FcResultMatch
            || path.is_null()
        {
            return Err(ProbeError::internal(
                "text.font-match",
                "matched pattern has no file",
            ));
        }
        Ok(unsafe { CStr::from_ptr(path.cast()) }
            .to_string_lossy()
            .into_owned())
    }

    unsafe fn raster_summary(
        face: ft::FT_Face,
        character: char,
        pixels: u32,
    ) -> ProbeResult<BitmapSummary> {
        if unsafe { ft::FT_Set_Pixel_Sizes(face, 0, pixels) } != 0
            || unsafe { ft::FT_Load_Char(face, u64::from(character as u32), ft::FT_LOAD_RENDER) }
                != 0
        {
            return Err(ProbeError::internal(
                "text.raster",
                format!(
                    "could not rasterize U+{:04X} at {pixels}px",
                    character as u32
                ),
            ));
        }
        let slot = unsafe { (*face).glyph };
        if slot.is_null() {
            return Err(ProbeError::internal(
                "text.raster",
                "FreeType returned a null glyph slot",
            ));
        }
        let bitmap = unsafe { &(*slot).bitmap };
        let width = usize::try_from(bitmap.width)
            .map_err(|_| ProbeError::internal("text.bitmap", "negative bitmap width"))?;
        let rows = usize::try_from(bitmap.rows)
            .map_err(|_| ProbeError::internal("text.bitmap", "negative bitmap height"))?;
        let pitch = usize::try_from(bitmap.pitch.unsigned_abs())
            .map_err(|_| ProbeError::internal("text.bitmap", "bitmap pitch overflow"))?;
        let bytes = rows
            .checked_mul(pitch)
            .filter(|bytes| *bytes <= 16 * 1024 * 1024)
            .ok_or_else(|| ProbeError::internal("text.bitmap", "bitmap exceeds 16 MiB limit"))?;
        if bytes > 0 && bitmap.buffer.is_null() {
            return Err(ProbeError::internal(
                "text.bitmap",
                "non-empty bitmap has a null buffer",
            ));
        }
        Ok(BitmapSummary {
            width,
            rows,
            bytes,
            advance: unsafe { (*slot).advance.x },
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn run(reporter: &mut Reporter, requested: &str) -> ProbeResult<()> {
        let requested = CString::new(requested)
            .map_err(|_| ProbeError::internal("text.font-pattern", "font pattern contains NUL"))?;
        let fc = fc::statics::LIB_RESULT.as_ref().map_err(|error| {
            ProbeError::missing(
                "text.fontconfig",
                error.to_string(),
                "install the Fontconfig runtime library (libfontconfig1)",
            )
        })?;
        // SAFETY: all native owners remain in this scope; every pointer is checked before use and
        // RAII declarations are ordered so HarfBuzz font is destroyed before the FreeType face.
        unsafe {
            if (fc.FcInit)() == 0 {
                return Err(ProbeError::internal("text.fontconfig", "FcInit failed"));
            }
            let _fontconfig = FontconfigSession(fc);
            let path = font_path(fc, &requested, None)?;
            let fallback_path = font_path(fc, &requested, Some('中'))?;
            let path_c = CString::new(path.as_bytes())
                .map_err(|_| ProbeError::internal("text.font-path", "font path contains NUL"))?;

            let mut library = ptr::null_mut();
            if ft::FT_Init_FreeType(&raw mut library) != 0 || library.is_null() {
                return Err(ProbeError::internal(
                    "text.freetype",
                    "FT_Init_FreeType failed",
                ));
            }
            let library = Library(library);
            let mut face = ptr::null_mut();
            if ft::FT_New_Face(library.0, path_c.as_ptr(), 0, &raw mut face) != 0 || face.is_null()
            {
                return Err(ProbeError::internal(
                    "text.freetype",
                    format!("could not open {path}"),
                ));
            }
            let face = Face(face);
            for character in ['A', '中', '\u{301}'] {
                let index = ft::FT_Get_Char_Index(face.0, u64::from(character as u32));
                if index != 0
                    && ft::FT_Load_Char(face.0, u64::from(character as u32), ft::FT_LOAD_RENDER)
                        != 0
                {
                    return Err(ProbeError::internal(
                        "text.raster",
                        format!("could not rasterize U+{:04X}", character as u32),
                    ));
                }
            }
            let raster_1x = raster_summary(face.0, 'A', 16)?;
            let raster_2x = raster_summary(face.0, 'A', 32)?;
            if raster_1x == raster_2x
                || raster_2x.width <= raster_1x.width
                || raster_2x.rows <= raster_1x.rows
            {
                return Err(ProbeError::internal(
                    "text.rerasterize",
                    "32px glyph did not produce larger metrics and bitmap",
                ));
            }

            let blob = HbBlob(hb::hb_blob_create_from_file(path_c.as_ptr()));
            let hb_face = HbFace(hb::hb_face_create(blob.0, 0));
            let font = Font(hb::hb_font_create(hb_face.0));
            let buffer = Buffer(hb::hb_buffer_create());
            if blob.0.is_null() || hb_face.0.is_null() || font.0.is_null() || buffer.0.is_null() {
                return Err(ProbeError::internal(
                    "text.harfbuzz",
                    "could not create blob, face, font, or buffer",
                ));
            }
            hb::hb_ot_font_set_funcs(font.0);
            let sample = "A中e\u{301}";
            hb::hb_buffer_add_utf8(
                buffer.0,
                sample.as_ptr().cast(),
                c_int::try_from(sample.len()).unwrap_or(c_int::MAX),
                0,
                -1,
            );
            hb::hb_buffer_guess_segment_properties(buffer.0);
            hb::hb_shape(font.0, buffer.0, ptr::null(), 0);
            let count = hb::hb_buffer_get_length(buffer.0);
            let mut info_count = count;
            let mut position_count = count;
            let infos = hb::hb_buffer_get_glyph_infos(buffer.0, &raw mut info_count);
            let positions = hb::hb_buffer_get_glyph_positions(buffer.0, &raw mut position_count);
            if count == 0
                || count > 1024
                || infos.is_null()
                || positions.is_null()
                || info_count != count
                || position_count != count
            {
                return Err(ProbeError::internal(
                    "text.harfbuzz",
                    "invalid shaping output",
                ));
            }
            let first = (&*infos, &*positions);
            reporter.pass(
                "text",
                "pipeline",
                format!(
                    "font={path}; glyphs={count}; first_id={}; first_advance={}; first_cluster={}",
                    first.0.codepoint, first.1.x_advance, first.0.cluster
                ),
            );
            reporter.pass("text", "fallback", format!("U+4E2D font={fallback_path}"));
            reporter.pass(
                "text",
                "rerasterize",
                format!("16px={raster_1x:?}; 32px={raster_2x:?}"),
            );
        }
        Ok(())
    }
}
