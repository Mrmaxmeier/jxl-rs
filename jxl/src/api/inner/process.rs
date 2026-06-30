// Copyright (c) the JPEG XL Project Authors. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{
    io::IoSliceMut,
    ops::{Deref, Range},
};

use crate::error::Result;

use crate::api::{JxlBitstreamInput, JxlDecoderInner, JxlOutputBuffer, ProcessingResult};

// General implementation strategy:
// - Anything that is not a section is read into a small buffer.
// - As soon as we know section sizes, data is read directly into sections.
// When the start of the populated range in `buf` goes past half of its length,
// the data in the buffer is moved back to the beginning.

pub(super) struct SmallBuffer {
    buf: Vec<u8>,
    range: Range<usize>,
}

impl SmallBuffer {
    pub(super) fn refill(
        &mut self,
        mut get_input: impl FnMut(&mut [IoSliceMut]) -> Result<usize, std::io::Error>,
        max: Option<usize>,
    ) -> Result<usize> {
        let mut total = 0;
        loop {
            if self.range.start >= self.buf.len() / 2 && !self.range.is_empty() {
                let start = self.range.start;
                let len = self.range.len();
                self.buf.copy_within(start..start + len, 0);
                self.range.start = 0;
                self.range.end = len;
            }
            if self.range.len() >= self.buf.len() / 2 {
                break;
            }
            let stop = if let Some(max) = max {
                self.range
                    .end
                    .saturating_add(max.saturating_sub(total))
                    .min(self.buf.len())
            } else {
                self.buf.len()
            };
            let num = get_input(&mut [IoSliceMut::new(&mut self.buf[self.range.end..stop])])?;
            total += num;
            self.range.end += num;
            if num == 0 {
                break;
            }
        }
        Ok(total)
    }

    pub(super) fn take(&mut self, mut buffers: &mut [IoSliceMut]) -> usize {
        let mut num = 0;
        while !self.range.is_empty() {
            let Some((buf, rest)) = buffers.split_first_mut() else {
                break;
            };
            buffers = rest;
            let len = self.range.len().min(buf.len());
            // Only copy 'len' bytes, not the entire range, to avoid panic when buf is smaller than range
            buf[..len].copy_from_slice(&self.buf[self.range.start..self.range.start + len]);
            self.range.start += len;
            num += len;
        }
        num
    }

    pub(super) fn consume(&mut self, amount: usize) -> usize {
        let amount = amount.min(self.range.len());
        self.range.start += amount;
        amount
    }

    /// Prepend bytes so they are returned next by [`Self::take`] (and appear first in [`Deref`]).
    pub(super) fn inject_bytes_front(&mut self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        if self.range.is_empty() {
            self.buf = data;
            self.range = 0..self.buf.len();
            return;
        }
        let mut combined = data;
        combined.extend_from_slice(&self.buf[self.range.clone()]);
        self.buf = combined;
        self.range = 0..self.buf.len();
    }

    pub(super) fn new(initial_size: usize) -> Self {
        Self {
            buf: vec![0; initial_size],
            range: 0..0,
        }
    }

    pub(super) fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub(super) fn enlarge(&mut self) {
        // Note: we need a *4 here because doubling the buffer size might still not allow refill() to make progress.
        self.buf.resize(self.buf.len() * 4, 0);
    }

    pub(super) fn can_read_more(&self) -> bool {
        self.buf.len() > self.len() * 2 && self.range.end < self.buf.len()
    }
}

impl Deref for SmallBuffer {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.buf[self.range.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refill_compacts_when_range_spans_second_half_with_odd_capacity() {
        let mut buf = SmallBuffer::new(93);
        // Fill the second half so compaction runs with len > start (93/2 = 46, len = 47).
        buf.range = 46..93;
        for i in 46..93 {
            buf.buf[i] = (i - 46) as u8;
        }
        let read = buf.refill(|_| Ok(0), None).expect("refill must not panic");
        assert_eq!(read, 0);
        assert_eq!(buf.range, 0..47);
        assert_eq!(&buf.buf[0..47], &(0u8..47).collect::<Vec<_>>());
    }
}

impl JxlDecoderInner {
    /// Process more of the input file.
    /// This function will return when reaching the next decoding stage (i.e. finished decoding
    /// file/frame header, or finished decoding a frame).
    /// If called when decoding a frame with `None` for buffers, the frame will still be read,
    /// but pixel data will not be produced.
    #[inline(never)]
    pub fn process(
        &mut self,
        input: &mut dyn JxlBitstreamInput,
        buffers: Option<&mut [JxlOutputBuffer]>,
    ) -> Result<ProcessingResult<(), ()>> {
        ProcessingResult::new(self.codestream_parser.process(
            &mut self.box_parser,
            input,
            &self.options,
            buffers,
            false,
        ))
    }

    /// Draws all the pixels we have data for. Returns `true` if any new pixels
    /// were written to `buffers` since the previous call to `flush_pixels`;
    /// returns `false` if no new rendering has happened, in which case the
    /// contents of `buffers` are unchanged from the caller's perspective.
    pub fn flush_pixels(&mut self, buffers: &mut [JxlOutputBuffer]) -> Result<bool> {
        let mut input: &[u8] = &[];
        match self.codestream_parser.process(
            &mut self.box_parser,
            &mut input,
            &self.options,
            Some(buffers),
            true,
        ) {
            Ok(()) | Err(crate::error::Error::OutOfBounds(_)) => {
                let updated = self.codestream_parser.pixels_dirty;
                self.codestream_parser.pixels_dirty = false;
                Ok(updated)
            }
            Err(e) => Err(e),
        }
    }
}
