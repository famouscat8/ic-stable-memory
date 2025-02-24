use crate::mem::allocator::EMPTY_PTR;
use crate::utils::mem_context::{stable, PAGE_SIZE_BYTES};
use crate::utils::phantom_data::SPhantomData;
use speedy::{Context, Readable, Reader, Writable, Writer};
use std::mem::size_of;
use std::usize;

pub(crate) const ALLOCATED: usize = 2usize.pow(usize::BITS - 1); // first biggest bit set to 1, other set to 0
pub(crate) const FREE: usize = 2usize.pow(usize::BITS - 1) - 1; // first biggest bit set to 0, other set to 1
pub(crate) const CELL_META_SIZE: usize = size_of::<usize>() as usize;
pub(crate) const PTR_SIZE: usize = size_of::<u64>();
pub(crate) const CELL_MIN_SIZE: usize = PTR_SIZE * 2;

pub(crate) enum Side {
    Start,
    End,
}

/// A smart-pointer for stable memory.
pub struct SSlice<T> {
    pub(crate) ptr: u64,
    data: SPhantomData<T>,
    pub(crate) size: usize,
    pub(crate) allocated: bool,
}

impl<'a, T, C: Context> Readable<'a, C> for SSlice<T> {
    fn read_from<R: Reader<'a, C>>(reader: &mut R) -> Result<Self, <C as Context>::Error> {
        let ptr = reader.read_u64()?;

        unsafe { Ok(SSlice::<T>::from_ptr(ptr, Side::Start).unwrap()) }
    }
}

impl<T, C: Context> Writable<C> for SSlice<T> {
    fn write_to<W: ?Sized + Writer<C>>(&self, writer: &mut W) -> Result<(), <C as Context>::Error> {
        writer.write_u64(self.ptr)
    }
}

impl<T> SSlice<T> {
    pub fn get_size_bytes(&self) -> usize {
        self.get_meta().0
    }

    pub fn get_total_size_bytes(&self) -> usize {
        self.get_size_bytes() + CELL_META_SIZE * 2
    }

    pub fn _write_bytes(&self, offset: usize, data: &[u8]) {
        let size = self.get_size_bytes();

        assert!(
            offset + data.len() <= size,
            "MemBox overflow (max {}, provided {})",
            size,
            offset + data.len()
        );

        stable::write(self.get_ptr() + (CELL_META_SIZE + offset) as u64, data);
    }

    pub fn _write_word(&self, offset: usize, word: u64) {
        let num = word.to_le_bytes();
        self._write_bytes(offset, &num);
    }

    pub fn _read_bytes(&self, offset: usize, data: &mut [u8]) {
        let size = self.get_size_bytes();

        assert!(
            data.len() + offset <= size,
            "MemBox overflow (max {}, provided {})",
            size,
            data.len() + offset
        );

        stable::read(self.get_ptr() + (CELL_META_SIZE + offset) as u64, data);
    }

    pub fn _read_word(&self, offset: usize) -> u64 {
        let mut buf = [0u8; PTR_SIZE];
        self._read_bytes(offset, &mut buf);

        u64::from_le_bytes(buf)
    }

    /// # Safety
    /// Make sure there are no duplicates of this `MemBox`, before creating.
    pub(crate) unsafe fn new(ptr: u64, size: usize, allocated: bool) -> Self {
        assert!(
            size >= CELL_MIN_SIZE,
            "Size lesser than {} ({})",
            CELL_MIN_SIZE,
            size
        );
        assert!(size < ALLOCATED, "Size is bigger than {} ({})", FREE, size);
        assert!(ptr < stable::size_pages() * PAGE_SIZE_BYTES as u64);

        Self::write_meta(ptr, size, allocated);

        Self {
            ptr,
            data: SPhantomData::default(),
            size,
            allocated,
        }
    }

    /// # Safety
    /// Make sure there no duplicates of this `MemBox`, before creation.
    pub(crate) unsafe fn new_total_size(ptr: u64, total_size: usize, allocated: bool) -> Self {
        Self::new(ptr, total_size - CELL_META_SIZE * 2, allocated)
    }

