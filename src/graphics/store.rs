//! Per-pane Kitty image blob store with a simple size cap.

use std::collections::HashMap;

/// Soft cap on total stored image bytes per pane (~32 MiB).
const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct StoredImage {
    pub width: u32,
    pub height: u32,
    pub format: u8,
    pub zlib: bool,
    pub data: Vec<u8>,
    pub place_cols: Option<u32>,
    pub place_rows: Option<u32>,
}

#[derive(Debug, Default)]
pub struct ImageStore {
    images: HashMap<u32, StoredImage>,
    total_bytes: usize,
    /// Insertion order for crude eviction.
    order: Vec<u32>,
}

impl ImageStore {
    pub fn insert(&mut self, id: u32, img: StoredImage) {
        if img.data.is_empty() && img.width == 0 {
            // Placement-only update: merge onto existing.
            if let Some(existing) = self.images.get_mut(&id) {
                if img.place_cols.is_some() {
                    existing.place_cols = img.place_cols;
                }
                if img.place_rows.is_some() {
                    existing.place_rows = img.place_rows;
                }
            }
            return;
        }
        self.remove(id);
        while self.total_bytes + img.data.len() > MAX_BYTES && !self.order.is_empty() {
            let old = self.order.remove(0);
            self.remove(old);
        }
        self.total_bytes += img.data.len();
        self.order.push(id);
        self.images.insert(id, img);
    }

    pub fn get(&self, id: u32) -> Option<&StoredImage> {
        self.images.get(&id)
    }

    pub fn remove(&mut self, id: u32) -> Option<StoredImage> {
        if let Some(img) = self.images.remove(&id) {
            self.total_bytes = self.total_bytes.saturating_sub(img.data.len());
            self.order.retain(|x| *x != id);
            Some(img)
        } else {
            None
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &StoredImage)> {
        self.images.iter().map(|(k, v)| (*k, v))
    }

    pub fn ids_in_order(&self) -> &[u32] {
        &self.order
    }
}
