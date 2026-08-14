#![allow(unsafe_code)]

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_int},
    ptr,
    sync::{Arc, OnceLock},
};

use fontconfig_sys as fc;
use freetype_sys as ft;
use harfbuzz_sys as hb;

use crate::{
    CellMetrics, FaceId, FontRequest, FontStyle, GlyphAsset, GlyphBitmap, GlyphKey, MAX_FACES,
    MAX_GLYPH_BITMAP_BYTES, MAX_GLYPH_BITMAPS, MAX_PREPARED_GLYPHS, ShapedCluster, ShapedGlyph,
    TextError,
};

#[derive(Clone, Copy)]
struct FontconfigOwner(&'static fc::Fc);

struct Library(ft::FT_Library);

impl Drop for Library {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the library is valid and all faces are dropped before this field.
            unsafe { ft::FT_Done_FreeType(self.0) };
        }
    }
}

struct Face {
    hb_font: *mut hb::hb_font_t,
    shaping_source: *mut hb::hb_face_t,
    shaping_blob: *mut hb::hb_blob_t,
    raw: ft::FT_Face,
    id: FaceId,
    style: FontStyle,
}

impl Drop for Face {
    fn drop(&mut self) {
        // SAFETY: both pointers were checked at construction and are uniquely owned here.
        unsafe {
            if !self.hb_font.is_null() {
                hb::hb_font_destroy(self.hb_font);
            }
            if !self.shaping_source.is_null() {
                hb::hb_face_destroy(self.shaping_source);
            }
            if !self.shaping_blob.is_null() {
                hb::hb_blob_destroy(self.shaping_blob);
            }
            if !self.raw.is_null() {
                ft::FT_Done_Face(self.raw);
            }
        }
    }
}

struct Buffer(*mut hb::hb_buffer_t);

struct Pattern(&'static fc::Fc, *mut fc::FcPattern);
impl Drop for Pattern {
    fn drop(&mut self) {
        unsafe { (self.0.FcPatternDestroy)(self.1) }
    }
}
struct Charset(&'static fc::Fc, *mut fc::FcCharSet);
impl Drop for Charset {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.FcCharSetDestroy)(self.1) }
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: buffer is uniquely owned.
            unsafe { hb::hb_buffer_destroy(self.0) }
        }
    }
}

/// Safe facade over process-local Fontconfig and UI-thread-owned FreeType/HarfBuzz objects.
pub struct TextSystem {
    faces: Vec<Face>,
    face_lookup: HashMap<(Arc<str>, u32), FaceId>,
    glyph_cache: HashMap<GlyphKey, GlyphBitmap>,
    glyph_cache_bytes: usize,
    library: Library,
    fontconfig: FontconfigOwner,
    request: FontRequest,
    generation: u64,
    metrics: CellMetrics,
}

/// Fully initialized font resources which can be committed without further native work.
pub struct PreparedFontState {
    base_generation: u64,
    system: TextSystem,
    metrics: CellMetrics,
    generation: u64,
}

impl PreparedFontState {
    #[must_use]
    pub const fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn text_system_mut(&mut self) -> &mut TextSystem {
        &mut self.system
    }
}

impl TextSystem {
    /// Opens the configured primary monospace face and freezes cell metrics for generation one.
    ///
    /// # Errors
    /// Returns a typed environment or font data error when the configured face cannot be opened.
    pub fn new(request: FontRequest) -> Result<Self, TextError> {
        let fontconfig = FontconfigOwner(initialize_fontconfig()?);
        Self::with_generation(request, fontconfig, 1)
    }