    /// # Safety
    /// This method may create a duplicate of the same underlying memory slice. Make sure, your logic
    /// doesn't do that.
    pub(crate) unsafe fn from_ptr(mut ptr: u64, side: Side) -> Option<Self> {
        if ptr >= stable::size_pages() * PAGE_SIZE_BYTES as u64 || ptr == EMPTY_PTR {
            return None;
        }

        let (size, allocated) = match side {
            Side::Start => {
                let (size, allocated) = Self::read_meta(ptr);
                if size < CELL_MIN_SIZE {
                    return None;
                }

                (size, allocated)
            }
            Side::End => {
                ptr -= CELL_META_SIZE as u64;
                let (size, allocated) = Self::read_meta(ptr);
                if size < CELL_MIN_SIZE {
                    return None;
                }

                if ptr < (size + CELL_META_SIZE) as u64 {
                    return None;
                }

                ptr -= (size + CELL_META_SIZE) as u64;

                (size, allocated)
            }
        };

        Some(Self {
            ptr,
            data: SPhantomData::default(),
            size,
            allocated,
        })
    }

    pub(crate) fn get_ptr(&self) -> u64 {
        self.ptr
    }

    pub(crate) fn get_meta(&self) -> (usize, bool) {
        (self.size, self.allocated)
    }

    pub(crate) fn set_allocated(&mut self, allocated: bool) {
        let (size, _) = self.get_meta();
        Self::write_meta(self.get_ptr(), size, allocated);
        self.allocated = allocated;
    }

    pub unsafe fn clone(&self) -> Self {
        Self::from_ptr(self.ptr, Side::Start).unwrap()
    }

    /// Splits this free `MemBox` into two new ones, if possible. The first one will have the provided size, the second
    /// one will have the rest (but not less than `min_size_second`. If size is not enough, returns
    /// `Err(self)`. Both new `MemBox`-es are free.
    ///
    /// # Safety
    /// Make sure there are no duplicates of this `MemBox` left before splitting.
    pub(crate) unsafe fn split(self, size_first: usize) -> Result<(Self, Self), Self> {
        assert!(
            size_first >= CELL_MIN_SIZE,
            "Size lesser than {} ({})",
            CELL_MIN_SIZE,
            size_first
        );

        let (size, allocated) = self.get_meta();
        self.assert_allocated(false, Some(allocated));

        if size < size_first + CELL_MIN_SIZE + CELL_META_SIZE * 2 {
            return Err(self);
        }

        let first = Self::new(self.get_ptr(), size_first, false);

        let size_second = size - size_first - CELL_META_SIZE * 2;

        let second = Self::new(first.get_next_neighbor_ptr(), size_second, false);

        Ok((first, second))
    }

    /// # Safety
    /// Make sure this MemBox and its neighbor are both have no duplicates, before merging.
    pub(crate) unsafe fn merge_with_neighbor(self, neighbor: Self) -> Self {
        let (self_size, self_allocated) = self.get_meta();
        self.assert_allocated(false, Some(self_allocated));

        let (neighbor_size, neighbor_allocated) = neighbor.get_meta();
        neighbor.assert_allocated(false, Some(neighbor_allocated));

        let self_ptr = self.get_ptr();
        let neighbor_ptr = neighbor.get_ptr();

        let n = if self_ptr > neighbor_ptr {
            self.get_neighbor(Side::Start).unwrap()
        } else {
            self.get_neighbor(Side::End).unwrap()
        };
        assert_eq!(n.get_ptr(), neighbor_ptr, "Not a neighbor");

        let ptr = if self_ptr > neighbor_ptr {
            neighbor_ptr
        } else {
            self_ptr
        };

        let size = self_size + neighbor_size + CELL_META_SIZE * 2;

        Self::new(ptr, size, false)
    }

    /// # Safety
    /// This method uses `MemBox::from_ptr()` under the hood. Follow its safety directions in order
    /// to do this right.
    pub(crate) unsafe fn get_neighbor(&self, side: Side) -> Option<Self> {
        match side {
            Side::Start => Self::from_ptr(self.get_ptr(), Side::End),
            Side::End => Self::from_ptr(self.get_next_neighbor_ptr(), Side::Start),
        }
    }

