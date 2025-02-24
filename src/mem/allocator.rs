use crate::primitive::s_slice::{Side, CELL_MIN_SIZE, PTR_SIZE};
use crate::utils::math::fast_log2;
use crate::utils::mem_context::{stable, OutOfMemory, PAGE_SIZE_BYTES};
use crate::SSlice;
use ic_cdk::api::call::call_raw;
use ic_cdk::{id, print, spawn, trap};
use std::fmt::{Debug, Formatter};
use std::usize;

pub(crate) const EMPTY_PTR: u64 = u64::MAX;
pub(crate) const MAGIC: [u8; 4] = [b'S', b'M', b'A', b'M'];
pub(crate) const SEG_CLASS_PTRS_COUNT: u32 = usize::BITS - 4;
pub(crate) const CUSTOM_DATA_PTRS_COUNT: usize = 4;
pub(crate) const DEFAULT_MAX_ALLOCATION_PAGES: u32 = 180; // 180 * 64k = ~10MB
pub(crate) const DEFAULT_MAX_GROW_PAGES: u64 = 0;
pub(crate) const LOW_ON_MEMORY_HOOK_NAME: &str = "on_low_stable_memory";

pub(crate) type SegClassId = u32;

#[derive(Debug, Copy, Clone)]
pub(crate) struct StableMemoryAllocator;

impl SSlice<StableMemoryAllocator> {
    const SIZE: usize = MAGIC.len()                     // magic bytes
        + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE      // segregations classes table
        + PTR_SIZE * 2                                  // free & allocated counters
        + PTR_SIZE                                      // max allocation size
        + 1                                             // was on_low_stable_memory() callback executed flag
        + PTR_SIZE                                      // max grow pages
        + CUSTOM_DATA_PTRS_COUNT * PTR_SIZE; // pointers to custom data

    /// # Safety
    /// Invoke only once during `init()` canister function execution
    /// Execution more than once will lead to undefined behavior
    pub(crate) unsafe fn init(offset: u64) -> Self {
        let mut allocator = SSlice::<StableMemoryAllocator>::new(offset, Self::SIZE, true);

        allocator._write_bytes(0, &MAGIC);
        allocator.reset();

        allocator
    }

    /// # Safety
    /// Invoke each time your canister upgrades, in `post_upgrade()` function
    /// It's fine to call this function more than once, but remember that using multiple copies of
    /// a single allocator can lead to race condition in an asynchronous scenario
    pub(crate) unsafe fn reinit(offset: u64) -> Option<Self> {
        let membox = SSlice::<StableMemoryAllocator>::from_ptr(offset, Side::Start)?;
        let (size, allocated) = membox.get_meta();
        if !allocated || size != Self::SIZE {
            return None;
        }

        let mut magic = [0u8; MAGIC.len()];
        membox._read_bytes(0, &mut magic);
        if magic != MAGIC {
            return None;
        }

        Some(membox)
    }

    pub(crate) fn allocate<T>(&mut self, mut size: usize) -> SSlice<T> {
        if size < CELL_MIN_SIZE {
            size = CELL_MIN_SIZE
        }

        // will be called only once during first ever allocate()
        self.handle_free_buffer();

        let free_membox = match self.pop_allocated_membox(size) {
            Ok(m) => m,
            Err(_) => trap(format!("Not enough stable memory to allocate {} more bytes. Grown: {} bytes; Allocated: {} bytes; Free: {} bytes", size, stable::size_pages() * PAGE_SIZE_BYTES as u64, self.get_allocated_size(), self.get_free_size()).as_str())
        };

        self.handle_free_buffer();

        let it = unsafe {
            // shouldn't throw, since the membox was just allocated and therefore operable
            SSlice::<T>::from_ptr(free_membox.get_ptr(), Side::Start).unwrap()
        };

        let buf = vec![0u8; it.get_size_bytes()];
        it._write_bytes(0, &buf);

        it
    }

    pub(crate) fn deallocate<T>(&mut self, mut membox: SSlice<T>) {
        let (_, allocated) = membox.get_meta();
        membox.assert_allocated(true, Some(allocated));
        membox.set_allocated(false);

        let total_allocated = self.get_allocated_size();
        self.set_allocated_size(total_allocated - membox.get_total_size_bytes() as u64);

        let membox = unsafe { SSlice::<Free>::from_ptr(membox.get_ptr(), Side::Start).unwrap() };
        self.push_free_membox(membox);
    }

    // TODO: reallocate inplace

