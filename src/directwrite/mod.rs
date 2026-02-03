//! Rasterization powered by DirectWrite.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::ptr;
use std::sync::OnceLock;

use log::info;
use log::debug;
use dwrote::{
    FontCollection, FontFace, FontFallback, FontStretch, FontStyle, FontWeight, GlyphOffset,
    GlyphRunAnalysis, TextAnalysisSource, TextAnalysisSourceMethods, DWRITE_GLYPH_RUN,
};

use winapi::shared::ntdef::{HRESULT, LOCALE_NAME_MAX_LENGTH};
use winapi::shared::winerror::S_OK;
use winapi::um::dwrite;
use winapi::um::dwrite::{IDWriteFactory, IDWriteGlyphRunAnalysis, DWRITE_FACTORY_TYPE_SHARED};
use winapi::um::dwrite_1::{DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE, DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE};
use winapi::um::dwrite_2::{DWRITE_GRID_FIT_MODE_DISABLED, DWRITE_GRID_FIT_MODE_ENABLED};
use winapi::um::dwrite_3::{IDWriteFactory3, DWRITE_RENDERING_MODE1_ALIASED, DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC};
use winapi::um::unknwnbase::IUnknown;
use winapi::um::winnls::GetUserDefaultLocaleName;
use winapi::Interface;
use wio::com::ComPtr;

use super::{
    BitmapBuffer, Error, FontDesc, FontKey, GlyphKey, Metrics, RasterizedGlyph, Size, Slant, Style,
    Weight,
};

/// Get or create the IDWriteFactory3 interface for DWrite3 API access.
fn get_dwrite3_factory() -> Option<*mut IDWriteFactory3> {
    static FACTORY3: OnceLock<usize> = OnceLock::new();

    let ptr = *FACTORY3.get_or_init(|| unsafe {
        // Create a DWrite factory and QueryInterface to IDWriteFactory3.
        let mut factory: *mut IDWriteFactory = ptr::null_mut();
        let hr = winapi::um::dwrite::DWriteCreateFactory(
            DWRITE_FACTORY_TYPE_SHARED,
            &IDWriteFactory::uuidof(),
            &mut factory as *mut *mut IDWriteFactory as *mut *mut IUnknown,
        );
        if hr != S_OK || factory.is_null() {
            return 0;
        }

        let mut factory3: *mut IDWriteFactory3 = ptr::null_mut();
        let hr = (*(factory as *mut IUnknown)).QueryInterface(
            &IDWriteFactory3::uuidof(),
            &mut factory3 as *mut *mut IDWriteFactory3 as *mut *mut std::ffi::c_void,
        );
        // Release the original factory reference (QueryInterface adds a ref).
        (*(factory as *mut IUnknown)).Release();

        if hr != S_OK || factory3.is_null() {
            return 0;
        }

        factory3 as usize
    });

    if ptr == 0 { None } else { Some(ptr as *mut IDWriteFactory3) }
}

/// DirectWrite uses 0 for missing glyph symbols.
/// https://docs.microsoft.com/en-us/typography/opentype/spec/recom#glyph-0-the-notdef-glyph
const MISSING_GLYPH_INDEX: u16 = 0;

/// Cached DirectWrite font.
struct Font {
    face: FontFace,
    family_name: String,
    weight: FontWeight,
    style: FontStyle,
    stretch: FontStretch,
}

pub struct DirectWriteRasterizer {
    fonts: HashMap<FontKey, Font>,
    keys: HashMap<FontDesc, FontKey>,
    available_fonts: FontCollection,
    fallback_sequence: Option<FontFallback>,
    rendering_mode: super::RenderingMode,
    grid_fitting: bool,
}

