//! Ordered, bounded subtitle atlas pages. A one-pixel border preserves linear
//! filtering even when a single libass bitmap must span multiple textures.
use crate::core::{PlayerError, Result};
use crate::subtitle::SubtitleAlphaBitmap;

#[derive(Debug)]
pub(super) struct Tile {
    pub bitmap: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub atlas_x: u32,
}

#[derive(Debug, Default)]
pub(super) struct Page {
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<Tile>,
}

fn invalid() -> PlayerError {
    PlayerError::Renderer("d3d11: invalid subtitle alpha bitmap dimensions/stride/length".into())
}

pub(super) fn layout(bitmaps: &[SubtitleAlphaBitmap], limit: u32) -> Result<Vec<Page>> {
    let tile_limit = limit
        .checked_sub(2)
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    let mut pages = Vec::new();
    let mut page = Page::default();
    for (bitmap, image) in bitmaps.iter().enumerate() {
        let (width, height) = (image.placement.width, image.placement.height);
        if width == 0 || height == 0 {
            continue;
        }
        let required = image
            .stride
            .checked_mul((height - 1) as usize)
            .and_then(|n| n.checked_add(width as usize))
            .ok_or_else(invalid)?;
        if image.stride < width as usize || image.alpha.len() < required {
            return Err(invalid());
        }
        let mut y = 0;
        while y < height {
            let h = (height - y).min(tile_limit);
            let mut x = 0;
            while x < width {
                let w = (width - x).min(tile_limit);
                let padded = w + 2; // bounded by limit
                if page.width > limit - padded {
                    pages.push(std::mem::take(&mut page));
                }
                page.tiles.push(Tile {
                    bitmap,
                    x,
                    y,
                    width: w,
                    height: h,
                    atlas_x: page.width + 1,
                });
                page.width += padded;
                page.height = page.height.max(h + 2);
                x += w;
            }
            y += h;
        }
    }
    if !page.tiles.is_empty() {
        pages.push(page);
    }
    Ok(pages)
}

impl Page {
    pub fn pixels(&self, bitmaps: &[SubtitleAlphaBitmap]) -> Result<Vec<u8>> {
        let len = (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or_else(invalid)?;
        let mut pixels = vec![0; len];
        for tile in &self.tiles {
            let image = &bitmaps[tile.bitmap];
            for row in 0..tile.height + 2 {
                let src_y = tile
                    .y
                    .saturating_add(row)
                    .saturating_sub(1)
                    .min(image.placement.height - 1);
                let dst = row as usize * self.width as usize + (tile.atlas_x - 1) as usize;
                let src_row = src_y as usize * image.stride;
                let src = src_row + tile.x as usize;
                let width = tile.width as usize;
                pixels[dst + 1..dst + 1 + width].copy_from_slice(&image.alpha[src..src + width]);
                pixels[dst] = image.alpha[src_row + tile.x.saturating_sub(1) as usize];
                let right = (tile.x + tile.width).min(image.placement.width - 1);
                pixels[dst + width + 1] = image.alpha[src_row + right as usize];
            }
        }
        Ok(pixels)
    }
}
