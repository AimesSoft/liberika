//! Ordered, bounded subtitle atlas pages. A one-pixel border preserves linear
//! filtering even when a single libass bitmap must span multiple textures.
use crate::core::{PlayerError, Result};
use crate::overlay::OverlayFrame;
use crate::subtitle::{SubtitleAlphaBitmap, SubtitleBitmapPlane};
use std::sync::Arc;

// Bounds retained CPU keys/layouts, R8 texture payload, and retained upload
// scratch. Large subtitles still render in full.
const CACHE_BUDGET: usize = 32 * 1024 * 1024;

struct BitmapKey {
    width: u32,
    height: u32,
    stride: usize,
    alpha: Box<[u8]>,
}

enum AlphaStorage {
    Owned(Box<[BitmapKey]>),
    // Alpha-only frames keep the presenter's allocation alive; mixed RGBA
    // overlays always use the owned mask snapshot instead.
    Shared(Arc<OverlayFrame>),
}

pub(super) struct AlphaSnapshot {
    storage: AlphaStorage,
}

impl AlphaSnapshot {
    pub fn new(bitmaps: &[SubtitleAlphaBitmap]) -> Arc<Self> {
        Arc::new(Self {
            storage: AlphaStorage::Owned(
                bitmaps
                    .iter()
                    .map(|bitmap| BitmapKey {
                        width: bitmap.placement.width,
                        height: bitmap.placement.height,
                        stride: bitmap.stride,
                        alpha: bitmap.alpha.clone().into_boxed_slice(),
                    })
                    .collect(),
            ),
        })
    }

    pub fn matches(&self, bitmaps: &[SubtitleAlphaBitmap]) -> bool {
        match &self.storage {
            AlphaStorage::Owned(keys) => {
                keys.len() == bitmaps.len()
                    && keys.iter().zip(bitmaps).all(|(key, bitmap)| {
                        key.width == bitmap.placement.width
                            && key.height == bitmap.placement.height
                            && key.stride == bitmap.stride
                            && key.alpha.as_ref() == bitmap.alpha.as_slice()
                    })
            }
            AlphaStorage::Shared(owner) => {
                owner.subtitle_alpha_planes.len() == bitmaps.len()
                    && owner
                        .subtitle_alpha_planes
                        .iter()
                        .zip(bitmaps)
                        .all(|(key, bitmap)| {
                            key.placement.width == bitmap.placement.width
                                && key.placement.height == bitmap.placement.height
                                && key.stride == bitmap.stride
                                && key.alpha == bitmap.alpha
                        })
            }
        }
    }

    #[cfg(test)]
    pub fn alpha(&self, index: usize) -> &[u8] {
        match &self.storage {
            AlphaStorage::Owned(keys) => &keys[index].alpha,
            AlphaStorage::Shared(owner) => &owner.subtitle_alpha_planes[index].alpha,
        }
    }

    fn shared_retained_bytes(frame: &OverlayFrame) -> Option<usize> {
        // Charge the complete owner even while the presenter also retains it,
        // including spare capacities and both Arc allocations.
        let mut bytes = (4 * std::mem::size_of::<usize>())
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(std::mem::size_of::<OverlayFrame>())?
            .checked_add(
                frame
                    .subtitle_planes
                    .capacity()
                    .checked_mul(std::mem::size_of::<SubtitleBitmapPlane>())?,
            )?
            .checked_add(
                frame
                    .subtitle_alpha_planes
                    .capacity()
                    .checked_mul(std::mem::size_of::<SubtitleAlphaBitmap>())?,
            )?;
        for bitmap in &frame.subtitle_alpha_planes {
            bytes = bytes.checked_add(bitmap.alpha.capacity())?;
        }
        Some(bytes)
    }

