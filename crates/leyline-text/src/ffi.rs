#![allow(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString, c_int},
    ptr,
    sync::{Arc, OnceLock},
};

use fontconfig_sys as fc;
use freetype_sys as ft;
use harfbuzz_sys as hb;

use crate::{
    AntialiasMode, AntialiasPreference, CacheStats, CellMetrics, FaceId, FontRequest, FontStyle,
    GlyphAsset, GlyphBitmap, GlyphFormat, GlyphKey, HintingMode, HintingPreference, MAX_FACES,
    MAX_GLYPH_BITMAP_BYTES, MAX_GLYPH_BITMAPS, MAX_PREPARED_GLYPHS, RasterProfile, ResolvedFace,
    ShapedCluster, ShapedGlyph, ShapedRun, TextDirection, TextError,
};

struct CachedGlyph {
    bitmap: GlyphBitmap,
    last_used: u64,
}

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
    glyph_cache: HashMap<GlyphKey, CachedGlyph>,
    glyph_cache_bytes: usize,
    cache_clock: u64,
    cache_evictions: u64,
    scene_pins: Option<HashSet<GlyphKey>>,
    library: Library,
    fontconfig: FontconfigOwner,
    request: FontRequest,
    generation: u64,
    metrics: CellMetrics,
    raster_profile: RasterProfile,
    resolved_primary: Option<ResolvedFace>,
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
        let raster_profile = requested_profile(&request);
        let mut system = Self {
            faces: Vec::new(),
            face_lookup: HashMap::new(),
            glyph_cache: HashMap::new(),
            glyph_cache_bytes: 0,
            cache_clock: 0,
            cache_evictions: 0,
            scene_pins: None,
            library,
            fontconfig,
            request,
            generation,
            metrics: placeholder_metrics(),
            raster_profile,
            resolved_primary: None,
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

    #[must_use]
    pub const fn raster_profile(&self) -> RasterProfile {
        self.raster_profile
    }

    #[must_use]
    pub fn resolved_primary(&self) -> Option<&ResolvedFace> {
        self.resolved_primary.as_ref()
    }

    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            entries: self.glyph_cache.len(),
            bytes: self.glyph_cache_bytes,
            evictions: self.cache_evictions,
            pinned: self.scene_pins.as_ref().map_or(0, HashSet::len),
        }
    }

    /// Pins every glyph touched until [`Self::end_scene`] so one scene cannot evict and
    /// rerasterize its own working set while it is being composed.
    pub fn begin_scene(&mut self) {
        self.scene_pins = Some(HashSet::new());
    }

    pub fn end_scene(&mut self) {
        self.scene_pins = None;
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
        let shaped = self.shape_cluster_only(text, style)?;
        let keys = shaped
            .glyphs
            .iter()
            .map(|glyph| glyph.key)
            .collect::<Vec<_>>();
        let assets = self.rasterize_working_set(&keys)?;
        Ok(ShapedCluster {
            glyphs: shaped.glyphs,
            assets,
        })
    }

    /// Shapes a cluster without rasterizing any glyphs.
    ///
    /// This is the first phase of frame preparation. Callers can collect every unique key,
    /// validate the complete working set, and then rasterize it as one pinned transaction.
    ///
    /// # Errors
    /// Returns a typed shaping, font data, or capacity error.
    pub fn shape_cluster_only(
        &mut self,
        text: &str,
        style: FontStyle,
    ) -> Result<ShapedRun, TextError> {
        self.shape_run_only(text, style, TextDirection::Auto)
    }

    /// Shapes a complete directional run without rasterizing it.
    ///
    /// # Errors
    /// Returns a typed shaping, font-data, or capacity error.
    #[allow(clippy::too_many_lines)]
    pub fn shape_run_only(
        &mut self,
        text: &str,
        style: FontStyle,
        direction: TextDirection,
    ) -> Result<ShapedRun, TextError> {
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
            match direction {
                TextDirection::Auto => {}
                TextDirection::LeftToRight => {
                    hb::hb_buffer_set_direction(buffer.0, hb::HB_DIRECTION_LTR);
                }
                TextDirection::RightToLeft => {
                    hb::hb_buffer_set_direction(buffer.0, hb::HB_DIRECTION_RTL);
                }
            }
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
                presentation: infer_presentation(text),
            };
            glyphs.push(ShapedGlyph {
                key,
                cluster: info.cluster,
                offset_26_6: [position.x_offset, position.y_offset],
                advance_26_6: [position.x_advance, position.y_advance],
            });
        }
        Ok(ShapedRun { glyphs })
    }

    /// Rasterizes a complete scene working set while pinning every requested key up front.
    ///
    /// # Errors
    /// Returns a typed font or capacity error without exposing a partial asset list.
    pub fn rasterize_working_set(
        &mut self,
        keys: &[GlyphKey],
    ) -> Result<Vec<GlyphAsset>, TextError> {
        let mut seen = HashSet::with_capacity(keys.len().min(MAX_GLYPH_BITMAPS));
        let mut unique = Vec::with_capacity(keys.len().min(MAX_GLYPH_BITMAPS));
        for key in keys {
            if key.font_generation != self.generation {
                return Err(TextError::StalePreparedState);
            }
            if seen.insert(*key) {
                unique.push(*key);
            }
            if unique.len() > MAX_GLYPH_BITMAPS {
                return Err(TextError::CapacityExceeded("glyph bitmap entries"));
            }
        }
        if let Some(pins) = self.scene_pins.as_mut() {
            pins.extend(unique.iter().copied());
        }
        let mut assets = Vec::with_capacity(unique.len());
        let mut bitmap_bytes = 0_usize;
        for key in unique {
            let bitmap = self.rasterize(key)?;
            bitmap_bytes = bitmap_bytes
                .checked_add(bitmap.pixels.len())
                .ok_or(TextError::CapacityExceeded("glyph working-set bytes"))?;
            if bitmap_bytes > MAX_GLYPH_BITMAP_BYTES {
                return Err(TextError::CapacityExceeded("glyph working-set bytes"));
            }
            assets.push(GlyphAsset { key, bitmap });
        }
        Ok(assets)
    }

    #[allow(clippy::too_many_lines)]
    fn rasterize(&mut self, key: GlyphKey) -> Result<GlyphBitmap, TextError> {
        self.cache_clock = self.cache_clock.saturating_add(1);
        if let Some(cached) = self.glyph_cache.get_mut(&key) {
            cached.last_used = self.cache_clock;
            let bitmap = cached.bitmap.clone();
            if let Some(pins) = self.scene_pins.as_mut() {
                pins.insert(key);
            }
            return Ok(bitmap);
        }
        let face = self.face(key.face)?.raw;
        let mut load = match self.raster_profile.hinting {
            HintingMode::None => ft::FT_LOAD_NO_HINTING | ft::FT_LOAD_NO_AUTOHINT,
            HintingMode::Slight => ft::FT_LOAD_DEFAULT | ft::FT_LOAD_TARGET_LIGHT,
            HintingMode::Full => ft::FT_LOAD_DEFAULT | ft::FT_LOAD_TARGET_NORMAL,
        };
        if self.request.color_glyphs {
            load |= ft::FT_LOAD_COLOR;
        }
        // SAFETY: face is UI-thread-owned and glyph slot data is copied before the next call.
        if unsafe { ft::FT_Load_Glyph(face, key.glyph_id, load) } != 0 {
            return Err(TextError::FontData(format!(
                "cannot load glyph {}",
                key.glyph_id
            )));
        }
        let slot = unsafe { (*face).glyph };
        if slot.is_null() {
            return Err(TextError::FontData(
                "FreeType returned a null glyph slot".into(),
            ));
        }
        if key.synthetic_bold {
            unsafe { ft::FT_GlyphSlot_Embolden(slot) };
        }
        if unsafe { (*slot).format != ft::FT_GLYPH_FORMAT_BITMAP }
            && unsafe { ft::FT_Render_Glyph(slot, ft::FT_RENDER_MODE_NORMAL) } != 0
        {
            return Err(TextError::FontData(format!(
                "cannot render glyph {}",
                key.glyph_id
            )));
        }
        let raw = unsafe { &(*slot).bitmap };
        let gray_mode = i8::try_from(ft::FT_PIXEL_MODE_GRAY).expect("gray mode fits i8");
        let bgra_mode = i8::try_from(ft::FT_PIXEL_MODE_BGRA).expect("BGRA mode fits i8");
        if raw.pixel_mode != gray_mode
            && raw.pixel_mode != bgra_mode
            && (raw.width != 0 || raw.rows != 0)
        {
            return Err(TextError::FontData(format!(
                "glyph bitmap has unsupported pixel mode {}",
                raw.pixel_mode
            )));
        }
        let color = raw.pixel_mode == bgra_mode;
        let mut width = usize::try_from(raw.width)
            .map_err(|_| TextError::FontData("negative glyph width".into()))?;
        let mut rows = usize::try_from(raw.rows)
            .map_err(|_| TextError::FontData("negative glyph height".into()))?;
        let max_dimension = if color { 1022 } else { 2046 };
        if width > max_dimension || rows > max_dimension {
            return Err(TextError::CapacityExceeded("glyph dimensions"));
        }
        let pitch = usize::try_from(raw.pitch.unsigned_abs())
            .map_err(|_| TextError::CapacityExceeded("glyph pitch"))?;
        let source_len = rows
            .checked_mul(pitch)
            .ok_or(TextError::CapacityExceeded("glyph bitmap"))?;
        let bytes_per_pixel = if color { 4 } else { 1 };
        let output_len = rows
            .checked_mul(width)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or(TextError::CapacityExceeded("glyph bitmap"))?;
        if output_len > MAX_GLYPH_BITMAP_BYTES {
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
        if pitch < width.saturating_mul(bytes_per_pixel) {
            return Err(TextError::FontData(
                "glyph pitch is shorter than a row".into(),
            ));
        }
        let mut pixels = vec![0_u8; output_len];
        for row in 0..rows {
            let source_row = if raw.pitch < 0 { rows - 1 - row } else { row };
            if color {
                for column in 0..width {
                    let source_offset = source_row * pitch + column * 4;
                    let target_offset = (row * width + column) * 4;
                    pixels[target_offset..target_offset + 4].copy_from_slice(
                        &bgra_to_straight_rgba(
                            source[source_offset..source_offset + 4]
                                .try_into()
                                .expect("four-byte BGRA pixel"),
                        ),
                    );
                }
            } else {
                pixels[row * width..(row + 1) * width]
                    .copy_from_slice(&source[source_row * pitch..source_row * pitch + width]);
            }
        }
        let mut bearing = [
            checked_i16(unsafe { (*slot).bitmap_left })?,
            checked_i16(unsafe { (*slot).bitmap_top })?,
        ];
        if color {
            let max_width = usize::from(self.metrics.width_px.get()).saturating_mul(2);
            let max_height = usize::from(self.metrics.height_px.get());
            if width > max_width || rows > max_height {
                let (scaled, scaled_width, scaled_rows) =
                    downscale_color_srgba(&pixels, width, rows, max_width, max_height)?;
                bearing[0] = scale_bearing(bearing[0], scaled_width, width)?;
                bearing[1] = scale_bearing(bearing[1], scaled_rows, rows)?;
                pixels = scaled;
                width = scaled_width;
                rows = scaled_rows;
            }
        }
        let bitmap = GlyphBitmap {
            format: if color {
                GlyphFormat::ColorSrgba8
            } else {
                GlyphFormat::Gray8
            },
            size_px: [
                u16::try_from(width).map_err(|_| TextError::CapacityExceeded("glyph width"))?,
                u16::try_from(rows).map_err(|_| TextError::CapacityExceeded("glyph height"))?,
            ],
            bearing_px: bearing,
            advance_26_6: i32::try_from(unsafe { (*slot).advance.x })
                .map_err(|_| TextError::FontData("glyph advance overflow".into()))?,
            pixels: Arc::from(pixels),
        };
        let bitmap_len = bitmap.pixels.len();
        self.make_cache_room(bitmap_len)?;
        self.glyph_cache_bytes = self
            .glyph_cache_bytes
            .checked_add(bitmap_len)
            .ok_or(TextError::CapacityExceeded("glyph bitmap cache bytes"))?;
        self.glyph_cache.insert(
            key,
            CachedGlyph {
                bitmap: bitmap.clone(),
                last_used: self.cache_clock,
            },
        );
        if let Some(pins) = self.scene_pins.as_mut() {
            pins.insert(key);
        }
        Ok(bitmap)
    }

    fn make_cache_room(&mut self, incoming_bytes: usize) -> Result<(), TextError> {
        while self.glyph_cache.len() >= MAX_GLYPH_BITMAPS
            || self
                .glyph_cache_bytes
                .checked_add(incoming_bytes)
                .is_none_or(|bytes| bytes > MAX_GLYPH_BITMAP_BYTES)
        {
            let oldest = self
                .glyph_cache
                .iter()
                .filter(|(key, _)| {
                    !self
                        .scene_pins
                        .as_ref()
                        .is_some_and(|pins| pins.contains(key))
                })
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(key, _)| *key)
                .ok_or(TextError::CapacityExceeded("glyph bitmap cache bytes"))?;
            let removed = self
                .glyph_cache
                .remove(&oldest)
                .ok_or(TextError::CapacityExceeded("glyph bitmap cache entries"))?;
            self.glyph_cache_bytes = self
                .glyph_cache_bytes
                .checked_sub(removed.bitmap.pixels.len())
                .ok_or(TextError::CapacityExceeded("glyph bitmap cache accounting"))?;
            self.cache_evictions = self.cache_evictions.saturating_add(1);
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
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
        let matched = if let Some(path) = &self.request.face_path_override {
            FontMatch {
                path: Arc::clone(path),
                index: 0,
                family: Arc::from(self.request.family.as_ref()),
                profile: requested_profile(&self.request),
            }
        } else {
            font_match(self.fontconfig.0, &self.request, cluster, style)?
        };
        if self.faces.is_empty() {
            self.raster_profile = matched.profile;
            self.resolved_primary = Some(ResolvedFace {
                path: Arc::clone(&matched.path),
                index: matched.index,
                family: Arc::clone(&matched.family),
                style,
            });
        }
        let path = matched.path;
        let index = matched.index;
        if let Some(id) = self.face_lookup.get(&(path.clone(), index)) {
            return Ok(*id);
        }
        if self.faces.len() >= MAX_FACES {
            tracing::debug!(
                category = "capacity_pressure",
                operation = "font_face_fallback",
                limit = MAX_FACES,
                "font face capacity reached; using primary face"
            );
            return self
                .faces
                .first()
                .map(|face| face.id)
                .ok_or(TextError::CapacityExceeded("font faces"));
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
        let requested_size = self.request.physical_size_26_6()?;
        let scalable = unsafe { (*raw_face).face_flags & ft::FT_FACE_FLAG_SCALABLE != 0 };
        let size = if scalable {
            requested_size
        } else {
            let count = usize::try_from(unsafe { (*raw_face).num_fixed_sizes })
                .map_err(|_| TextError::FontData("invalid bitmap strike count".into()))?;
            let strikes = if count == 0 || unsafe { (*raw_face).available_sizes.is_null() } {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts((*raw_face).available_sizes, count) }
            };
            let target = requested_size;
            let selected = strikes
                .iter()
                .enumerate()
                .filter(|(_, strike)| strike.y_ppem >= target)
                .min_by_key(|(_, strike)| strike.y_ppem)
                .or_else(|| {
                    strikes
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, strike)| strike.y_ppem)
                })
                .map(|(index, strike)| (index, strike.y_ppem))
                .ok_or_else(|| TextError::FontData("bitmap face has no usable strike".into()))?;
            if unsafe {
                ft::FT_Select_Size(raw_face, i32::try_from(selected.0).unwrap_or(i32::MAX))
            } != 0
            {
                unsafe { ft::FT_Done_Face(raw_face) };
                return Err(TextError::FontData(
                    "cannot select bitmap font strike".into(),
                ));
            }
            selected.1
        };
        if scalable && unsafe { ft::FT_Set_Char_Size(raw_face, 0, size, 72, 72) } != 0 {
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
        let ft_size = unsafe { (*raw_face).size };
        if ft_size.is_null() {
            unsafe {
                hb::hb_font_destroy(hb_font);
                hb::hb_face_destroy(hb_face);
                hb::hb_blob_destroy(hb_blob);
                ft::FT_Done_Face(raw_face);
            }
            return Err(TextError::FontData("face has no selected size".into()));
        }
        let size_metrics = unsafe { (*ft_size).metrics };
        let x_ppem = size_metrics.x_ppem;
        let y_ppem = size_metrics.y_ppem;
        let x_scale = i32::from(x_ppem).saturating_mul(64);
        let y_scale = i32::from(y_ppem).saturating_mul(64);
        unsafe {
            hb::hb_ot_font_set_funcs(hb_font);
            hb::hb_font_set_scale(hb_font, x_scale, y_scale);
            hb::hb_font_set_ppem(hb_font, u32::from(x_ppem), u32::from(y_ppem));
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

fn bgra_to_straight_rgba([blue, green, red, alpha]: [u8; 4]) -> [u8; 4] {
    let unpremultiply = |component: u8| -> u8 {
        if alpha == 0 {
            0
        } else {
            ((u32::from(component) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
        }
    };
    [
        unpremultiply(red),
        unpremultiply(green),
        unpremultiply(blue),
        alpha,
    ]
}

fn scale_bearing(value: i16, destination: usize, source: usize) -> Result<i16, TextError> {
    if source == 0 {
        return Ok(value);
    }
    let numerator = i64::from(value)
        .checked_mul(
            i64::try_from(destination).map_err(|_| TextError::CapacityExceeded("bearing"))?,
        )
        .ok_or(TextError::CapacityExceeded("bearing"))?;
    let source = i64::try_from(source).map_err(|_| TextError::CapacityExceeded("bearing"))?;
    let rounded = if numerator >= 0 {
        numerator.saturating_add(source / 2) / source
    } else {
        numerator.saturating_sub(source / 2) / source
    };
    i16::try_from(rounded).map_err(|_| TextError::CapacityExceeded("bearing"))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn downscale_color_srgba(
    source: &[u8],
    width: usize,
    height: usize,
    max_width: usize,
    max_height: usize,
) -> Result<(Vec<u8>, usize, usize), TextError> {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        return Ok((Vec::new(), 0, 0));
    }
    let (out_width, out_height) =
        if width.saturating_mul(max_height) > height.saturating_mul(max_width) {
            (
                max_width,
                height.saturating_mul(max_width).div_ceil(width).max(1),
            )
        } else {
            (
                width.saturating_mul(max_height).div_ceil(height).max(1),
                max_height,
            )
        };
    let output_len = out_width
        .checked_mul(out_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(TextError::CapacityExceeded("color glyph scaling"))?;
    if source
        .len()
        .checked_add(output_len)
        .is_none_or(|bytes| bytes > MAX_GLYPH_BITMAP_BYTES)
    {
        return Err(TextError::CapacityExceeded("color glyph scaling"));
    }
    let mut output = vec![0_u8; output_len];
    for out_y in 0..out_height {
        let y0 = out_y * height;
        let y1 = (out_y + 1) * height;
        for out_x in 0..out_width {
            let x0 = out_x * width;
            let x1 = (out_x + 1) * width;
            let mut premultiplied = [0.0_f64; 3];
            let mut alpha_sum = 0.0_f64;
            let mut weight_sum = 0.0_f64;
            for source_y in y0 / out_height..y1.div_ceil(out_height) {
                let overlap_y = y1
                    .min((source_y + 1) * out_height)
                    .saturating_sub(y0.max(source_y * out_height));
                for source_x in x0 / out_width..x1.div_ceil(out_width) {
                    let overlap_x = x1
                        .min((source_x + 1) * out_width)
                        .saturating_sub(x0.max(source_x * out_width));
                    let weight = (overlap_x * overlap_y) as f64;
                    let offset = (source_y * width + source_x) * 4;
                    let alpha = f64::from(source[offset + 3]) / 255.0;
                    for channel in 0..3 {
                        premultiplied[channel] +=
                            srgb_decode(source[offset + channel]) * alpha * weight;
                    }
                    alpha_sum += alpha * weight;
                    weight_sum += weight;
                }
            }
            let target = (out_y * out_width + out_x) * 4;
            let alpha = if weight_sum == 0.0 {
                0.0
            } else {
                alpha_sum / weight_sum
            };
            if alpha > 0.0 {
                for channel in 0..3 {
                    output[target + channel] =
                        srgb_encode(premultiplied[channel] / weight_sum / alpha);
                }
            }
            output[target + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    Ok((output, out_width, out_height))
}

fn srgb_decode(byte: u8) -> f64 {
    let value = f64::from(byte) / 255.0;
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn srgb_encode(value: f64) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn face_supports(face: ft::FT_Face, text: &str) -> bool {
    text.chars().all(|ch| {
        is_fallback_ignorable(ch)
            || unsafe { ft::FT_Get_Char_Index(face, u64::from(u32::from(ch))) } != 0
    })
}

fn infer_presentation(text: &str) -> crate::GlyphPresentation {
    if text.contains('\u{fe0e}') {
        return crate::GlyphPresentation::Text;
    }
    if text.chars().any(|ch| {
        matches!(
            ch,
            '\u{fe0f}'
                | '\u{1f1e6}'..='\u{1f1ff}'
                | '\u{1f300}'..='\u{1faff}'
                | '\u{2600}'..='\u{27bf}'
        )
    }) {
        crate::GlyphPresentation::Emoji
    } else {
        crate::GlyphPresentation::Text
    }
}

const fn is_fallback_ignorable(ch: char) -> bool {
    matches!(
        ch,
        '\u{200c}'
            | '\u{200d}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{e0100}'..='\u{e01ef}'
            | '\u{e0020}'..='\u{e007f}'
    )
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

#[allow(clippy::too_many_lines)]
fn font_match(
    fc: &'static fc::Fc,
    request: &FontRequest,
    cluster: Option<&str>,
    style: FontStyle,
) -> Result<FontMatch, TextError> {
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
        if request.monospace {
            (fc.FcPatternAddInteger)(
                pattern.1,
                fc::constants::FC_SPACING.as_ptr(),
                fc::constants::FC_MONO,
            );
        }
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
            if !is_fallback_ignorable(ch) {
                unsafe { (fc.FcCharSetAddChar)(charset.1, ch as u32) };
            }
        }
        if unsafe {
            (fc.FcPatternAddCharSet)(pattern.1, fc::constants::FC_CHARSET.as_ptr(), charset.1)
        } == 0
        {
            return Err(TextError::Environment("Fontconfig rejected charset".into()));
        }
        if text.contains('\u{fe0e}') || infer_presentation(text) == crate::GlyphPresentation::Emoji
        {
            unsafe {
                (fc.FcPatternAddBool)(
                    pattern.1,
                    fc::constants::FC_COLOR.as_ptr(),
                    i32::from(request.color_glyphs && !text.contains('\u{fe0e}')),
                )
            };
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
    let family = pattern_string(fc, matched.1, fc::constants::FC_FAMILY)
        .unwrap_or_else(|| request.family.to_string());
    let profile = matched_profile(fc, matched.1, request);
    Ok(FontMatch {
        path: Arc::from(path),
        index,
        family: Arc::from(family),
        profile,
    })
}

struct FontMatch {
    path: Arc<str>,
    index: u32,
    family: Arc<str>,
    profile: RasterProfile,
}

fn requested_profile(request: &FontRequest) -> RasterProfile {
    RasterProfile {
        hinting: match request.hinting {
            HintingPreference::None => HintingMode::None,
            HintingPreference::Full => HintingMode::Full,
            HintingPreference::Slight | HintingPreference::System => HintingMode::Slight,
        },
        antialias: AntialiasMode::Gray,
    }
}

fn matched_profile(
    fc: &'static fc::Fc,
    pattern: *mut fc::FcPattern,
    request: &FontRequest,
) -> RasterProfile {
    let mut profile = requested_profile(request);
    if request.hinting == HintingPreference::System {
        let enabled = pattern_bool(fc, pattern, fc::constants::FC_HINTING).unwrap_or(true);
        profile.hinting = if enabled {
            match pattern_integer(fc, pattern, fc::constants::FC_HINT_STYLE) {
                Some(fc::constants::FC_HINT_NONE) => HintingMode::None,
                Some(fc::constants::FC_HINT_FULL | fc::constants::FC_HINT_MEDIUM) => {
                    HintingMode::Full
                }
                _ => HintingMode::Slight,
            }
        } else {
            HintingMode::None
        };
    }
    if request.antialiasing == AntialiasPreference::System
        && !pattern_bool(fc, pattern, fc::constants::FC_ANTIALIAS).unwrap_or(true)
    {
        tracing::warn!(
            category = "text_profile",
            reason = "system-antialias-disabled-overridden",
            "monochrome rendering is unsupported; using grayscale antialiasing"
        );
    }
    if request.antialiasing == AntialiasPreference::System
        && matches!(
            pattern_integer(fc, pattern, fc::constants::FC_RGBA),
            Some(fc::constants::FC_RGBA_RGB | fc::constants::FC_RGBA_BGR)
        )
    {
        tracing::info!(
            category = "text_profile",
            reason = "lcd-not-enabled-grayscale-fallback",
            "Fontconfig prefers subpixel antialiasing; using the safe grayscale path"
        );
    }
    profile
}

fn pattern_integer(fc: &'static fc::Fc, pattern: *mut fc::FcPattern, key: &CStr) -> Option<c_int> {
    let mut value = 0;
    (unsafe { (fc.FcPatternGetInteger)(pattern, key.as_ptr(), 0, &raw mut value) }
        == fc::FcResultMatch)
        .then_some(value)
}

fn pattern_bool(fc: &'static fc::Fc, pattern: *mut fc::FcPattern, key: &CStr) -> Option<bool> {
    let mut value = 0;
    (unsafe { (fc.FcPatternGetBool)(pattern, key.as_ptr(), 0, &raw mut value) }
        == fc::FcResultMatch)
        .then_some(value != 0)
}

fn pattern_string(fc: &'static fc::Fc, pattern: *mut fc::FcPattern, key: &CStr) -> Option<String> {
    let mut value = ptr::null_mut();
    if unsafe { (fc.FcPatternGetString)(pattern, key.as_ptr(), 0, &raw mut value) }
        != fc::FcResultMatch
        || value.is_null()
    {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(value.cast()) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn metrics(face: ft::FT_Face) -> Result<CellMetrics, TextError> {
    let size = unsafe { (*face).size };
    if size.is_null() {
        return Err(TextError::FontData("face has no active size".into()));
    }
    let value = unsafe { (*size).metrics };
    let width = ceil_26_6(value.max_advance)?.max(1);
    let baseline = ceil_26_6(value.ascender)?;
    let height = cell_height_26_6(value.height, value.ascender, value.descender)?;
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

fn cell_height_26_6(height: i64, ascender: i64, descender: i64) -> Result<i64, TextError> {
    let line_height = ceil_26_6(height)?.max(1);
    let glyph_extent = ceil_26_6(ascender)?
        .checked_sub(descender.div_euclid(64))
        .ok_or(TextError::CapacityExceeded("font metrics"))?;
    Ok(line_height.max(glyph_extent).max(1))
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
    fn freetype_bgra_is_unpremultiplied_and_swizzled() {
        assert_eq!(bgra_to_straight_rgba([0, 0, 0, 0]), [0, 0, 0, 0]);
        assert_eq!(bgra_to_straight_rgba([10, 20, 30, 255]), [30, 20, 10, 255]);
        assert_eq!(
            bgra_to_straight_rgba([25, 50, 100, 128]),
            [199, 100, 50, 128]
        );
    }

    #[test]
    fn color_downscale_filters_in_linear_premultiplied_space() {
        let source = [255, 0, 0, 255, 0, 0, 0, 0];
        let (scaled, width, height) = downscale_color_srgba(&source, 2, 1, 1, 1).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(&scaled[..3], &[255, 0, 0]);
        assert_eq!(scaled[3], 128);
    }

    #[test]
    fn locked_cbdt_fixture_rasterizes_as_bounded_straight_srgba() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/fonts/noto-color-emoji/NotoColorEmoji.ttf");
        let request = FontRequest::from_points("Noto Color Emoji", 13.0, 120, false)
            .unwrap()
            .with_monospace(false)
            .with_face_path_override(fixture.to_string_lossy().into_owned());
        let mut system = TextSystem::new(request).unwrap();
        let shaped = system
            .shape_cluster("\u{1f600}\u{fe0f}", FontStyle::Regular)
            .unwrap();
        assert!(!shaped.glyphs.is_empty());
        assert!(shaped.assets.iter().any(|asset| {
            asset.bitmap.format == GlyphFormat::ColorSrgba8
                && asset.bitmap.pixels.len()
                    == usize::from(asset.bitmap.size_px[0])
                        * usize::from(asset.bitmap.size_px[1])
                        * 4
        }));
        assert!(shaped.assets.iter().all(|asset| {
            asset.bitmap.size_px[0] <= system.metrics().width_px.get().saturating_mul(2)
                && asset.bitmap.size_px[1] <= system.metrics().height_px.get()
        }));
        for cluster in [
            "\u{1f44d}\u{1f3fd}",
            "\u{1f1fa}\u{1f1f8}",
            "1\u{fe0f}\u{20e3}",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
        ] {
            let shaped = system.shape_cluster(cluster, FontStyle::Regular).unwrap();
            assert!(!shaped.glyphs.is_empty());
            assert!(shaped.glyphs.iter().all(|glyph| glyph.key.glyph_id != 0));
            assert!(
                shaped
                    .assets
                    .iter()
                    .any(|asset| asset.bitmap.format == GlyphFormat::ColorSrgba8)
            );
        }
        let mut text_system =
            TextSystem::new(FontRequest::from_points("monospace", 13.0, 120, false).unwrap())
                .unwrap();
        let text_presentation = text_system
            .shape_cluster("\u{2600}\u{fe0e}", FontStyle::Regular)
            .unwrap();
        assert!(
            text_presentation
                .assets
                .iter()
                .all(|asset| asset.bitmap.format == GlyphFormat::Gray8)
        );
    }

    #[test]
    fn cell_height_includes_the_complete_descender_pixel_extent() {
        assert_eq!(cell_height_26_6(17 * 64, 14 * 64, -4 * 64).unwrap(), 18);
    }

    #[test]
    fn underscore_bitmap_fits_inside_the_default_cell() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();
        let baseline = i32::from(system.metrics().baseline_px);
        let cell_height = i32::from(system.metrics().height_px.get());
        let shaped = system.shape_cluster("_", FontStyle::Regular).unwrap();

        for asset in shaped.assets {
            let top = baseline - i32::from(asset.bitmap.bearing_px[1]);
            let bottom = top + i32::from(asset.bitmap.size_px[1]);
            assert!(bottom <= cell_height, "underscore extends below its cell");
        }
    }

    fn cached_glyph(id: u32, bytes: usize, last_used: u64) -> (GlyphKey, CachedGlyph) {
        let key = GlyphKey {
            font_generation: 1,
            face: FaceId(0),
            glyph_id: id,
            synthetic_bold: false,
            synthetic_italic: false,
            presentation: crate::GlyphPresentation::Text,
        };
        let bitmap = GlyphBitmap {
            format: GlyphFormat::Gray8,
            size_px: [0, 0],
            bearing_px: [0, 0],
            advance_26_6: 0,
            pixels: Arc::from(vec![0; bytes]),
        };
        (key, CachedGlyph { bitmap, last_used })
    }

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

    #[test]
    fn shaping_phase_does_not_populate_the_bitmap_cache() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();

        let shaped = system
            .shape_cluster_only("two phase", FontStyle::Regular)
            .unwrap();
        assert!(!shaped.glyphs.is_empty());
        assert_eq!(system.cache_stats().entries, 0);

        system.begin_scene();
        let keys = shaped
            .glyphs
            .iter()
            .map(|glyph| glyph.key)
            .collect::<Vec<_>>();
        let assets = system.rasterize_working_set(&keys).unwrap();
        assert_eq!(assets.len(), system.cache_stats().pinned);
        assert_eq!(assets.len(), system.cache_stats().entries);
        system.end_scene();
    }

    #[test]
    fn working_set_rejects_stale_generation_before_rasterizing() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();
        let mut key = system
            .shape_cluster_only("x", FontStyle::Regular)
            .unwrap()
            .glyphs[0]
            .key;
        key.font_generation = key.font_generation.saturating_add(1);

        assert!(matches!(
            system.rasterize_working_set(&[key]),
            Err(TextError::StalePreparedState)
        ));
        assert_eq!(system.cache_stats().entries, 0);
    }

    #[test]
    fn cache_pressure_evicts_the_oldest_entry_and_preserves_accounting() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();
        let (old_key, old) = cached_glyph(1, 1, 1);
        let (new_key, new) = cached_glyph(2, 1, 2);
        system.glyph_cache.insert(old_key, old);
        system.glyph_cache.insert(new_key, new);
        system.glyph_cache_bytes = 2;

        system.make_cache_room(MAX_GLYPH_BITMAP_BYTES - 1).unwrap();

        assert!(!system.glyph_cache.contains_key(&old_key));
        assert!(system.glyph_cache.contains_key(&new_key));
        assert_eq!(system.glyph_cache_bytes, 1);
        assert_eq!(system.cache_evictions, 1);
    }

    #[test]
    fn scene_pins_exclude_the_current_working_set_from_eviction() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();
        let (pinned_key, pinned) = cached_glyph(1, 1, 1);
        let (other_key, other) = cached_glyph(2, 1, 2);
        system.glyph_cache.insert(pinned_key, pinned);
        system.glyph_cache.insert(other_key, other);
        system.glyph_cache_bytes = 2;
        system.scene_pins = Some(HashSet::from([pinned_key]));

        system.make_cache_room(MAX_GLYPH_BITMAP_BYTES - 1).unwrap();
        assert!(system.glyph_cache.contains_key(&pinned_key));
        assert!(!system.glyph_cache.contains_key(&other_key));

        assert!(matches!(
            system.make_cache_room(MAX_GLYPH_BITMAP_BYTES),
            Err(TextError::CapacityExceeded("glyph bitmap cache bytes"))
        ));
        assert!(system.glyph_cache.contains_key(&pinned_key));
    }
}
