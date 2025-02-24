use crate::primitive::s_slice::Side;
use crate::{allocate, deallocate, reallocate, SSlice};
use speedy::{LittleEndian, Readable, Writable};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};

#[derive(Readable, Writable)]
pub struct SUnsafeCell<T> {
    pub(crate) slice: SSlice<T>,
    #[speedy(skip)]
    pub(crate) buf: RefCell<Option<Vec<u8>>>,
}

impl<'a, T: Readable<'a, LittleEndian> + Writable<LittleEndian>> SUnsafeCell<T> {
    pub fn new(it: &T) -> Self {
        let buf = it.write_to_vec().expect("Unable to encode");
        let slice = allocate(buf.len());

        slice._write_bytes(0, &buf);

        Self {
            slice,
            buf: RefCell::new(Some(buf)),
        }
    }

    pub fn get_cloned(&self) -> T {
        {
            if let Some(buf) = &*self.buf.borrow() {
                return T::read_from_buffer_copying_data(buf).expect("Unable to decode");
            }
        }

        let mut buf = vec![0u8; self._allocated_size()];
        self.slice._read_bytes(0, &mut buf);

        let res = T::read_from_buffer_copying_data(&buf).expect("Unable to decode");
        *self.buf.borrow_mut() = Some(buf);

        res
    }

    /// # Safety
    /// Make sure you update all references pointing to this sbox after setting a new value to it.
    /// Set can cause a reallocation that will change the location of the data.
    /// Use the return bool value to determine if the location is changed (true = you need to update).
    pub unsafe fn set(&mut self, it: &T) -> bool {
        let buf = it.write_to_vec().expect("Unable to encode");
        let mut res = false;

        if self._allocated_size() < buf.len() {
            self.slice = reallocate(self.slice.clone(), buf.len());
            res = true;
        }

        self.slice._write_bytes(0, &buf);
        *self.buf.borrow_mut() = Some(buf);

        res
    }

    pub fn _allocated_size(&self) -> usize {
        self.slice.get_size_bytes()
    }

    pub unsafe fn from_ptr(ptr: u64) -> Self {
        assert_ne!(ptr, 0);

        let slice = SSlice::from_ptr(ptr, Side::Start).unwrap();

        Self {
            slice,
            buf: RefCell::new(None),
        }
    }

    pub unsafe fn as_ptr(&self) -> u64 {
        self.slice.ptr
    }

    pub fn drop(self) {
        deallocate(self.slice)
    }
}

impl<'a, T: Eq + Readable<'a, LittleEndian> + Writable<LittleEndian>> PartialEq<Self>
    for SUnsafeCell<T>
{
    fn eq(&self, other: &Self) -> bool {
        self.get_cloned().eq(&other.get_cloned())
    }
}

impl<'a, T: Eq + Readable<'a, LittleEndian> + Writable<LittleEndian>> Eq for SUnsafeCell<T> {}

impl<'a, T: Ord + Readable<'a, LittleEndian> + Writable<LittleEndian>> PartialOrd<Self>
    for SUnsafeCell<T>
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.get_cloned().partial_cmp(&other.get_cloned())
    }
}

impl<'a, T: Ord + Readable<'a, LittleEndian> + Writable<LittleEndian>> Ord for SUnsafeCell<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.get_cloned().cmp(&other.get_cloned())
    }

    fn max(self, other: Self) -> Self
    where
        Self: Sized,
    {
        let self_val = self.get_cloned();
        let other_val = other.get_cloned();

        if other_val > self_val {
            other
        } else {
            self
        }
    }

    fn min(self, other: Self) -> Self
    where
        Self: Sized,
    {
        let self_val = self.get_cloned();
        let other_val = other.get_cloned();

        if other_val < self_val {
            other
        } else {
            self
        }
    }

    fn clamp(self, min: Self, max: Self) -> Self
    where
        Self: Sized,
    {
        let self_val = self.get_cloned();
        let min_val = min.get_cloned();
        if min_val > self_val {
            return min;
        }

        let max_val = max.get_cloned();
        if max_val < self_val {
            return max;
        }

        self
    }
}

impl<'a, T: Hash + Readable<'a, LittleEndian> + Writable<LittleEndian>> Hash for SUnsafeCell<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get_cloned().hash(state)
    }
}

impl<'a, T: Debug + Readable<'a, LittleEndian> + Writable<LittleEndian>> Debug for SUnsafeCell<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.get_cloned().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use crate::init_allocator;
    use crate::primitive::s_unsafe_cell::SUnsafeCell;
    use crate::utils::mem_context::stable;
    use speedy::{Readable, Writable};

    #[derive(Readable, Writable, Debug, PartialEq, Eq)]
    struct Test {
        pub a: u128,
        pub b: String,
    }

    #[test]
    fn candid_membox_works_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let obj = Test {
            a: 12341231231,
            b: String::from("The string"),
        };

        let membox = SUnsafeCell::new(&obj);
        let obj1 = membox.get_cloned();

        assert_eq!(obj, obj1);
    }
}
