use std::collections::HashMap;

use ecow::eco_format;
use indexmap::IndexMap;
use pdf_writer::Filter;
use pdf_writer::{types::UnicodeCmap, Finish, Name, Rect, Ref};
use ttf_parser::name_id;

use typst::layout::Em;
use typst::text::{color::frame_for_glyph, Font};

use crate::resources::ResourcesRefs;
use crate::{
    content,
    font::{subset_tag, write_font_descriptor, CMAP_NAME, SYSTEM_INFO},
    EmExt, PdfChunk,
};
use crate::{AllocRefs, Resources};

pub fn write_color_fonts(
    context: &AllocRefs,
) -> (PdfChunk, HashMap<ColorFontSlice, Ref>) {
    let mut out = HashMap::new();
    let mut chunk = PdfChunk::new();
    context.resources.traverse(&mut |resources: &Resources| {
        let Some(color_fonts) = &resources.color_fonts else {
            return;
        };

        for (font, color_font) in &color_fonts.map {
            // For each Type3 font that is part of this family…
            for font_index in 0..(color_font.glyphs.len() / 256) + 1 {
                let font_slice =
                    ColorFontSlice { font: font.clone(), subfont: font_index };
                if out.contains_key(&font_slice) {
                    continue;
                }

                let subfont_id = chunk.alloc();
                out.insert(font_slice, subfont_id);

                // Allocate some IDs.
                let cmap_ref = chunk.alloc();
                let descriptor_ref = chunk.alloc();
                let widths_ref = chunk.alloc();
                // And a map between glyph IDs and the instructions to draw this
                // glyph.
                let mut glyphs_to_instructions = Vec::new();

                let start = font_index * 256;
                let end = (start + 256).min(color_font.glyphs.len());
                let glyph_count = end - start;
                let subset = &color_font.glyphs[start..end];
                let mut widths = Vec::new();
                let mut gids = Vec::new();

                let scale_factor = font.ttf().units_per_em() as f32;

                // Write the instructions for each glyph.
                for color_glyph in subset {
                    let instructions_stream_ref = chunk.alloc();
                    let width = font
                        .advance(color_glyph.gid)
                        .unwrap_or(Em::new(0.0))
                        .to_font_units();
                    widths.push(width);
                    chunk
                        .stream(
                            instructions_stream_ref,
                            color_glyph.instructions.content.wait(),
                        )
                        .filter(Filter::FlateDecode);

                    // Use this stream as instructions to draw the glyph.
                    glyphs_to_instructions.push(instructions_stream_ref);
                    gids.push(color_glyph.gid);
                }

                // Write the Type3 font object.
                let mut pdf_font = chunk.type3_font(subfont_id);
                pdf_font.pair(Name(b"Resources"), color_fonts.resources.reference);
                pdf_font.bbox(color_font.bbox);
                pdf_font.matrix([
                    1.0 / scale_factor,
                    0.0,
                    0.0,
                    1.0 / scale_factor,
                    0.0,
                    0.0,
                ]);
                pdf_font.first_char(0);
                pdf_font.last_char((glyph_count - 1) as u8);
                pdf_font.pair(Name(b"Widths"), widths_ref);
                pdf_font.to_unicode(cmap_ref);
                pdf_font.font_descriptor(descriptor_ref);

                // Write the /CharProcs dictionary, that maps glyph names to
                // drawing instructions.
                let mut char_procs = pdf_font.char_procs();
                for (gid, instructions_ref) in glyphs_to_instructions.iter().enumerate() {
                    char_procs.pair(
                        Name(eco_format!("glyph{gid}").as_bytes()),
                        *instructions_ref,
                    );
                }
                char_procs.finish();

                // Write the /Encoding dictionary.
                let names = (0..glyph_count)
                    .map(|gid| eco_format!("glyph{gid}"))
                    .collect::<Vec<_>>();
                pdf_font
                    .encoding_custom()
                    .differences()
                    .consecutive(0, names.iter().map(|name| Name(name.as_bytes())));
                pdf_font.finish();

                // Encode a CMAP to make it possible to search or copy glyphs.
                let glyph_set = resources.glyph_sets.get(font).unwrap();
                let mut cmap = UnicodeCmap::new(CMAP_NAME, SYSTEM_INFO);
                for (index, glyph) in subset.iter().enumerate() {
                    let Some(text) = glyph_set.get(&glyph.gid) else {
                        continue;
                    };

                    if !text.is_empty() {
                        cmap.pair_with_multiple(index as u8, text.chars());
                    }
                }
                chunk.cmap(cmap_ref, &cmap.finish());

                // Write the font descriptor.
                gids.sort();
                let subset_tag = subset_tag(&gids);
                let postscript_name = font
                    .find_name(name_id::POST_SCRIPT_NAME)
                    .unwrap_or_else(|| "unknown".to_string());
                let base_font = eco_format!("{subset_tag}+{postscript_name}");
                write_font_descriptor(&mut chunk, descriptor_ref, font, &base_font);

                // Write the widths array
                chunk.indirect(widths_ref).array().items(widths);
            }
        }
    });

    (chunk, out)
}