    fn with_generation(
        request: FontRequest,
        fontconfig: FontconfigOwner,
        generation: u64,
    ) -> Result<Self, TextError> {
        let library = initialize_freetype()?;
        let mut system = Self {
            faces: Vec::new(),
            face_lookup: HashMap::new(),
            glyph_cache: HashMap::new(),
            glyph_cache_bytes: 0,
            library,
            fontconfig,
            request,
            generation,
            metrics: placeholder_metrics(),
        };
        let primary = system.resolve_face(None, FontStyle::Regular)?;
        system.metrics = metrics(system.face(primary)?.raw)?;
        Ok(system)
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Resolves a complete replacement without changing the active font resources.
    ///
    /// # Errors
    /// Returns a typed font error while leaving `self` untouched.
    pub fn prepare_configure(&self, request: FontRequest) -> Result<PreparedFontState, TextError> {
        let generation = if self.request == request {
            self.generation
        } else {
            self.generation
                .checked_add(1)
                .ok_or(TextError::CapacityExceeded("font generation"))?
        };
        let system = Self::with_generation(request, self.fontconfig, generation)?;
        Ok(PreparedFontState {
            base_generation: self.generation,
            metrics: system.metrics,
            generation,
            system,
        })
    }

    /// Atomically swaps a prepared replacement into the active text system.
    ///
    /// # Errors
    /// Rejects a state prepared against an older active generation.
    pub fn commit_configure(
        &mut self,
        prepared: PreparedFontState,
    ) -> Result<CellMetrics, TextError> {
        if prepared.base_generation != self.generation {
            return Err(TextError::StalePreparedState);
        }
        *self = prepared.system;
        Ok(self.metrics)
    }

    /// Reopens faces and invalidates generation-bound caches after font or scale changes.
    ///
    /// # Errors
    /// Returns a typed font error without changing the resident graphics scene.
    pub fn configure(&mut self, request: FontRequest) -> Result<CellMetrics, TextError> {
        let prepared = self.prepare_configure(request)?;
        self.commit_configure(prepared)
    }

    /// Shapes and rasterizes one indivisible snapshot cluster with bounded output.
    ///
    /// # Errors
    /// Returns a typed shaping, font data, or capacity error.
    pub fn shape_cluster(
        &mut self,
        text: &str,
        style: FontStyle,
    ) -> Result<ShapedCluster, TextError> {
        if text.is_empty() || text.len() > 4096 {
            return Err(TextError::Shape(
                "cluster text is empty or excessive".into(),
            ));
        }
        let face_id = self.resolve_face(Some(text), style)?;
        let face = self.face(face_id)?;
        let buffer = Buffer(unsafe { hb::hb_buffer_create() });
        if buffer.0.is_null() {
            return Err(TextError::Shape("HarfBuzz buffer allocation failed".into()));
        }
        let length = c_int::try_from(text.len())
            .map_err(|_| TextError::CapacityExceeded("cluster bytes"))?;
        // SAFETY: buffer/font are live; text remains borrowed during shaping; result pointers are
        // bounded by the counts returned by HarfBuzz and copied before native objects change.
        unsafe {
            hb::hb_buffer_add_utf8(buffer.0, text.as_ptr().cast(), length, 0, length);
            hb::hb_buffer_set_cluster_level(
                buffer.0,
                hb::HB_BUFFER_CLUSTER_LEVEL_MONOTONE_GRAPHEMES,
            );
            hb::hb_buffer_guess_segment_properties(buffer.0);
            let language = hb::hb_language_from_string(c"und".as_ptr(), -1);
            hb::hb_buffer_set_language(buffer.0, language);
        }
        let features = if self.request.ligatures {
            Vec::new()
        } else {
            [b"liga\0", b"clig\0", b"dlig\0"]
                .into_iter()
                .map(|tag| hb::hb_feature_t {
                    tag: u32::from_be_bytes([tag[0], tag[1], tag[2], tag[3]]),
                    value: 0,
                    start: 0,
                    end: u32::MAX,
                })
                .collect()
        };
        // SAFETY: feature slice and native owners remain valid for the duration of the call.
        unsafe {
            hb::hb_shape(
                face.hb_font,
                buffer.0,
                features.as_ptr(),
                u32::try_from(features.len()).unwrap_or(0),
            );
        }
        let count = unsafe { hb::hb_buffer_get_length(buffer.0) } as usize;
        if count == 0 || count > MAX_PREPARED_GLYPHS {
            return Err(TextError::Shape(
                "HarfBuzz returned an invalid glyph count".into(),
            ));
        }
        let mut info_count =
            u32::try_from(count).map_err(|_| TextError::Shape("glyph count".into()))?;
        let mut position_count = info_count;
        let infos = unsafe { hb::hb_buffer_get_glyph_infos(buffer.0, &raw mut info_count) };
        let positions =
            unsafe { hb::hb_buffer_get_glyph_positions(buffer.0, &raw mut position_count) };
        if infos.is_null()
            || positions.is_null()
            || info_count as usize != count
            || position_count as usize != count
        {
            return Err(TextError::Shape(
                "HarfBuzz returned inconsistent arrays".into(),
            ));
        }
        let infos = unsafe { std::slice::from_raw_parts(infos, count) };
        let positions = unsafe { std::slice::from_raw_parts(positions, count) };
        let synthetic_bold = matches!(style, FontStyle::Bold | FontStyle::BoldItalic)
            && unsafe { (*face.raw).style_flags & ft::FT_STYLE_FLAG_BOLD == 0 };
        let synthetic_italic = matches!(style, FontStyle::Italic | FontStyle::BoldItalic)
            && unsafe { (*face.raw).style_flags & ft::FT_STYLE_FLAG_ITALIC == 0 };
        let mut glyphs = Vec::with_capacity(count);
        let mut assets = Vec::with_capacity(count);
        for (info, position) in infos.iter().zip(positions) {
            if info.cluster as usize >= text.len() || !text.is_char_boundary(info.cluster as usize)
            {
                return Err(TextError::Shape(
                    "glyph cluster is outside UTF-8 boundaries".into(),
                ));
            }
            let key = GlyphKey {
                font_generation: self.generation,
                face: face_id,
                glyph_id: info.codepoint,
                synthetic_bold,
                synthetic_italic,
            };
            glyphs.push(ShapedGlyph {
                key,
                cluster: info.cluster,
                offset_26_6: [position.x_offset, position.y_offset],
                advance_26_6: [position.x_advance, position.y_advance],
            });
            if !assets.iter().any(|asset: &GlyphAsset| asset.key == key) {
                let bitmap = self.rasterize(key)?;
                assets.push(GlyphAsset { key, bitmap });
            }
        }
        Ok(ShapedCluster { glyphs, assets })
    }

    fn rasterize(&mut self, key: GlyphKey) -> Result<GlyphBitmap, TextError> {
        if let Some(bitmap) = self.glyph_cache.get(&key) {
            return Ok(bitmap.clone());
        }
        if self.glyph_cache.len() >= MAX_GLYPH_BITMAPS {
            return Err(TextError::CapacityExceeded("glyph bitmap cache entries"));
        }
        let face = self.face(key.face)?;
        let load = ft::FT_LOAD_DEFAULT | ft::FT_LOAD_TARGET_NORMAL;
        // SAFETY: face is UI-thread-owned and glyph slot data is copied before the next call.
        if unsafe { ft::FT_Load_Glyph(face.raw, key.glyph_id, load) } != 0 {
            return Err(TextError::FontData(format!(
                "cannot load glyph {}",
                key.glyph_id
            )));
        }
        let slot = unsafe { (*face.raw).glyph };
        if slot.is_null() {
            return Err(TextError::FontData(
                "FreeType returned a null glyph slot".into(),
            ));
        }
        if key.synthetic_bold {
            unsafe { ft::FT_GlyphSlot_Embolden(slot) };
        }
        if unsafe { ft::FT_Render_Glyph(slot, ft::FT_RENDER_MODE_NORMAL) } != 0 {
            return Err(TextError::FontData(format!(
                "cannot render glyph {}",
                key.glyph_id
            )));
        }
        let raw = unsafe { &(*slot).bitmap };
        if raw.pixel_mode != i8::try_from(ft::FT_PIXEL_MODE_GRAY).expect("gray mode fits i8") {
            return Err(TextError::FontData(
                "glyph is not grayscale coverage".into(),
            ));
        }
        let width = usize::try_from(raw.width)
            .map_err(|_| TextError::FontData("negative glyph width".into()))?;
        let rows = usize::try_from(raw.rows)
            .map_err(|_| TextError::FontData("negative glyph height".into()))?;
        if width > 2046 || rows > 2046 {
            return Err(TextError::CapacityExceeded("glyph dimensions"));
        }
        let pitch = usize::try_from(raw.pitch.unsigned_abs())
            .map_err(|_| TextError::CapacityExceeded("glyph pitch"))?;
        let source_len = rows
            .checked_mul(pitch)
            .ok_or(TextError::CapacityExceeded("glyph bitmap"))?;
        let output_len = rows
            .checked_mul(width)
            .ok_or(TextError::CapacityExceeded("glyph bitmap"))?;
        if self
            .glyph_cache_bytes
            .checked_add(output_len)
            .as_ref()
            .is_none_or(|value| *value > MAX_GLYPH_BITMAP_BYTES)
        {
            return Err(TextError::CapacityExceeded("glyph bitmap cache bytes"));
        }
        if source_len > 0 && raw.buffer.is_null() {
            return Err(TextError::FontData("glyph bitmap has a null buffer".into()));
        }
        let source = if source_len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(raw.buffer, source_len) }
        };
        let mut coverage = vec![0_u8; output_len];
        for row in 0..rows {
            let source_row = if raw.pitch < 0 { rows - 1 - row } else { row };
            coverage[row * width..(row + 1) * width]
                .copy_from_slice(&source[source_row * pitch..source_row * pitch + width]);
        }
        let bitmap = GlyphBitmap {
            size_px: [
                u16::try_from(width).map_err(|_| TextError::CapacityExceeded("glyph width"))?,
                u16::try_from(rows).map_err(|_| TextError::CapacityExceeded("glyph height"))?,
            ],
            bearing_px: [
                checked_i16(unsafe { (*slot).bitmap_left })?,
                checked_i16(unsafe { (*slot).bitmap_top })?,
            ],
            advance_26_6: i32::try_from(unsafe { (*slot).advance.x })
                .map_err(|_| TextError::FontData("glyph advance overflow".into()))?,
            coverage: Arc::from(coverage),
        };
        self.glyph_cache_bytes += output_len;
        self.glyph_cache.insert(key, bitmap.clone());
        Ok(bitmap)
    }

    fn resolve_face(
        &mut self,
        cluster: Option<&str>,
        style: FontStyle,
    ) -> Result<FaceId, TextError> {
        if let Some(text) = cluster
            && let Some(face) = self
                .faces
                .iter()
                .find(|face| face.style == style && face_supports(face.raw, text))
        {
            return Ok(face.id);
        }
        let (path, index) = font_match(self.fontconfig.0, &self.request, cluster, style)?;
        if let Some(id) = self.face_lookup.get(&(path.clone(), index)) {
            return Ok(*id);
        }
        if self.faces.len() >= MAX_FACES {
            return Err(TextError::CapacityExceeded("font faces"));
        }
        let path_c = CString::new(path.as_bytes())
            .map_err(|_| TextError::FontData("font path contains NUL".into()))?;
        let mut raw_face = ptr::null_mut();
        if unsafe {
            ft::FT_New_Face(
                self.library.0,
                path_c.as_ptr(),
                i64::from(index),
                &raw mut raw_face,
            )
        } != 0
            || raw_face.is_null()
        {
            return Err(TextError::FontData(format!(
                "cannot open matched font {path}"
            )));
        }
        let size = self.request.physical_size_26_6()?;
        if unsafe { ft::FT_Set_Char_Size(raw_face, 0, size, 72, 72) } != 0 {
            unsafe { ft::FT_Done_Face(raw_face) };
            return Err(TextError::FontData(
                "cannot set FreeType character size".into(),
            ));
        }
        let hb_blob = unsafe { hb::hb_blob_create_from_file(path_c.as_ptr()) };
        let hb_face = unsafe { hb::hb_face_create(hb_blob, index) };
        let hb_font = unsafe { hb::hb_font_create(hb_face) };
        if hb_blob.is_null() || hb_face.is_null() || hb_font.is_null() {
            unsafe {
                if !hb_font.is_null() {
                    hb::hb_font_destroy(hb_font);
                }
                if !hb_face.is_null() {
                    hb::hb_face_destroy(hb_face);
                }
                if !hb_blob.is_null() {
                    hb::hb_blob_destroy(hb_blob);
                }
            }
            unsafe { ft::FT_Done_Face(raw_face) };
            return Err(TextError::FontData(
                "cannot create HarfBuzz font objects".into(),
            ));
        }
        let hb_scale =
            i32::try_from(size).map_err(|_| TextError::CapacityExceeded("HarfBuzz scale"))?;
        unsafe {
            hb::hb_ot_font_set_funcs(hb_font);
            hb::hb_font_set_scale(hb_font, hb_scale, hb_scale);
        }
        let id = FaceId(
            u32::try_from(self.faces.len())
                .map_err(|_| TextError::CapacityExceeded("font faces"))?,
        );
        self.faces.push(Face {
            hb_font,
            shaping_source: hb_face,
            shaping_blob: hb_blob,
            raw: raw_face,
            id,
            style,
        });
        self.face_lookup.insert((path, index), id);
        Ok(id)
    }

    fn face(&self, id: FaceId) -> Result<&Face, TextError> {
        self.faces
            .get(id.0 as usize)
            .filter(|face| face.id == id)
            .ok_or_else(|| TextError::FontData("unknown face id".into()))
    }
}

