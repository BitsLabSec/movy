use std::ops::Range;

/// An iterator that splits a range into chunks of a given size.
pub struct RangeChunks {
    start: usize,
    end: usize,
    chunk_size: usize,
}

impl RangeChunks {
    /// Creates a new iterator for splitting a range.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0.
    pub fn new(range: Range<usize>, chunk_size: usize) -> Self {
        assert!(chunk_size > 0, "Chunk size must be greater than 0");
        Self {
            start: range.start,
            end: range.end,
            chunk_size,
        }
    }
}

impl Iterator for RangeChunks {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        // If the current start is at or beyond the end, we're done.
        if self.start >= self.end {
            return None;
        }

        // Calculate the start and end of the current chunk.
        let chunk_start = self.start;
        // The end of the chunk is the smaller of two values:
        // 1. The start plus the chunk size.
        // 2. The end of the total range.
        // This ensures the last chunk doesn't go past N.
        let chunk_end = std::cmp::min(self.start + self.chunk_size, self.end);

        // Update our state for the next iteration.
        self.start = chunk_end;

        // Return the generated range.
        Some(chunk_start..chunk_end)
    }
}

// 1. Define the trait with the desired method signature.
pub trait RangeChunksExt {
    /// Splits a range into an iterator of smaller range chunks.
    fn range_chunks(self, chunk_size: usize) -> RangeChunks;
}

// 2. Implement the trait for the target type, `Range<usize>`.
impl RangeChunksExt for Range<usize> {
    fn range_chunks(self, chunk_size: usize) -> RangeChunks {
        // The implementation simply calls the constructor for our iterator.
        RangeChunks::new(self, chunk_size)
    }
}