    pub(crate) fn reallocate<T>(&mut self, membox: SSlice<T>, new_size: usize) -> SSlice<T> {
        let mut data = vec![0u8; membox.get_size_bytes()];
        membox._read_bytes(0, &mut data);

        self.deallocate(membox);
        let new_membox = self.allocate(new_size);
        new_membox._write_bytes(0, &data);

        new_membox
    }

    pub(crate) fn reset(&mut self) {
        let empty_ptr_bytes = EMPTY_PTR.to_le_bytes();

        for i in 0..(SEG_CLASS_PTRS_COUNT as usize + CUSTOM_DATA_PTRS_COUNT) {
            self._write_bytes(MAGIC.len() + i * PTR_SIZE, &empty_ptr_bytes)
        }

        self.set_allocated_size(0);
        self.set_free_size(0);
        self.set_max_allocation_pages(DEFAULT_MAX_ALLOCATION_PAGES);
        self.set_max_grow_pages(DEFAULT_MAX_GROW_PAGES);
        self.set_on_low_executed_flag(false);

        let total_free_size =
            stable::size_pages() * PAGE_SIZE_BYTES as u64 - self.get_next_neighbor_ptr();

        if total_free_size > 0 {
            let ptr = self.get_next_neighbor_ptr();

            let free_mem_box =
                unsafe { SSlice::<Free>::new_total_size(ptr, total_free_size as usize, false) };

            self.push_free_membox(free_mem_box);
        }
    }

    fn push_free_membox(&mut self, mut membox: SSlice<Free>) {
        membox.assert_allocated(false, None);

        membox = self.maybe_merge_with_free_neighbors(membox);

        let total_free = self.get_free_size();
        self.set_free_size(total_free + membox.get_total_size_bytes() as u64);

        let (size, _) = membox.get_meta();
        let seg_class_id = get_seg_class_id(size);
        let head_opt = unsafe { self.get_seg_class_head(seg_class_id) };

        self.set_seg_class_head(seg_class_id, membox.get_ptr());
        membox.set_prev_free_ptr(self.get_ptr());

        match head_opt {
            None => {
                membox.set_next_free_ptr(EMPTY_PTR);
            }
            Some(mut head) => {
                membox.set_next_free_ptr(head.get_ptr());

                head.set_prev_free_ptr(membox.get_ptr());
            }
        }
    }

    /// returns ALLOCATED membox
    fn pop_allocated_membox(&mut self, size: usize) -> Result<SSlice<Free>, OutOfMemory> {
        let mut seg_class_id = get_seg_class_id(size);
        let free_membox_opt = unsafe { self.get_seg_class_head(seg_class_id) };

        // iterate over this seg class, until big enough membox found or til it ends
        if let Some(mut free_membox) = free_membox_opt {
            loop {
                let membox_size = free_membox.get_size_bytes();

                // if valid membox found,
                if membox_size >= size {
                    self.eject_from_freelist(seg_class_id, &mut free_membox);

                    let total_allocated = self.get_allocated_size();
                    self.set_allocated_size(
                        total_allocated + free_membox.get_total_size_bytes() as u64,
                    );

                    free_membox.set_allocated(true);

                    return Ok(free_membox);
                }

                let next_ptr = free_membox.get_next_free_ptr();
                if next_ptr == EMPTY_PTR {
                    break;
                }

                free_membox = unsafe { SSlice::<Free>::from_ptr(next_ptr, Side::Start).unwrap() };
            }
        }

        // if no appropriate membox was found previously, try to find any of larger size
        let mut free_membox_opt = None;
        seg_class_id += 1;

        while seg_class_id < SEG_CLASS_PTRS_COUNT as u32 {
            free_membox_opt = unsafe { self.get_seg_class_head(seg_class_id) };

            if let Some(free_membox) = &free_membox_opt {
                if free_membox.get_size_bytes() >= size {
                    break;
                }
            }

            seg_class_id += 1;
        }

        match free_membox_opt {
            // if at least one such a big membox found, pop it, split in two, take first, push second
            Some(mut free_membox) => {
                self.eject_from_freelist(seg_class_id, &mut free_membox);

                let res = unsafe { free_membox.split(size) };
                match res {
                    Ok((mut result, additional)) => {
                        result.set_allocated(true);
                        self.push_free_membox(additional);

                        let total_allocated = self.get_allocated_size();
                        self.set_allocated_size(
                            total_allocated + result.get_total_size_bytes() as u64,
                        );

                        Ok(result)
                    }
                    Err(mut result) => {
                        result.set_allocated(true);

                        let total_allocated = self.get_allocated_size();
                        self.set_allocated_size(
                            total_allocated + result.get_total_size_bytes() as u64,
                        );

                        Ok(result)
                    }
                }
            }
            // otherwise, throw (max allocation size limit violated)
            None => Err(OutOfMemory),
        }
    }