    pub(crate) fn get_next_neighbor_ptr(&self) -> u64 {
        self.get_ptr() + (CELL_META_SIZE * 2 + self.get_meta().0) as u64
    }

    pub(crate) fn assert_allocated(&self, expected: bool, val: Option<bool>) {
        let actual = match val {
            Some(v) => v,
            None => {
                let (_, is_allocated) = self.get_meta();
                is_allocated
            }
        };

        assert_eq!(actual, expected);
    }

    pub(crate) fn read_meta(ptr: u64) -> (usize, bool) {
        let mut meta = [0u8; CELL_META_SIZE as usize];
        stable::read(ptr, &mut meta);

        let encoded_size = usize::from_le_bytes(meta);
        let mut size = encoded_size;

        let allocated = if encoded_size & ALLOCATED == ALLOCATED {
            size &= FREE;
            true
        } else {
            false
        };

        (size, allocated)
    }

    fn write_meta(ptr: u64, size: usize, allocated: bool) {
        let encoded_size = if allocated {
            size | ALLOCATED
        } else {
            size & FREE
        };

        let meta = encoded_size.to_le_bytes();

        stable::write(ptr, &meta);
        stable::write(ptr + (CELL_META_SIZE + size) as u64, &meta);
    }
}

/// Only run these tests with `-- --test-threads=1`. It fails otherwise.
#[cfg(test)]
mod tests {
    use crate::primitive::s_slice::{Side, CELL_META_SIZE};
    use crate::utils::mem_context::stable;
    use crate::SSlice;

    #[test]
    fn creation_works_fine() {
        unsafe {
            stable::clear();
            stable::grow(10).expect("Unable to grow");

            let m1_size: usize = 100;
            let m2_size: usize = 200;
            let m3_size: usize = 300;

            let m1 = SSlice::<()>::new(0, m1_size, false);
            assert_eq!(m1.get_meta(), (m1_size, false));
            assert_eq!(
                m1.get_next_neighbor_ptr(),
                (0 + m1_size + CELL_META_SIZE * 2) as u64
            );

            let m2 = SSlice::<()>::new(m1.get_next_neighbor_ptr(), m2_size, true);
            assert_eq!(m2.get_meta(), (m2_size, true));
            assert_eq!(
                m2.get_next_neighbor_ptr(),
                m1.get_next_neighbor_ptr() + (m2_size + CELL_META_SIZE * 2) as u64
            );

            let m3 = SSlice::<()>::new(m2.get_next_neighbor_ptr(), m3_size, false);
            assert_eq!(m3.get_meta(), (m3_size, false));
            assert_eq!(
                m3.get_next_neighbor_ptr(),
                m2.get_next_neighbor_ptr() + (m3_size + CELL_META_SIZE * 2) as u64
            );

            let m1 = SSlice::<()>::from_ptr(0, Side::Start).unwrap();
            assert_eq!(m1.get_meta(), (m1_size, false));
            assert_eq!(
                m1.get_next_neighbor_ptr(),
                0 + (m1_size + CELL_META_SIZE * 2) as u64
            );

            let m1 = SSlice::<()>::from_ptr(m1.get_next_neighbor_ptr(), Side::End).unwrap();
            assert_eq!(m1.get_meta(), (m1_size, false));
            assert_eq!(
                m1.get_next_neighbor_ptr(),
                0 + (m1_size + CELL_META_SIZE * 2) as u64
            );

            let m2 = SSlice::<()>::from_ptr(m1.get_next_neighbor_ptr(), Side::Start).unwrap();
            assert_eq!(m2.get_meta(), (m2_size, true));
            assert_eq!(
                m2.get_next_neighbor_ptr(),
                m1.get_next_neighbor_ptr() + (m2_size + CELL_META_SIZE * 2) as u64
            );

            let m2 = SSlice::<()>::from_ptr(m2.get_next_neighbor_ptr(), Side::End).unwrap();
            assert_eq!(m2.get_meta(), (m2_size, true));
            assert_eq!(
                m2.get_next_neighbor_ptr(),
                m1.get_next_neighbor_ptr() + (m2_size + CELL_META_SIZE * 2) as u64
            );

            let m3 = SSlice::<()>::from_ptr(m2.get_next_neighbor_ptr(), Side::Start).unwrap();
            assert_eq!(m3.get_meta(), (m3_size, false));
            assert_eq!(
                m3.get_next_neighbor_ptr(),
                m2.get_next_neighbor_ptr() + (m3_size + CELL_META_SIZE * 2) as u64
            );

            let m3 = SSlice::<()>::from_ptr(m3.get_next_neighbor_ptr(), Side::End).unwrap();
            assert_eq!(m3.get_meta(), (m3_size, false));
            assert_eq!(
                m3.get_next_neighbor_ptr(),
                m2.get_next_neighbor_ptr() + (m3_size + CELL_META_SIZE * 2) as u64
            );
        }
    }

