use super::{ActivePageTable, Page, PhysicalAddress, VirtualAddress, ENTRY_COUNT};
use super::entry::EntryFlags;
use super::table::{self, Level4, Table};
use arch::memory::{allocate_frames, Frame, PAGE_SIZE};
use core::ptr::Unique;
use core::mem;

/// A helper struct which does most of the paging gruntwork.
pub struct Mapper {
    p4: Unique<Table<Level4>>,
}

impl Mapper {
    pub unsafe fn new() -> Mapper {
        Mapper {
            p4: Unique::new_unchecked(table::P4),
        }
    }

    pub fn p4(&self) -> &Table<Level4> {
        unsafe { self.p4.as_ref() }
    }

    pub fn p4_mut(&mut self) -> &mut Table<Level4> {
        unsafe { self.p4.as_mut() }
    }

    /// Translate a virtual address to a physical address.
    pub fn translate(&self, virtual_address: VirtualAddress) -> Option<PhysicalAddress> {
        let offset = virtual_address.get() % PAGE_SIZE;
        self.translate_page(Page::containing_address(virtual_address))
            .map(|frame| PhysicalAddress::new(frame.number * PAGE_SIZE + offset))
    }

    /// Walk the page tables to find the physical frame that a passed `page` is mapped to.
    pub fn translate_page(&self, page: Page) -> Option<Frame> {
        // Get reference to the P3 table.
        let p3 = self.p4().next_table(page.p4_index());

        // Check if this page is a huge page.
        let huge_page = || {
            p3.and_then(|p3| {
                let p3_entry = &p3[page.p3_index()];
                // 1GiB page?
                if let Some(start_frame) = p3_entry.pointed_frame() {
                    if p3_entry.flags().contains(EntryFlags::HUGE_PAGE) {
                        // address must be 1GiB aligned
                        assert!(start_frame.number % (ENTRY_COUNT * ENTRY_COUNT) == 0);
                        return Some(Frame {
                            number: start_frame.number + page.p2_index() * ENTRY_COUNT
                                + page.p1_index(),
                        });
                    }
                }
                if let Some(p2) = p3.next_table(page.p3_index()) {
                    let p2_entry = &p2[page.p2_index()];
                    // 2MiB page?
                    if let Some(start_frame) = p2_entry.pointed_frame() {
                        if p2_entry.flags().contains(EntryFlags::HUGE_PAGE) {
                            // address must be 2MiB aligned
                            assert!(start_frame.number % ENTRY_COUNT == 0);
                            return Some(Frame {
                                number: start_frame.number + page.p1_index(),
                            });
                        }
                    }
                }
                None
            })
        };

        p3.and_then(|p3| p3.next_table(page.p3_index()))
            .and_then(|p2| p2.next_table(page.p2_index()))
            .and_then(|p1| p1[page.p1_index()].pointed_frame())
            .or_else(huge_page)
    }

    /// Map a page to a frame by getting reference to the page tables and setting the index in the
    /// P1 table to the given frame.
    pub fn map_to(&mut self, page: Page, frame: Frame, flags: EntryFlags) -> MapperFlush {
        let p3 = self.p4_mut().next_table_create(page.p4_index());
        let p2 = p3.next_table_create(page.p3_index());
        let p1 = p2.next_table_create(page.p2_index());

        assert!(p1[page.p1_index()].is_unused());
        p1[page.p1_index()].set(frame, flags | EntryFlags::PRESENT);

        MapperFlush::new(page)
    }

    /// Map a page by allocating a free frame and mapping a page to that frame.
    pub fn map(&mut self, page: Page, flags: EntryFlags) -> MapperFlush {
        let frame = allocate_frames(1).expect("out of memory");
        self.map_to(page, frame, flags)
    }

    /// Map a page by translating a given `Frame` to a `Page`.
    pub fn identity_map(&mut self, frame: Frame, flags: EntryFlags) -> MapperFlush {
        let page = Page::containing_address(VirtualAddress::new(frame.start_address().get()));
        self.map_to(page, frame, flags)
    }

    /// Unmap a page from a physical frame.
    pub fn unmap(&mut self, page: Page) -> MapperFlush {
        use x86_64;
        use x86_64::instructions::tlb;

        // Check if the page is already unmapped (page not mapped to frame, translation failed).
        assert!(self.translate(page.start_address()).is_some());

        let p1 = self.p4_mut()
            .next_table_mut(page.p4_index())
            .and_then(|p3| p3.next_table_mut(page.p3_index()))
            .and_then(|p2| p2.next_table_mut(page.p2_index()))
            .expect("mapping code does not support huge pages");
        let _frame = p1[page.p1_index()].pointed_frame().unwrap();
        p1[page.p1_index()].set_unused();
        tlb::flush(x86_64::VirtualAddress(page.start_address().get()));
        // TODO free p(1,2,3) table if empty
        // allocator.deallocate_frame(frame);
        MapperFlush::new(page)
    }
}

/// A promise to flush a virtual address.
#[must_use = "The page must be flushed, or the changes are ignored."]
pub struct MapperFlush(Page);

impl Drop for MapperFlush {
    fn drop(&mut self) {
        panic!("Flush not consumed!");
    }
}

impl MapperFlush {
    pub fn new(page: Page) -> Self {
        MapperFlush(page)
    }

    pub fn flush(self, table: &mut ActivePageTable) {
        table.flush(self.0);
        mem::forget(self);
    }

    pub unsafe fn ignore(self) {
        mem::forget(self);
    }
}

/// A way to flush the entire active page table.
#[must_use = "The active page table must be flushed, or the changes ignored"]
pub struct MapperFlushAll(bool);

impl Drop for MapperFlushAll {
    fn drop(&mut self) {
        panic!("FlushAll not consumed!");
    }
}

impl MapperFlushAll {
    pub fn new() -> Self {
        MapperFlushAll(false)
    }

    pub fn consume(&mut self, flush: MapperFlush) {
        self.0 = true;
        mem::forget(flush);
    }

    pub fn flush(self, table: &mut ActivePageTable) {
        if self.0 {
            unsafe { table.flush_all() };
        }

        mem::forget(self);
    }

    pub unsafe fn forget(self) {
        mem::forget(self);
    }
}