    pub(crate) fn get_allocated_size(&self) -> u64 {
        self._read_word(MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE)
    }

    fn set_allocated_size(&mut self, size: u64) {
        self._write_word(MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE, size);
    }

    pub(crate) fn get_free_size(&self) -> u64 {
        self._read_word(MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE)
    }

    fn set_free_size(&mut self, size: u64) {
        self._write_word(
            MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE,
            size,
        );
    }

    pub(crate) fn get_max_allocation_pages(&self) -> u32 {
        self._read_word(MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE * 2)
            as u32
    }

    pub(crate) fn set_max_allocation_pages(&mut self, pages: u32) {
        self._write_word(
            MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE * 2,
            pages as u64,
        );
    }

    pub(crate) fn get_on_low_executed_flag(&self) -> bool {
        let mut buf = [0u8; 1];
        self._read_bytes(
            MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE * 2 + 1,
            &mut buf,
        );

        buf[0] == 1
    }

    pub(crate) fn set_on_low_executed_flag(&mut self, flag: bool) {
        let buf = if flag { [1u8; 1] } else { [0u8; 1] };

        self._write_bytes(
            MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE * 2 + 1,
            &buf,
        );
    }

    pub(crate) fn get_max_grow_pages(&self) -> u64 {
        self._read_word(MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE * 3 + 1)
    }

    pub(crate) fn set_max_grow_pages(&mut self, max_pages: u64) {
        self._write_word(
            MAGIC.len() + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE + PTR_SIZE * 3 + 1,
            max_pages,
        );
    }

    pub fn set_custom_data_ptr(&mut self, idx: usize, ptr: u64) {
        assert!(idx < CUSTOM_DATA_PTRS_COUNT);

        self._write_word(
            MAGIC.len()
                + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE
                + PTR_SIZE * 4
                + 1
                + idx * PTR_SIZE,
            ptr,
        );
    }

    pub fn get_custom_data_ptr(&mut self, idx: usize) -> u64 {
        assert!(idx < CUSTOM_DATA_PTRS_COUNT);

        self._read_word(
            MAGIC.len()
                + SEG_CLASS_PTRS_COUNT as usize * PTR_SIZE
                + PTR_SIZE * 4
                + 1
                + idx * PTR_SIZE,
        )
    }

    unsafe fn get_seg_class_head(&self, id: SegClassId) -> Option<SSlice<Free>> {
        let ptr = self._read_word(Self::get_seg_class_head_offset(id));
        if ptr == EMPTY_PTR {
            return None;
        }

        Some(SSlice::<Free>::from_ptr(ptr, Side::Start).unwrap())
    }

    fn eject_from_freelist(&mut self, seg_class_id: SegClassId, membox: &mut SSlice<Free>) {
        // if membox is the head of it's seg class
        if membox.get_prev_free_ptr() == self.get_ptr() {
            self.set_seg_class_head(seg_class_id, membox.get_next_free_ptr());

            let next_opt =
                unsafe { SSlice::<Free>::from_ptr(membox.get_next_free_ptr(), Side::Start) };

            if let Some(mut next) = next_opt {
                next.set_prev_free_ptr(self.get_ptr());
            }
        } else {
            let mut prev = unsafe {
                SSlice::<Free>::from_ptr(membox.get_prev_free_ptr(), Side::Start).unwrap()
            };
            let next_opt =
                unsafe { SSlice::<Free>::from_ptr(membox.get_next_free_ptr(), Side::Start) };

            if let Some(mut next) = next_opt {
                prev.set_next_free_ptr(next.get_ptr());
                next.set_prev_free_ptr(prev.get_ptr());
            } else {
                prev.set_next_free_ptr(EMPTY_PTR);
            }
        }

        let total_free = self.get_free_size();
        self.set_free_size(total_free - membox.get_total_size_bytes() as u64);

        membox.set_prev_free_ptr(EMPTY_PTR);
        membox.set_next_free_ptr(EMPTY_PTR);
    }