    #[test]
    fn split_merge_work_fine() {
        unsafe {
            stable::clear();
            stable::grow(10).expect("Unable to grow");

            let m1_size: usize = 100;
            let m2_size: usize = 200;
            let m3_size: usize = 300;

            let m1 = SSlice::<()>::new(0, m1_size, false);
            let m2 = SSlice::<()>::new(m1.get_next_neighbor_ptr(), m2_size, false);
            let m3 = SSlice::<()>::new(m2.get_next_neighbor_ptr(), m3_size, false);

            let initial_m3_next_ptr = m3.get_next_neighbor_ptr();

            let (m3, m4) = m3.split(100).ok().unwrap();
            assert_eq!(m3.get_meta(), (100, false));
            assert_eq!(m3.get_next_neighbor_ptr(), m4.get_ptr());

            assert_eq!(m4.get_meta(), (m3_size - 100 - 2 * CELL_META_SIZE, false));
            assert_eq!(m4.get_next_neighbor_ptr(), initial_m3_next_ptr);

            let m3 = m4.merge_with_neighbor(m3);
            assert_eq!(m3.get_meta(), (m3_size, false));
            assert_eq!(m3.get_next_neighbor_ptr(), initial_m3_next_ptr);

            let m2 = m2.merge_with_neighbor(m3);
            assert_eq!(
                m2.get_meta(),
                (m2_size + m3_size + 2 * CELL_META_SIZE, false)
            );
            assert_eq!(m2.get_next_neighbor_ptr(), initial_m3_next_ptr);

            let m1 = m2.merge_with_neighbor(m1);
            assert_eq!(
                m1.get_meta(),
                (m1_size + m2_size + m3_size + 4 * CELL_META_SIZE, false)
            );
            assert_eq!(m1.get_next_neighbor_ptr(), initial_m3_next_ptr);

            let (m1, m2) = m1.split(m1_size).ok().unwrap();
            assert_eq!(m1.get_meta(), (m1_size, false));
            assert_eq!(
                m2.get_meta(),
                (m2_size + m3_size + 2 * CELL_META_SIZE, false)
            );
            assert_eq!(m1.get_next_neighbor_ptr(), m2.get_ptr());
            assert_eq!(m2.get_next_neighbor_ptr(), initial_m3_next_ptr);

            let (m2, m3) = m2.split(m2_size).ok().unwrap();
            assert_eq!(m2.get_meta(), (m2_size, false));
            assert_eq!(m3.get_meta(), (m3_size, false));
            assert_eq!(m2.get_next_neighbor_ptr(), m3.get_ptr());
            assert_eq!(m3.get_next_neighbor_ptr(), initial_m3_next_ptr);
        }
    }

    #[test]
    fn read_write_work_fine() {
        unsafe {
            stable::clear();
            stable::grow(10).expect("Unable to grow");

            let m1 = SSlice::<()>::new(0, 100, true);

            let a = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
            let b = vec![1u8, 3, 3, 7];
            let c = vec![9u8, 8, 7, 6, 5, 4, 3, 2, 1];

            m1._write_bytes(0, &a);
            m1._write_bytes(8, &b);
            m1._write_bytes(90, &c);

            let mut a1 = [0u8; 8];
            let mut b1 = [0u8; 4];
            let mut c1 = [0u8; 9];

            m1._read_bytes(0, &mut a1);
            m1._read_bytes(8, &mut b1);
            m1._read_bytes(90, &mut c1);

            assert_eq!(&a, &a1);
            assert_eq!(&b, &b1);
            assert_eq!(&c, &c1);
        }
    }
}
