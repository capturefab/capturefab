//! Readers that expose transient I/O failures to parser tests.
#![forbid(unsafe_code)]
use std::io::{self, BufRead, Read, Seek, SeekFrom};

/// Fail before every other read without consuming any bytes.
pub(crate) struct FaultReader<R> {
    inner: R,
    fail: bool,
    kind: io::ErrorKind,
}
impl<R> FaultReader<R> {
    #[must_use]
    pub(crate) const fn new(inner: R, kind: io::ErrorKind) -> Self {
        Self {
            inner,
            fail: false,
            kind,
        }
    }
    fn fault(&mut self) -> io::Result<()> {
        self.fail = !self.fail;
        if self.fail {
            return Err(self.kind.into());
        }
        Ok(())
    }
}
impl<R: Read> Read for FaultReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.fault()?;
        self.inner.read(bytes)
    }
}
impl<R: BufRead> BufRead for FaultReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.fault()?;
        self.inner.fill_buf()
    }
    fn consume(&mut self, count: usize) {
        self.inner.consume(count);
    }
}
impl<R: Seek> Seek for FaultReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}