    fn maybe_merge_with_free_neighbors(&mut self, mut membox: SSlice<Free>) -> SSlice<Free> {
        let prev_neighbor_opt = unsafe { membox.get_neighbor(Side::Start) };
        membox = if let Some(mut prev_neighbor) = prev_neighbor_opt {
            let (neighbor_size, neighbor_allocated) = prev_neighbor.get_meta();

            if !neighbor_allocated {
                let seg_class_id = get_seg_class_id(neighbor_size);
                self.eject_from_freelist(seg_class_id, &mut prev_neighbor);

                unsafe { membox.merge_with_neighbor(prev_neighbor) }
            } else {
                membox
            }
        } else {
            membox
        };

        let next_neighbor_opt = unsafe { membox.get_neighbor(Side::End) };
        membox = if let Some(mut next_neighbor) = next_neighbor_opt {
            let (neighbor_size, neighbor_allocated) = next_neighbor.get_meta();

            if !neighbor_allocated {
                let seg_class_id = get_seg_class_id(neighbor_size);
                self.eject_from_freelist(seg_class_id, &mut next_neighbor);

                unsafe { membox.merge_with_neighbor(next_neighbor) }
            } else {
                membox
            }
        } else {
            membox
        };

        membox
    }

    // makes sure the allocator always has at least X bytes of free memory, tries to grow otherwise
    fn handle_free_buffer(&mut self) {
        let free = self.get_free_size();
        let max_allocation_size = self.get_max_allocation_pages() as u64;

        if free >= max_allocation_size * PAGE_SIZE_BYTES as u64 {
            return;
        }

        let pages_to_grow = max_allocation_size - free / PAGE_SIZE_BYTES as u64 + 1;

        if let Some(prev_pages) = self.grow_or_trigger_low_memory_hook(pages_to_grow) {
            let ptr = prev_pages * PAGE_SIZE_BYTES as u64;
            let new_memory_size = stable::size_pages() * PAGE_SIZE_BYTES as u64 - ptr;

            let new_free_membox =
                unsafe { SSlice::<Free>::new_total_size(ptr, new_memory_size as usize, false) };

            self.push_free_membox(new_free_membox);
        }
    }

    fn grow_or_trigger_low_memory_hook(&mut self, pages_to_grow: u64) -> Option<u64> {
        let already_grew = stable::size_pages();
        let max_grow_pages = self.get_max_grow_pages();

        if max_grow_pages != 0 && already_grew + pages_to_grow >= max_grow_pages {
            self.handle_low_memory();

            return None;
        }

        match stable::grow(pages_to_grow) {
            Ok(prev_pages) => Some(prev_pages),
            Err(_) => {
                self.handle_low_memory();

                None
            }
        }
    }

    fn handle_low_memory(&mut self) {
        if self.get_on_low_executed_flag() {
            return;
        }

        print(
            format!(
                "Low on stable memory, triggering {}()...",
                LOW_ON_MEMORY_HOOK_NAME
            )
            .as_str(),
        );

        spawn(async {
            call_raw(id(), LOW_ON_MEMORY_HOOK_NAME, &EMPTY_ARGS, 0)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "Unable to trigger {}(), failing silently...",
                        LOW_ON_MEMORY_HOOK_NAME
                    )
                });
        });

        self.set_on_low_executed_flag(true);
    }

    fn set_seg_class_head(&mut self, id: SegClassId, head_ptr: u64) {
        self._write_word(Self::get_seg_class_head_offset(id), head_ptr);
    }

    fn get_seg_class_head_offset(seg_class_id: SegClassId) -> usize {
        assert!(seg_class_id < SEG_CLASS_PTRS_COUNT as SegClassId);

        MAGIC.len() + seg_class_id as usize * PTR_SIZE
    }
}

const EMPTY_ARGS: [u8; 6] = [b'D', b'I', b'D', b'L', 0, 0];

fn get_seg_class_id(size: usize) -> SegClassId {
    let mut log = fast_log2(size);

    if 2usize.pow(log) < size {
        log += 1;
    }

    if log > 3 {
        (log - 4) as SegClassId
    } else {
        0
    }
}