impl DirectWriteRasterizer {
    fn rasterize_glyph(
        &self,
        face: &FontFace,
        size: Size,
        character: char,
        glyph_index: u16,
    ) -> Result<RasterizedGlyph, Error> {
        let em_size = size.as_px();

        let glyph_run = DWRITE_GLYPH_RUN {
            fontFace: unsafe { face.as_ptr() },
            fontEmSize: em_size,
            glyphCount: 1,
            glyphIndices: &glyph_index,
            glyphAdvances: &0.0,
            glyphOffsets: &GlyphOffset::default(),
            isSideways: 0,
            bidiLevel: 0,
        };

        let (rendering_mode1, measuring_mode, antialias_mode) = match self.rendering_mode {
            super::RenderingMode::Aliased => (
                DWRITE_RENDERING_MODE1_ALIASED,
                dwrote::DWRITE_MEASURING_MODE_GDI_CLASSIC,
                DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            ),
            super::RenderingMode::Grayscale => (
                DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
                dwrote::DWRITE_MEASURING_MODE_NATURAL,
                DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            ),
            super::RenderingMode::Subpixel => (
                DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
                dwrote::DWRITE_MEASURING_MODE_NATURAL,
                DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE,
            ),
        };

        let grid_fit_mode = if self.grid_fitting {
            DWRITE_GRID_FIT_MODE_ENABLED
        } else {
            DWRITE_GRID_FIT_MODE_DISABLED
        };

        let factory3 = get_dwrite3_factory()
            .ok_or_else(|| Error::PlatformError("IDWriteFactory3 not available".into()))?;

        let glyph_analysis = unsafe {
            let mut native: *mut IDWriteGlyphRunAnalysis = ptr::null_mut();
            let hr = (*factory3).CreateGlyphRunAnalysis(
                &glyph_run as *const DWRITE_GLYPH_RUN,
                ptr::null(),
                rendering_mode1,
                measuring_mode,
                grid_fit_mode,
                antialias_mode,
                0.0,
                0.0,
                &mut native,
            );
            if hr != S_OK || native.is_null() {
                info!("DWrite3 CreateGlyphRunAnalysis failed: hr={:X}", hr);
                return Err(Error::from(hr));
            }
            GlyphRunAnalysis::take(ComPtr::from_raw(native))
        };

        let texture_type = match self.rendering_mode {
            super::RenderingMode::Subpixel => dwrote::DWRITE_TEXTURE_CLEARTYPE_3x1,
            _ => dwrote::DWRITE_TEXTURE_ALIASED_1x1,
        };

        let bounds = glyph_analysis.get_alpha_texture_bounds(texture_type)?;

        let raw_buffer = glyph_analysis.create_alpha_texture(texture_type, bounds)?;

        let buffer = match self.rendering_mode {
            super::RenderingMode::Subpixel => {
                // ClearType 3x1: raw RGB subpixel data, pass through directly.
                BitmapBuffer::Rgb(raw_buffer)
            },
            _ => {
                // Aliased and Grayscale both use ALIASED_1x1: single-channel alpha.
                // Expand to RGB for the glyph atlas.
                let mut rgb = Vec::with_capacity(raw_buffer.len() * 3);
                for &alpha in &raw_buffer {
                    rgb.push(alpha);
                    rgb.push(alpha);
                    rgb.push(alpha);
                }
                BitmapBuffer::Rgb(rgb)
            },
        };

        Ok(RasterizedGlyph {
            character,
            width: bounds.right - bounds.left,
            height: bounds.bottom - bounds.top,
            top: -bounds.top,
            left: bounds.left,
            advance: (0, 0),
            buffer,
        })
    }

    fn get_loaded_font(&self, font_key: FontKey) -> Result<&Font, Error> {
        self.fonts.get(&font_key).ok_or(Error::UnknownFontKey)
    }

    fn get_glyph_index(&self, face: &FontFace, character: char) -> u16 {
        face.glyph_indices(&[character as u32])
            .ok()
            .and_then(|v| v.first().copied())
            .unwrap_or(MISSING_GLYPH_INDEX)
    }