    fn retained_bytes(bitmaps: &[SubtitleAlphaBitmap]) -> Option<usize> {
        // Arc's strong/weak counters plus the snapshot and exact boxed slices.
        let mut bytes = (2 * std::mem::size_of::<usize>())
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(
                bitmaps
                    .len()
                    .checked_mul(std::mem::size_of::<BitmapKey>())?,
            )?;
        for bitmap in bitmaps {
            bytes = bytes.checked_add(bitmap.alpha.len())?;
        }
        Some(bytes)
    }
}

pub(super) struct ContentKey {
    limit: u32,
    pub snapshot: Arc<AlphaSnapshot>,
}

impl ContentKey {
    fn with_budget(
        bitmaps: &[SubtitleAlphaBitmap],
        limit: u32,
        pages: &[Page],
        budget: usize,
    ) -> Option<(Self, usize)> {
        Self::snapshot_with_budget(
            limit,
            pages,
            budget,
            AlphaSnapshot::retained_bytes(bitmaps)?,
            || AlphaSnapshot::new(bitmaps),
        )
    }

    /// Returns the selected key and the remaining cache budget for upload scratch.
    pub fn new_for_overlay(
        frame: &OverlayFrame,
        owner: Option<&Arc<OverlayFrame>>,
        limit: u32,
        pages: &[Page],
    ) -> Option<(Self, usize)> {
        owner
            .and_then(|owner| Self::shared_with_budget(frame, owner, limit, pages, CACHE_BUDGET))
            .or_else(|| Self::with_budget(&frame.subtitle_alpha_planes, limit, pages, CACHE_BUDGET))
    }

    fn shared_with_budget(
        frame: &OverlayFrame,
        owner: &Arc<OverlayFrame>,
        limit: u32,
        pages: &[Page],
        budget: usize,
    ) -> Option<(Self, usize)> {
        if !std::ptr::eq(frame, owner.as_ref()) || !frame.subtitle_planes.is_empty() {
            return None;
        }
        Self::snapshot_with_budget(
            limit,
            pages,
            budget,
            AlphaSnapshot::shared_retained_bytes(frame)?,
            || {
                Arc::new(AlphaSnapshot {
                    storage: AlphaStorage::Shared(owner.clone()),
                })
            },
        )
    }

    fn snapshot_with_budget(
        limit: u32,
        pages: &[Page],
        budget: usize,
        snapshot_bytes: usize,
        snapshot: impl FnOnce() -> Arc<AlphaSnapshot>,
    ) -> Option<(Self, usize)> {
        let mut bytes = std::mem::size_of::<Self>()
            .checked_add(std::mem::size_of::<Box<[Page]>>())?
            .checked_add(snapshot_bytes)?;
        // Cached pages are an exact boxed slice; tiles retain their actual Vec
        // capacity. Charge the immutable snapshot fully even while shared.
        for page in pages {
            bytes = bytes
                .checked_add(std::mem::size_of::<Page>() + 2 * std::mem::size_of::<usize>())?
                .checked_add(
                    page.tiles
                        .capacity()
                        .checked_mul(std::mem::size_of::<Tile>())?,
                )?
                .checked_add((page.width as usize).checked_mul(page.height as usize)?)?;
        }
        let remaining_bytes = budget.checked_sub(bytes)?;
        Some((
            Self {
                limit,
                snapshot: snapshot(),
            },
            remaining_bytes,
        ))
    }

    pub fn matches(&self, bitmaps: &[SubtitleAlphaBitmap], limit: u32) -> bool {
        self.limit == limit && self.snapshot.matches(bitmaps)
    }
}

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
    pub fn pixel_len(&self) -> Result<usize> {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or_else(invalid)
    }

    pub fn write_pixels(
        &self,
        bitmaps: &[SubtitleAlphaBitmap],
        pixels: &mut Vec<u8>,
    ) -> Result<()> {
        let len = self.pixel_len()?;
        // Reset every byte, including unused rows below shorter tiles, before
        // filling this page. The buffer may contain pixels from an earlier page.
        pixels.clear();
        pixels.resize(len, 0);
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
        Ok(())
    }
}