fn face_supports(face: ft::FT_Face, text: &str) -> bool {
    text.chars()
        .all(|ch| unsafe { ft::FT_Get_Char_Index(face, u64::from(u32::from(ch))) } != 0)
}

fn initialize_freetype() -> Result<Library, TextError> {
    let mut raw_library = ptr::null_mut();
    // SAFETY: output pointer is valid and checked immediately.
    if unsafe { ft::FT_Init_FreeType(&raw mut raw_library) } != 0 || raw_library.is_null() {
        return Err(TextError::Environment(
            "FreeType initialization failed".into(),
        ));
    }
    Ok(Library(raw_library))
}

fn initialize_fontconfig() -> Result<&'static fc::Fc, TextError> {
    static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
    let fc = fc::statics::LIB_RESULT
        .as_ref()
        .map_err(|error| TextError::Environment(format!("load libfontconfig.so.1: {error}")))?;
    let initialized = INITIALIZED.get_or_init(|| {
        // SAFETY: OnceLock makes FcInit process-wide and one-shot. FcFini is intentionally not
        // called, since it tears down global state that other TextSystem instances may still use.
        if unsafe { (fc.FcInit)() } == 0 {
            Err("Fontconfig initialization failed".into())
        } else {
            Ok(())
        }
    });
    initialized
        .as_ref()
        .map(|()| fc)
        .map_err(|error| TextError::Environment(error.clone()))
}