    fn get_fallback_font(&self, loaded_font: &Font, character: char) -> Option<dwrote::Font> {
        let fallback = self.fallback_sequence.as_ref()?;

        let mut buffer = [0u16; 2];
        character.encode_utf16(&mut buffer);

        let length = character.len_utf16() as u32;
        let utf16_codepoints = &buffer[..length as usize];

        let locale = get_current_locale();

        let text_analysis_source_data = TextAnalysisSourceData { locale: &locale, length };
        let text_analysis_source = TextAnalysisSource::from_text(
            Box::new(text_analysis_source_data),
            Cow::Borrowed(utf16_codepoints),
        );

        let fallback_result = fallback.map_characters(
            &text_analysis_source,
            0,
            length,
            &self.available_fonts,
            Some(&loaded_font.family_name),
            loaded_font.weight,
            loaded_font.style,
            loaded_font.stretch,
        );

        fallback_result.mapped_font
    }
}

impl crate::Rasterize for DirectWriteRasterizer {
    fn new() -> Result<DirectWriteRasterizer, Error> {
        Ok(DirectWriteRasterizer {
            fonts: HashMap::new(),
            keys: HashMap::new(),
            available_fonts: FontCollection::system(),
            fallback_sequence: FontFallback::get_system_fallback(),
            rendering_mode: Default::default(),
            grid_fitting: false,
        })
    }

    fn set_rendering_mode(&mut self, mode: super::RenderingMode) {
        self.rendering_mode = mode;
    }

    fn set_grid_fitting(&mut self, enabled: bool) {
        self.grid_fitting = enabled;
    }

    fn metrics(&self, key: FontKey, size: Size) -> Result<Metrics, Error> {
        let face = &self.get_loaded_font(key)?.face;
        let vmetrics = face.metrics().metrics0();

        let scale = f64::from(size.as_px()) / f64::from(vmetrics.designUnitsPerEm);

        let underline_position = f64::from(vmetrics.underlinePosition) * scale;
        let underline_thickness = f64::from(vmetrics.underlineThickness) * scale;

        let strikeout_position = f64::from(vmetrics.strikethroughPosition) * scale;
        let strikeout_thickness = f64::from(vmetrics.strikethroughThickness) * scale;

        let ascent = f64::from(vmetrics.ascent) * scale;
        let descent = -f64::from(vmetrics.descent) * scale;
        let line_gap = f64::from(vmetrics.lineGap) * scale;

        let line_height = ascent - descent + line_gap;

        // Since all monospace characters have the same width, we use `!` for horizontal metrics.
        let character = '!';
        let glyph_index = self.get_glyph_index(face, character);

        let glyph_metrics = face.design_glyph_metrics(&[glyph_index], false)
            .map_err(|_| Error::MetricsNotFound)?;
        let hmetrics = glyph_metrics.first().ok_or(Error::MetricsNotFound)?;

        let average_advance = f64::from(hmetrics.advanceWidth) * scale;

        debug!(
            "crossfont metrics: designUnitsPerEm={}, size_px={}, scale={}, advanceWidth={}, \
             average_advance={}, ascent={}, descent={}, line_gap={}, line_height={}",
            vmetrics.designUnitsPerEm,
            size.as_px(),
            scale,
            hmetrics.advanceWidth,
            average_advance,
            ascent,
            descent,
            line_gap,
            line_height,
        );

        Ok(Metrics {
            descent: descent as f32,
            average_advance,
            line_height,
            underline_position: underline_position as f32,
            underline_thickness: underline_thickness as f32,
            strikeout_position: strikeout_position as f32,
            strikeout_thickness: strikeout_thickness as f32,
        })
    }

