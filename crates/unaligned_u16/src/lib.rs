#[cfg(feature = "utf16")]
pub mod utf16;

pub mod error {
    /// An error indicating that the byte-length of the slice was not a multiple of tow.
    #[derive(Debug)]
    pub struct BadByteLength;
    impl core::error::Error for BadByteLength {}
    impl core::fmt::Display for BadByteLength {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "byte length must be a multiple of two")
        }
    }

    /// An error indicating that the slice was not correctly aligned.
    #[derive(Debug)]
    pub struct AlignmentError;
    impl core::error::Error for AlignmentError {}
    impl core::fmt::Display for AlignmentError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "slice is not correctly aligned")
        }
    }
}

pub mod iter {
    pub struct UnalignedU16SliceIterator<'a> { slice: &'a [u8] }
    impl<'a> UnalignedU16SliceIterator<'a> {
        pub fn new(slice: &super::UnalignedU16Slice<'a>) -> Self {
            Self { slice: slice.raw() }
        }
        pub fn remaining(&self) -> usize {
            self.slice.len() / 2
        }
    }
    impl Iterator for UnalignedU16SliceIterator<'_> {
        type Item = u16;
        fn next(&mut self) -> Option<Self::Item> {
            if self.slice.is_empty() { return None }
            let u16 = ((self.slice[1] as u16) << 8) | (self.slice[0] as u16);
            self.slice = &self.slice[2..];
            Some(u16)
        }
        fn size_hint(&self) -> (usize, Option<usize>) {
            let rem = self.remaining();
            (rem, Some(rem))
        }
    }
    impl ExactSizeIterator for UnalignedU16SliceIterator<'_> {}
    impl core::iter::FusedIterator for UnalignedU16SliceIterator<'_> {}
    impl DoubleEndedIterator for UnalignedU16SliceIterator<'_> {
        fn next_back(&mut self) -> Option<Self::Item> {
            if self.slice.is_empty() { return None }
            let end = self.slice.len();
            let u16 = ((self.slice[end - 1] as u16) << 8) | (self.slice[end - 2] as u16);
            self.slice = &self.slice[..=end - 3];
            Some(u16)
        }
    }
}

pub(crate) fn u16_slice_as_u8_slice(slice: &[u16]) -> &[u8] {
    let len = slice.len() * 2;
    let ptr = slice.as_ptr().cast();
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnalignedU16Slice<'a>(&'a [u8]);
impl<'a> UnalignedU16Slice<'a> {
    pub fn new(slice: &'a [u8]) -> Result<Self, error::BadByteLength> {
        if slice.len() % 2 != 0 { return Err(error::BadByteLength) }
        Ok(Self(slice))
    }

    /// # Safety
    /// - The provided slice must have a length that is a multiple of two.
    pub unsafe fn new_unchecked(slice: &'a [u8]) -> Self {
        Self(slice)
    }

    /// Returns the amount of `u16` elements.
    pub fn len(&self) -> usize {
        self.raw().len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn byte_len(&self) -> usize {
        self.raw().len()
    }

    pub fn raw(&self) -> &'a [u8] {
        self.0
    }
    pub fn get(&self, index: usize) -> Option<u16> {
        let real = index * 2;
        let u8 = self.raw();
        Some((
            (*u8.get(real + 1)? as u16) << 8) |
             *u8.get(real)?     as u16
        )
    }
    pub fn iter(&self) -> iter::UnalignedU16SliceIterator<'a> {
        self.into_iter()
    }
}
impl core::ops::Deref for UnalignedU16Slice<'_> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.0
    }
}
impl<'a> TryFrom<&'a [u8]> for UnalignedU16Slice<'a> {
    type Error = error::BadByteLength;
    fn try_from(value: &'a [u8]) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl<'a> From<&'a [u16]> for UnalignedU16Slice<'a> {
    fn from(value: &'a [u16]) -> Self {
        UnalignedU16Slice(u16_slice_as_u8_slice(value))
    }
}
impl<'a> From<&UnalignedU16Slice<'a>> for &'a [u8] {
    fn from(value: &UnalignedU16Slice<'a>) -> Self {
        value.0
    }
}
impl<'a> TryFrom<&UnalignedU16Slice<'a>> for &'a [u16] {
    type Error = error::AlignmentError;
    fn try_from(value: &UnalignedU16Slice<'a>) -> Result<Self, Self::Error> {
        let (unaligned, aligned, _) = unsafe { value.0.align_to::<u16>() };
        if unaligned.is_empty() { Ok(aligned) } else { Err(error::AlignmentError) }
    }
}
impl<'a> IntoIterator for &UnalignedU16Slice<'a> {
    type Item = u16;
    type IntoIter = iter::UnalignedU16SliceIterator<'a>;
    fn into_iter(self) -> Self::IntoIter {
        iter::UnalignedU16SliceIterator::new(self)
    }
}