/// A mapping between `Font`s and all the corresponding `ColorFont`s.
///
/// This mapping is one-to-many because there can only be 256 glyphs in a Type 3
/// font, and fonts generally have more color glyphs than that.
pub struct ColorFontMap<R> {
    /// The mapping itself.
    pub map: IndexMap<Font, ColorFont>,
    pub resources: Resources<R>,
    /// The number of font slices (groups of 256 color glyphs), across all color
    /// fonts.
    total_slice_count: usize,
}

/// A collection of Type3 font, belonging to the same TTF font.
pub struct ColorFont {
    /// The IDs of each sub-slice of this font. They are the numbers after "Cf"
    /// in the Resources dictionaries.
    slice_ids: Vec<usize>,
    /// The list of all color glyphs in this family.
    ///
    /// The index in this vector modulo 256 corresponds to the index in one of
    /// the Type3 fonts in `refs` (the `n`-th in the vector, where `n` is the
    /// quotient of the index divided by 256).
    pub glyphs: Vec<ColorGlyph>,
    /// The global bounding box of the font.
    pub bbox: Rect,
    /// A mapping between glyph IDs and character indices in the `glyphs`
    /// vector.
    glyph_indices: HashMap<u16, usize>,
}

/// A single color glyph.
pub struct ColorGlyph {
    /// The ID of the glyph.
    pub gid: u16,
    /// Instructions to draw the glyph.
    pub instructions: content::Encoded,
}

impl ColorFontMap<()> {
    /// Creates a new empty mapping
    pub fn new() -> Self {
        Self {
            map: IndexMap::new(),
            total_slice_count: 0,
            resources: Resources::default(),
        }
    }

    pub fn get(&mut self, font: &Font, gid: u16) -> (usize, u8) {
        let color_font = self.map.entry(font.clone()).or_insert_with(|| {
            let global_bbox = font.ttf().global_bounding_box();
            let bbox = Rect::new(
                font.to_em(global_bbox.x_min).to_font_units(),
                font.to_em(global_bbox.y_min).to_font_units(),
                font.to_em(global_bbox.x_max).to_font_units(),
                font.to_em(global_bbox.y_max).to_font_units(),
            );
            ColorFont {
                bbox,
                slice_ids: Vec::new(),
                glyphs: Vec::new(),
                glyph_indices: HashMap::new(),
            }
        });

        if let Some(index_of_glyph) = color_font.glyph_indices.get(&gid) {
            // If we already know this glyph, return it.
            (color_font.slice_ids[index_of_glyph / 256], *index_of_glyph as u8)
        } else {
            // Otherwise, allocate a new ColorGlyph in the font, and a new Type3 font
            // if needed
            let index = color_font.glyphs.len();
            if index % 256 == 0 {
                color_font.slice_ids.push(self.total_slice_count);
                self.total_slice_count += 1;
            }

            let frame = frame_for_glyph(font, gid);
            let instructions = content::build(&mut self.resources, &frame);
            color_font.glyphs.push(ColorGlyph { gid, instructions });
            color_font.glyph_indices.insert(gid, index);

            (color_font.slice_ids[index / 256], index as u8)
        }
    }

    pub fn with_refs(self, refs: &ResourcesRefs) -> ColorFontMap<Ref> {
        ColorFontMap {
            map: self.map,
            resources: self.resources.with_refs(refs),
            total_slice_count: self.total_slice_count,
        }
    }
}

#[derive(PartialEq, Eq, Hash, Debug)]
pub struct ColorFontSlice {
    pub font: Font,
    pub subfont: usize,
}