    fn load_font(&mut self, desc: &FontDesc, _size: Size) -> Result<FontKey, Error> {
        // Fast path if face is already loaded.
        if let Some(key) = self.keys.get(desc) {
            return Ok(*key);
        }

        let family = self
            .available_fonts
            .font_family_by_name(&desc.name)
            .ok()
            .flatten()
            .ok_or_else(|| Error::FontNotFound(desc.clone()))?;

        let font = match desc.style {
            Style::Description { weight, slant } => {
                // This searches for the "best" font - should mean we don't have to worry about
                // fallbacks if our exact desired weight/style isn't available.
                family
                    .first_matching_font(weight.into(), FontStretch::Normal, slant.into())
                    .map_err(|_| Error::FontNotFound(desc.clone()))
            },
            Style::Specific(ref style) => {
                let count = family.get_font_count();

                (0..count)
                    .find_map(|idx| {
                        family.font(idx).ok().filter(|f| f.face_name() == *style)
                    })
                    .ok_or_else(|| Error::FontNotFound(desc.clone()))
            },
        }?;

        let key = FontKey::next();
        self.keys.insert(desc.clone(), key);
        self.fonts.insert(key, font.into());

        Ok(key)
    }

    fn get_glyph(&mut self, glyph: GlyphKey) -> Result<RasterizedGlyph, Error> {
        let loaded_font = self.get_loaded_font(glyph.font_key)?;

        let loaded_fallback_font;
        let mut font = loaded_font;
        let mut glyph_index = self.get_glyph_index(&loaded_font.face, glyph.character);
        if glyph_index == MISSING_GLYPH_INDEX {
            if let Some(fallback_font) = self.get_fallback_font(loaded_font, glyph.character) {
                loaded_fallback_font = Font::from(fallback_font);
                glyph_index = self.get_glyph_index(&loaded_fallback_font.face, glyph.character);
                font = &loaded_fallback_font;
            }
        }

        let rasterized_glyph =
            self.rasterize_glyph(&font.face, glyph.size, glyph.character, glyph_index)?;

        if glyph_index == MISSING_GLYPH_INDEX {
            Err(Error::MissingGlyph(rasterized_glyph))
        } else {
            Ok(rasterized_glyph)
        }
    }

    fn kerning(&mut self, _left: GlyphKey, _right: GlyphKey) -> (f32, f32) {
        (0., 0.)
    }
}

impl From<dwrote::Font> for Font {
    fn from(font: dwrote::Font) -> Font {
        Font {
            face: font.create_font_face(),
            family_name: font.family_name(),
            weight: font.weight(),
            style: font.style(),
            stretch: font.stretch(),
        }
    }
}

impl From<Weight> for FontWeight {
    fn from(weight: Weight) -> FontWeight {
        match weight {
            Weight::Bold => FontWeight::Bold,
            Weight::Normal => FontWeight::Regular,
        }
    }
}

impl From<Slant> for FontStyle {
    fn from(slant: Slant) -> FontStyle {
        match slant {
            Slant::Oblique => FontStyle::Oblique,
            Slant::Italic => FontStyle::Italic,
            Slant::Normal => FontStyle::Normal,
        }
    }
}

fn get_current_locale() -> String {
    let mut buffer = vec![0u16; LOCALE_NAME_MAX_LENGTH];
    let len =
        unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) as usize };

    // `len` includes null byte, which we don't need in Rust.
    OsString::from_wide(&buffer[..len - 1]).into_string().expect("Locale not valid unicode")
}

/// Font fallback information for dwrote's TextAnalysisSource.
struct TextAnalysisSourceData<'a> {
    locale: &'a str,
    length: u32,
}

impl TextAnalysisSourceMethods for TextAnalysisSourceData<'_> {
    fn get_locale_name(&self, _text_position: u32) -> (Cow<'_, str>, u32) {
        (Cow::Borrowed(self.locale), self.length)
    }

    fn get_paragraph_reading_direction(&self) -> dwrite::DWRITE_READING_DIRECTION {
        dwrite::DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
    }
}

impl From<HRESULT> for Error {
    fn from(hresult: HRESULT) -> Self {
        let message = format!("a DirectWrite rendering error occurred: {:X}", hresult);
        Error::PlatformError(message)
    }
}