impl Debug for SSlice<StableMemoryAllocator> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("StableMemoryAllocator");

        d.field("total_allocated", &self.get_allocated_size())
            .field("total_free", &self.get_free_size())
            .field("max_allocation_size", &self.get_max_allocation_pages())
            .field("max_grow_pages", &self.get_max_grow_pages());

        for id in 0..SEG_CLASS_PTRS_COUNT as u32 {
            let head = unsafe { self.get_seg_class_head(id) };
            let mut seg_class = vec![];

            match head {
                None => seg_class.push(String::from("EMPTY")),
                Some(mut membox) => {
                    seg_class.push(format!("{:?}", membox));

                    let mut next_ptr = membox.get_next_free_ptr();
                    while next_ptr != EMPTY_PTR {
                        membox = unsafe {
                            SSlice::from_ptr(membox.get_next_free_ptr(), Side::Start).unwrap()
                        };
                        seg_class.push(format!("{:?}", membox));
                        next_ptr = membox.get_next_free_ptr();
                    }
                }
            }

            d.field(format!("up to 2**{}", id + 4).as_str(), &seg_class);
        }

        d.finish()
    }
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct Free;

impl SSlice<Free> {
    pub(crate) fn set_prev_free_ptr(&mut self, prev_ptr: u64) {
        self.assert_allocated(false, None);

        self._write_word(0, prev_ptr);
    }

    pub(crate) fn get_prev_free_ptr(&self) -> u64 {
        self.assert_allocated(false, None);

        self._read_word(0)
    }

    pub(crate) fn set_next_free_ptr(&mut self, next_ptr: u64) {
        self.assert_allocated(false, None);

        self._write_word(PTR_SIZE, next_ptr);
    }

    pub(crate) fn get_next_free_ptr(&self) -> u64 {
        self.assert_allocated(false, None);

        self._read_word(PTR_SIZE)
    }
}

impl Debug for SSlice<Free> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let (size, allocated) = self.get_meta();

        let prev_ptr = self.get_next_free_ptr();
        let prev = if prev_ptr == EMPTY_PTR {
            String::from("EMPTY")
        } else {
            prev_ptr.to_string()
        };

        let next_ptr = self.get_next_free_ptr();
        let next = if next_ptr == EMPTY_PTR {
            String::from("EMPTY")
        } else {
            next_ptr.to_string()
        };

        f.debug_struct("FreeMemBox")
            .field("ptr", &self.get_ptr())
            .field("size", &size)
            .field("is_allocated", &allocated)
            .field("prev_free", &prev)
            .field("next_free", &next)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::mem::allocator::SEG_CLASS_PTRS_COUNT;
    use crate::utils::mem_context::stable;
    use crate::{SSlice, StableMemoryAllocator};

    #[test]
    fn initialization_works_fine() {
        stable::clear();
        stable::grow(1).expect("Unable to grow");

        unsafe {
            let sma = SSlice::<StableMemoryAllocator>::init(0);
            let free_memboxes: Vec<_> = (0..SEG_CLASS_PTRS_COUNT)
                .filter_map(|it| sma.get_seg_class_head(it as u32))
                .collect();

            assert_eq!(free_memboxes.len(), 1);
            let free_membox1 = free_memboxes[0].clone();
            let (size1, allocated1) = free_membox1.get_meta();

            let sma = SSlice::<StableMemoryAllocator>::reinit(0).unwrap();
            let free_memboxes: Vec<_> = (0..SEG_CLASS_PTRS_COUNT)
                .filter_map(|it| sma.get_seg_class_head(it as u32))
                .collect();

            assert_eq!(free_memboxes.len(), 1);
            let free_membox2 = free_memboxes[0].clone();
            let (size2, allocated2) = free_membox2.get_meta();

            assert_eq!(size1, size2);
            assert_eq!(allocated1, allocated2);
        }
    }

    #[test]
    fn allocation_works_fine() {
        stable::clear();
        stable::grow(1).expect("Unable to grow");

        unsafe {
            let mut sma = SSlice::<StableMemoryAllocator>::init(0);
            sma.set_max_grow_pages(0);

            let mut memboxes = vec![];

            // try to allocate 1000 MB
            for i in 0..1024 {
                let membox = sma.allocate::<u8>(1024);

                assert!(membox.get_meta().0 >= 1024, "Invalid membox size at {}", i);

                memboxes.push(membox);
            }

            assert!(sma.get_allocated_size() >= 1024 * 1024);

            for i in 0..1024 {
                let mut membox = memboxes[i].clone();
                membox = sma.reallocate(membox, 2 * 1024);

                assert!(
                    membox.get_meta().0 >= 2 * 1024,
                    "Invalid membox size at {}",
                    i
                );

                memboxes[i] = membox;
            }

            assert!(sma.get_allocated_size() >= 2 * 1024 * 1024);

            for i in 0..1024 {
                let membox = memboxes[i].clone();
                sma.deallocate(membox);
            }

            assert_eq!(sma.get_allocated_size(), 0);
        }
    }
}
