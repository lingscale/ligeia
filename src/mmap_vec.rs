use std::{fs::File, io, marker::PhantomData, mem, ops::{Deref, DerefMut}, slice};
use mapr::{MmapOptions, MmapMut};

/// Variable length data.
pub trait VariableLength {
    type Meta;
    /// Maximum possible length (in bytes). Should be small.
    fn max_length(meta: &Self::Meta) -> usize;
    /// Returns number of bytes written.
    fn write_bytes(&self, meta: &Self::Meta, b: &mut [u8]) -> usize;
    /// Reads from `b` as necessary.
    fn from_bytes(meta: &Self::Meta, b: &mut &[u8]) -> Self;
}

/// Creates an appendable vector that's backed by
/// an anonymous, temporary file, so it can contain more data than
/// fits in physical memory.
pub struct VarMmapVec<T> {
    f: File,
    mapping: MmapMut,
    count: &'static mut u64,
    bytes: usize,
    cap: usize,
    _marker: PhantomData<T>,
}

impl<T: VariableLength> VarMmapVec<T> {
    pub unsafe fn create() -> io::Result<Self> {
        let f = tempfile::tempfile()?;

        let cap = 4096; // Let's try a single page to start.

        f.set_len(cap)?;

        let mapping = MmapOptions::new()
            .len(cap as usize)
            .map_mut(&f)?;

        let count = &mut *(mapping.as_ptr() as *mut u64);

        Ok(Self {
            f,
            mapping,
            count,
            bytes: 16,
            cap: cap as usize,
            _marker: PhantomData,
        })
    }

    /// Returns offset of item in buffer.
    pub fn push(&mut self, meta: &T::Meta, v: T) -> usize {
        if self.bytes + T::max_length(meta) >= self.cap {
            self.realloc();
        }

        let offset = self.bytes;

        self.bytes += v.write_bytes(meta, &mut self.mapping[self.bytes..]);
        *self.count += 1;
        offset
    }

    #[cold]
    fn realloc(&mut self) {
        self.cap *= 2;
        self.f.set_len(self.cap as u64).expect("failed to extend file");
        let mapping = unsafe {
            MmapOptions::new()
                .len(self.cap)
                .map_mut(&self.f)
                .expect("failed to create to mapping")
        };
        self.mapping = mapping;
        self.count = unsafe { &mut *(self.mapping.as_ptr() as *mut u64) };
    }

    pub fn iter(&self) -> MmapVecIter<T> {
        MmapVecIter {
            data: &self.mapping[16..],
            count: *self.count,
            _marker: PhantomData,
        }
    }

    pub fn flush(&self) {
        self.mapping.flush().expect("failed to flush mapping");
    }

    // /// This will return garbage if the offset is not aligned to the beginning
    // /// of a variable-length item.
    // pub fn get_at_offset(&self, offset: usize) -> T {
    //     T::from_bytes(&mut &self.mapping[offset..])
    // }
}

pub struct MmapVecIter<'a, T> {
    data: &'a [u8],
    count: u64,
    _marker: PhantomData<T>,
}

impl<'a, T: VariableLength> MmapVecIter<'a, T> {
    pub fn next(&mut self, meta: &T::Meta) -> Option<T> {
        if self.count > 0 {
            self.count -= 1;
            Some(T::from_bytes(meta, &mut self.data))
        } else {
            None
        }
    }
}

// impl<'a, T: VariableLength> Iterator for MmapVecIter<'a, T> {
//     type Item = T;
//     fn next(&mut self) -> Option<Self::Item> {
//         if self.count > 0 {
//             self.count -= 1;
//             Some(T::from_bytes(&mut self.data))
//         } else {
//             None
//         }
//     }
// }

pub unsafe trait Pod: Copy + Sized {}

unsafe impl Pod for u64 {}

struct ConstantLength<T>(T);

impl<T: Pod> VariableLength for ConstantLength<T> {
    type Meta = ();
    fn max_length(_: &()) -> usize {
        mem::size_of::<T>()
    }

    fn write_bytes(&self, _: &(), b: &mut [u8]) -> usize {
        assert!(b.len() >= mem::size_of::<T>());
        unsafe { *(b.as_mut_ptr() as *mut T) = self.0 };
        mem::size_of::<T>()
    }

    fn from_bytes(_: &(), b: &mut &[u8]) -> Self {
        assert!(b.len() >= mem::size_of::<T>());
        let v = unsafe { *(b.as_ptr() as *const T) };
        *b = &b[mem::size_of::<T>()..];

        Self(v)
    }
}

pub struct MmapVec<T> {
    inner: VarMmapVec<ConstantLength<T>>,
    len: usize,
}

impl<T: Pod> MmapVec<T> {
    pub unsafe fn create() -> io::Result<Self> {
        Ok(Self {
            inner: VarMmapVec::create()?,
            len: 0,
        })
    }

    /// Returns index of value.
    pub fn push(&mut self, v: T) -> usize {
        self.inner.push(&(), ConstantLength(v));
        let index = self.len;
        self.len += 1;
        index
    }
}

impl<T: Pod> Deref for MmapVec<T> {
    type Target = [T];
    
    fn deref(&self) -> &Self::Target {
        let slice = &self.inner.mapping[16..];
        unsafe {
            slice::from_raw_parts(slice.as_ptr() as *const T, self.len)
        }
    }
}

impl<T: Pod> DerefMut for MmapVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let slice = &mut self.inner.mapping[16..];
        unsafe {
            slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut T, self.len)
        }
    }
}