fn font_match(
    fc: &'static fc::Fc,
    request: &FontRequest,
    cluster: Option<&str>,
    style: FontStyle,
) -> Result<(Arc<str>, u32), TextError> {
    let family = CString::new(request.family.as_bytes())
        .map_err(|_| TextError::InvalidRequest("font family contains NUL"))?;
    let pattern = unsafe { (fc.FcPatternCreate)() };
    if pattern.is_null() {
        return Err(TextError::Environment(
            "Fontconfig pattern allocation failed".into(),
        ));
    }
    let pattern = Pattern(fc, pattern);
    unsafe {
        (fc.FcPatternAddString)(
            pattern.1,
            fc::constants::FC_FAMILY.as_ptr(),
            family.as_ptr().cast(),
        );
        (fc.FcPatternAddInteger)(
            pattern.1,
            fc::constants::FC_SPACING.as_ptr(),
            fc::constants::FC_MONO,
        );
        (fc.FcPatternAddInteger)(
            pattern.1,
            fc::constants::FC_WEIGHT.as_ptr(),
            if matches!(style, FontStyle::Bold | FontStyle::BoldItalic) {
                fc::constants::FC_WEIGHT_BOLD
            } else {
                fc::constants::FC_WEIGHT_REGULAR
            },
        );
        (fc.FcPatternAddInteger)(
            pattern.1,
            fc::constants::FC_SLANT.as_ptr(),
            if matches!(style, FontStyle::Italic | FontStyle::BoldItalic) {
                fc::constants::FC_SLANT_ITALIC
            } else {
                fc::constants::FC_SLANT_ROMAN
            },
        );
    }
    if let Some(text) = cluster {
        let charset = Charset(fc, unsafe { (fc.FcCharSetCreate)() });
        if charset.1.is_null() {
            return Err(TextError::Environment(
                "Fontconfig charset allocation failed".into(),
            ));
        }
        for ch in text.chars() {
            unsafe { (fc.FcCharSetAddChar)(charset.1, ch as u32) };
        }
        if unsafe {
            (fc.FcPatternAddCharSet)(pattern.1, fc::constants::FC_CHARSET.as_ptr(), charset.1)
        } == 0
        {
            return Err(TextError::Environment("Fontconfig rejected charset".into()));
        }
    }
    if unsafe { (fc.FcConfigSubstitute)(ptr::null_mut(), pattern.1, fc::FcMatchPattern) } == 0 {
        return Err(TextError::Environment(
            "Fontconfig substitution failed".into(),
        ));
    }
    unsafe { (fc.FcDefaultSubstitute)(pattern.1) };
    let mut result = fc::FcResultMatch;
    let matched = unsafe { (fc.FcFontMatch)(ptr::null_mut(), pattern.1, &raw mut result) };
    if matched.is_null() {
        return Err(TextError::Environment(
            "Fontconfig found no matching face".into(),
        ));
    }
    let matched = Pattern(fc, matched);
    let mut path = ptr::null_mut();
    if unsafe {
        (fc.FcPatternGetString)(matched.1, fc::constants::FC_FILE.as_ptr(), 0, &raw mut path)
    } != fc::FcResultMatch
        || path.is_null()
    {
        return Err(TextError::FontData("matched face has no file".into()));
    }
    let path = unsafe { CStr::from_ptr(path.cast()) }
        .to_string_lossy()
        .into_owned();
    let mut index = 0;
    let status = unsafe {
        (fc.FcPatternGetInteger)(
            matched.1,
            fc::constants::FC_INDEX.as_ptr(),
            0,
            &raw mut index,
        )
    };
    if status != fc::FcResultMatch {
        index = 0;
    }
    let index =
        u32::try_from(index).map_err(|_| TextError::FontData("negative face index".into()))?;
    Ok((Arc::from(path), index))
}

