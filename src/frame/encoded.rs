//! Encoded stills as the parts they are written in: a body that borrows the
//! frame where the still is bytes the frame already holds (raw, and the
//! JPEG a source sent), and at most one insert that goes into it on its way
//! out (a JPEG's comment segment). Files and pipes take the parts with
//! vectored writes, so neither the frame nor the encoder's output is copied
//! to put them together.
#![deny(clippy::missing_const_for_fn)]
use std::io::{IoSlice, Write};

/// The bulk of an encoded image.
pub enum Body<'a> {
    /// The frame's own bytes.
    Borrowed(&'a [u8]),
    Vec(Vec<u8>),
    /// A conversion of the frame, in a buffer that goes back to its pool.
    Pooled(crate::framebuf::PoolVec),
    /// libjpeg-turbo's output, in the buffer it allocated.
    #[cfg(feature = "jpeg")]
    Jpeg(crate::jpeg::Jpeg),
    /// A JPEG in the sample buffer Apple's JPEG engine wrote it to.
    #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
    Native(crate::jpeg::videotoolbox::Output),
}

impl Clone for Body<'_> {
    fn clone(&self) -> Self {
        match self {
            Body::Borrowed(bytes) => Body::Borrowed(bytes),
            #[cfg(feature = "jpeg")]
            Body::Jpeg(jpeg) => Body::Jpeg(jpeg.clone()),
            other => Body::Vec(other.to_vec()),
        }
    }
}

impl std::ops::Deref for Body<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Body::Borrowed(bytes) => bytes,
            Body::Vec(bytes) => bytes,
            Body::Pooled(bytes) => bytes,
            #[cfg(feature = "jpeg")]
            Body::Jpeg(bytes) => bytes,
            #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
            Body::Native(bytes) => bytes,
        }
    }
}
impl AsRef<[u8]> for Body<'_> {
    fn as_ref(&self) -> &[u8] {
        self
    }
}
impl std::fmt::Debug for Body<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Body({} bytes)", self.len())
    }
}
impl PartialEq for Body<'_> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for Body<'_> {}
impl PartialEq<Vec<u8>> for Body<'_> {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == **other
    }
}
impl PartialEq<Body<'_>> for Vec<u8> {
    fn eq(&self, other: &Body<'_>) -> bool {
        **self == **other
    }
}
impl From<Vec<u8>> for Body<'_> {
    fn from(bytes: Vec<u8>) -> Self {
        Body::Vec(bytes)
    }
}

/// An encoded image: `body`, with `insert`'s bytes placed before the
/// body's byte at its offset.
#[derive(Debug)]
pub struct Encoded<'a> {
    body: Body<'a>,
    insert: Option<(usize, Vec<u8>)>,
    /// The body is the JPEG the source sent the frame as.
    passthrough: bool,
}

impl<'a> Encoded<'a> {
    pub(crate) const fn new(body: Body<'a>) -> Self {
        Self {
            body,
            insert: None,
            passthrough: false,
        }
    }
    /// `bytes` placed before the body's byte at `at`, which must lie within
    /// the body.
    pub(crate) fn with_insert(body: Body<'a>, at: usize, bytes: Vec<u8>) -> Self {
        assert!(at <= body.len(), "insert past the end of the body");
        Self {
            body,
            insert: Some((at, bytes)),
            passthrough: false,
        }
    }
    /// Its body is the JPEG the source sent, not an encoding of the frame.
    pub(crate) fn passed_through(self) -> Self {
        Self {
            passthrough: true,
            ..self
        }
    }
    pub const fn passthrough(&self) -> bool {
        self.passthrough
    }
    pub fn len(&self) -> usize {
        self.body.len() + self.insert.as_ref().map_or(0, |(_, bytes)| bytes.len())
    }
    /// The image's bytes in order, in up to three parts.
    pub fn parts(&self) -> [&[u8]; 3] {
        match &self.insert {
            Some((at, bytes)) => [&self.body[..*at], bytes, &self.body[*at..]],
            None => [&self.body, &[], &[]],
        }
    }
    /// The image in one buffer, for callers that need it whole.
    #[cfg(any(test, feature = "testing"))]
    pub fn to_vec(&self) -> Vec<u8> {
        self.parts().concat()
    }
    /// Write every byte to `out`, gathered into as few writes as it takes.
    ///
    /// # Errors
    /// Returns the writer's I/O error, including `WriteZero` if it stops
    /// accepting bytes before the image is complete. Interrupted writes retry.
    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        write_all_vectored(out, &mut self.parts().map(IoSlice::new))
    }
}

/// `write_all` for vectored writes: every byte of `slices`, taking up where
/// a short write left off. Writers without vectored writes take a part at a
/// time.
pub(crate) fn write_all_vectored(
    out: &mut impl Write,
    mut slices: &mut [IoSlice<'_>],
) -> std::io::Result<()> {
    // Leading empty parts would make a write of nothing look like an end.
    IoSlice::advance_slices(&mut slices, 0);
    while !slices.is_empty() {
        match out.write_vectored(slices) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => IoSlice::advance_slices(&mut slices, written),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inserts_go_between_the_parts_of_the_body() {
        let frame = [1, 2, 3, 4];
        let plain = Encoded::new(Body::Borrowed(&frame));
        assert_eq!(plain.parts(), [&frame[..], &[], &[]]);
        assert_eq!((plain.len(), plain.to_vec()), (4, frame.to_vec()));
        assert_eq!(plain.body.as_ptr(), frame.as_ptr());
        let header = Encoded::with_insert(Body::Borrowed(&frame), 0, b"P5".to_vec());
        assert_eq!(header.to_vec(), b"P5\x01\x02\x03\x04");
        let middle = Encoded::with_insert(Body::Vec(frame.to_vec()), 2, vec![9]);
        assert_eq!(middle.to_vec(), [1, 2, 9, 3, 4]);
        assert_eq!(middle.len(), 5);
        let mut out = Vec::new();
        middle.write_to(&mut out).unwrap();
        assert_eq!(out, [1, 2, 9, 3, 4]);
        let end = Encoded::with_insert(Body::Vec(frame.to_vec()), 4, vec![9]);
        assert_eq!(end.to_vec(), [1, 2, 3, 4, 9]);
        assert_eq!(Encoded::new(Body::Vec(Vec::new())).len(), 0);
    }
    #[test]
    #[should_panic(expected = "insert past the end")]
    fn inserts_lie_within_the_body() {
        Encoded::with_insert(Body::Vec(vec![1]), 2, vec![9]);
    }
}
