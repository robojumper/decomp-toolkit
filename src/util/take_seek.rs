// MIT License
//
// Copyright (c) jam1garner and other contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
//
// Source: https://github.com/jam1garner/binrw/blob/919c395316b9e971bdccb5547272098f20702c5b/binrw/src/io/take_seek.rs

//! Types for seekable reader adapters which limit the number of bytes read from
//! the underlying reader.

use std::io::{Read, Result, Seek, SeekFrom};

/// Read adapter which limits the bytes read from an underlying reader, with
/// seek support.
///
/// This struct is generally created by importing the [`TakeSeekExt`] extension
/// and calling [`take_seek`] on a reader.
///
/// [`take_seek`]: TakeSeekExt::take_seek
#[derive(Debug)]
pub struct TakeSeek<T> {
    inner: T,
    pos: u64,
    end: u64,
}

impl<T> TakeSeek<T> {
    /// Gets a reference to the underlying reader.
    pub fn get_ref(&self) -> &T { &self.inner }

    /// Gets a mutable reference to the underlying reader.
    ///
    /// Care should be taken to avoid modifying the internal I/O state of the
    /// underlying reader as doing so may corrupt the internal limit of this
    /// `TakeSeek`.
    pub fn get_mut(&mut self) -> &mut T { &mut self.inner }

    /// Consumes this wrapper, returning the wrapped value.
    pub fn into_inner(self) -> T { self.inner }

    /// Returns the number of bytes that can be read before this instance will
    /// return EOF.
    ///
    /// # Note
    ///
    /// This instance may reach EOF after reading fewer bytes than indicated by
    /// this method if the underlying [`Read`] instance reaches EOF.
    pub fn limit(&self) -> u64 { self.end.saturating_sub(self.pos) }
}

impl<T: Seek> TakeSeek<T> {
    /// Sets the number of bytes that can be read before this instance will
    /// return EOF. This is the same as constructing a new `TakeSeek` instance,
    /// so the amount of bytes read and the previous limit value don’t matter
    /// when calling this method.
    ///
    /// # Panics
    ///
    /// Panics if the inner stream returns an error from `stream_position`.
    pub fn set_limit(&mut self, limit: u64) {
        let pos = self.inner.stream_position().expect("cannot get position for `set_limit`");
        self.pos = pos;
        self.end = pos + limit;
    }
}

impl<T: Read> Read for TakeSeek<T> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let limit = self.limit();

        // Don't call into inner reader at all at EOF because it may still block
        if limit == 0 {
            return Ok(0);
        }

        // Lint: It is impossible for this cast to truncate because the value
        // being cast is the minimum of two values, and one of the value types
        // is already `usize`.
        #[allow(clippy::cast_possible_truncation)]
        let max = (buf.len() as u64).min(limit) as usize;
        let n = self.inner.read(&mut buf[0..max])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<T: Seek> Seek for TakeSeek<T> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }

    fn stream_position(&mut self) -> Result<u64> { Ok(self.pos) }
}

/// An extension trait that implements `take_seek()` for compatible streams.
pub trait TakeSeekExt {
    /// Creates an adapter which will read at most `limit` bytes from the
    /// wrapped stream.
    fn take_seek(self, limit: u64) -> TakeSeek<Self>
    where Self: Sized;
}

impl<T: Read + Seek> TakeSeekExt for T {
    fn take_seek(mut self, limit: u64) -> TakeSeek<Self>
    where Self: Sized {
        let pos = self.stream_position().expect("cannot get position for `take_seek`");

        TakeSeek { inner: self, pos, end: pos + limit }
    }
}