fn metrics(face: ft::FT_Face) -> Result<CellMetrics, TextError> {
    let size = unsafe { (*face).size };
    if size.is_null() {
        return Err(TextError::FontData("face has no active size".into()));
    }
    let value = unsafe { (*size).metrics };
    let width = ceil_26_6(value.max_advance)?.max(1);
    let height = ceil_26_6(value.height)?.max(1);
    let baseline = ceil_26_6(value.ascender)?;
    let y_scale = value.y_scale;
    let underline = i64::from(unsafe { (*face).underline_position }) * y_scale / 65_536 / 64;
    let thickness =
        (i64::from(unsafe { (*face).underline_thickness }) * y_scale / 65_536 / 64).max(1);
    Ok(CellMetrics {
        width_px: std::num::NonZeroU16::new(
            u16::try_from(width).map_err(|_| TextError::FontData("cell width overflow".into()))?,
        )
        .ok_or_else(|| TextError::FontData("zero cell width".into()))?,
        height_px: std::num::NonZeroU16::new(
            u16::try_from(height)
                .map_err(|_| TextError::FontData("cell height overflow".into()))?,
        )
        .ok_or_else(|| TextError::FontData("zero cell height".into()))?,
        baseline_px: checked_i16(baseline)?,
        underline_y_px: checked_i16(baseline - underline.max(-height))?,
        underline_thickness_px: std::num::NonZeroU16::new(u16::try_from(thickness).unwrap_or(1))
            .expect("one is nonzero"),
        strike_y_px: checked_i16(baseline - height / 3)?,
        strike_thickness_px: std::num::NonZeroU16::new(1).expect("one is nonzero"),
    })
}

fn ceil_26_6(value: i64) -> Result<i64, TextError> {
    value
        .checked_add(63)
        .map(|value| value / 64)
        .ok_or(TextError::CapacityExceeded("font metrics"))
}

fn checked_i16<T: TryInto<i16>>(value: T) -> Result<i16, TextError> {
    value
        .try_into()
        .map_err(|_| TextError::FontData("font metric exceeds i16".into()))
}

fn placeholder_metrics() -> CellMetrics {
    CellMetrics {
        width_px: std::num::NonZeroU16::new(1).expect("one is nonzero"),
        height_px: std::num::NonZeroU16::new(1).expect("one is nonzero"),
        baseline_px: 1,
        underline_y_px: 1,
        underline_thickness_px: std::num::NonZeroU16::new(1).expect("one is nonzero"),
        strike_y_px: 1,
        strike_thickness_px: std::num::NonZeroU16::new(1).expect("one is nonzero"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_supported_clusters_reuse_the_loaded_face() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();
        let initial_faces = system.faces.len();
        for cluster in ["a", "terminal", "123", "e\u{301}", "terminal"] {
            system.shape_cluster(cluster, FontStyle::Regular).unwrap();
        }
        assert_eq!(system.faces.len(), initial_faces);
    }
}
