use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use libghostty_vt::Terminal;
use libghostty_vt::kitty::graphics::ImageFormat;
use libghostty_vt::render::{ImageIterator, ImageLayer, ImageProtocol, Snapshot};

/// Straight-alpha RGBA pixels, converted off the UI thread so it only uploads them.
pub struct Image {
    pub id: u32,
    /// Changes with the pixels, so a texture keyed by `(id, generation)` is never stale.
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One rectangle of an image, in the terminal's pixels from the top-left of the viewport.
#[derive(Clone)]
pub struct Placement {
    pub image: Arc<Image>,
    pub layer: ImageLayer,
    /// The top-left cell, negative where the image starts beyond the viewport.
    pub col: i32,
    pub row: i32,
    /// From the top-left of that cell.
    pub offset: [u32; 2],
    /// The size to draw, whatever the source's.
    pub size: [u32; 2],
    /// The part of the image to draw, as `[x, y, width, height]` in its pixels.
    pub source: [u32; 4],
}

fn to_rgba(format: ImageFormat, pixels: &[u8]) -> Option<Vec<u8>> {
    Some(match format {
        ImageFormat::Rgba => pixels.to_vec(),
        ImageFormat::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[red, green, blue]| [red, green, blue, 255])
            .collect(),
        ImageFormat::Gray => pixels
            .iter()
            .flat_map(|&gray| [gray, gray, gray, 255])
            .collect(),
        ImageFormat::GrayAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[gray, alpha]| [gray, gray, gray, alpha])
            .collect(),
        // libghostty decodes PNGs when it stores them.
        _ => return None,
    })
}

pub(crate) struct ImageReader {
    iterator: ImageIterator<'static>,
    /// Converted images, so a placement that only moved shares its image's pixels with the last list.
    cache: HashMap<(u32, u64), Arc<Image>>,
    placements: Arc<[Placement]>,
    generation: Option<u64>,
}

impl ImageReader {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            iterator: ImageIterator::new()?,
            cache: HashMap::new(),
            placements: Arc::default(),
            generation: None,
        })
    }

    /// The placements in drawing order. libghostty's `generation` of the list says whether it is
    /// the one last read, so unchanged frames skip the iteration.
    pub(crate) fn read(
        &mut self,
        snapshot: &Snapshot<'static, '_>,
        terminal: &Terminal<'static, '_>,
        generation: u64,
    ) -> Result<Arc<[Placement]>> {
        if self.generation.replace(generation) == Some(generation) {
            return Ok(self.placements.clone());
        }
        let Some(graphics) = terminal.kitty_graphics()? else {
            return Ok(Arc::default());
        };
        let mut placements = Vec::new();
        let mut used = HashMap::new();
        let mut iteration = self.iterator.update(snapshot)?;
        while let Some(entry) = iteration.next() {
            if !matches!(entry.protocol(), Ok(ImageProtocol::Kitty)) {
                continue;
            }
            let (id, generation) = (entry.image_id()?, entry.image_generation()?);
            let image = match self.cache.get(&(id, generation)) {
                Some(image) => image.clone(),
                None => {
                    let stored = graphics
                        .image(id)
                        .context("a listed image should be in the storage")?;
                    let Some(pixels) = stored.data()? else {
                        continue;
                    };
                    let (width, height) = (stored.width()?, stored.height()?);
                    let Some(rgba) = to_rgba(stored.format()?, pixels) else {
                        continue;
                    };
                    Arc::new(Image {
                        id,
                        generation,
                        width,
                        height,
                        rgba,
                    })
                }
            };
            used.insert((id, generation), image.clone());
            placements.push((
                entry.z()?,
                Placement {
                    image,
                    layer: entry.layer()?,
                    col: entry.viewport_col()?,
                    row: entry.viewport_row()?,
                    offset: [entry.cell_offset_x()?, entry.cell_offset_y()?],
                    size: [entry.pixel_width()?, entry.pixel_height()?],
                    source: [
                        entry.source_x()?,
                        entry.source_y()?,
                        entry.source_width()?,
                        entry.source_height()?,
                    ],
                },
            ));
        }
        self.cache = used;
        placements.sort_by_key(|(z, placement)| (placement.layer as i32, *z));
        self.placements = placements
            .into_iter()
            .map(|(_, placement)| placement)
            .collect();
        Ok(self.placements.clone())
    }
}
