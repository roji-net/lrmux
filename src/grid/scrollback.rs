// Scrollback: ring buffer of rows for history above the visible region.

use super::cell::Cell;

/// A ring buffer holding scrollback history rows.
pub struct Scrollback {
    rows: Vec<Vec<Cell>>,
    capacity: usize,
    head: usize, // next write position
    len: usize,  // number of valid rows
}

impl Scrollback {
    pub fn new(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            capacity,
            head: 0,
            len: 0,
        }
    }

    /// Push a row into the scrollback. If full, the oldest row is overwritten.
    pub fn push(&mut self, row: Vec<Cell>) {
        if self.len < self.capacity {
            if self.rows.len() < self.capacity {
                self.rows.push(row);
            } else {
                self.rows[self.head] = row;
            }
            self.head = (self.head + 1) % self.capacity;
            self.len += 1;
        } else {
            self.rows[self.head] = row;
            self.head = (self.head + 1) % self.capacity;
        }
    }

    /// Number of rows in the scrollback.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the scrollback is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get a row by index, where 0 is the oldest row.
    pub fn get(&self, index: usize) -> Option<&[Cell]> {
        if index >= self.len {
            return None;
        }
        let start = if self.len < self.capacity {
            0
        } else {
            self.head
        };
        let actual = (start + index) % self.capacity;
        Some(&self.rows[actual])
    }

    /// Iterate over all rows, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &[Cell]> {
        (0..self.len).map(move |i| {
            let start = if self.len < self.capacity {
                0
            } else {
                self.head
            };
            let actual = (start + i) % self.capacity;
            &self.rows[actual][..]
        })
    }
}
