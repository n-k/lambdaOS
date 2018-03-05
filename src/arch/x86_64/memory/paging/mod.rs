pub use self::entry::EntryFlags;
pub use self::mapper::Mapper;
use arch::memory::{Frame, AreaFrameAllocator, PAGE_SIZE};
use arch::memory::allocate_frames;
use arch::memory::stack_allocator::StackAllocator;
use self::temporary_page::TemporaryPage;
use core::ops::{Add, Deref, DerefMut};
use multiboot2::BootInformation;

pub mod entry;
mod table;
mod temporary_page;
mod mapper;

/// Maximum number of entries a page table can hold.
const ENTRY_COUNT: usize = 512;

/// A physical memory address.
pub struct PhysicalAddress(pub usize);

impl PhysicalAddress {
    pub fn new(addr: usize) -> Self {
        PhysicalAddress(addr)
    }

    /// Return the inner address this `PhysicalAddress` wraps.
    pub fn get(&self) -> usize {
        self.0
    }
}

/// A virtual memory address.
pub struct VirtualAddress(pub usize);

impl VirtualAddress {
    /// Create a new virtual address.
    pub fn new(addr: usize) -> Self {
        VirtualAddress(addr)
    }

    /// Return the inner address this `VirtualAddress` wraps.
    pub fn get(&self) -> usize {
        self.0
    }
}

/// A 4KiB page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Page {
    number: usize,
}

impl Page {
    /// Return the number of the page which contains the given `VirtualAddress`.
    pub fn containing_address(address: VirtualAddress) -> Page {
        assert!(
            address.get() < 0x0000_8000_0000_0000 || address.get() >= 0xffff_8000_0000_0000,
            "invalid address: 0x{:x}",
            address.get()
        );
        Page {
            number: address.get() / PAGE_SIZE,
        }
    }

    /// Return the starting address of a page.
    pub fn start_address(&self) -> VirtualAddress {
        VirtualAddress::new(self.number * PAGE_SIZE)
    }

    fn p4_index(&self) -> usize {
        (self.number >> 27) & 0o777
    }
    fn p3_index(&self) -> usize {
        (self.number >> 18) & 0o777
    }
    fn p2_index(&self) -> usize {
        (self.number >> 9) & 0o777
    }
    fn p1_index(&self) -> usize {
        (self.number >> 0) & 0o777
    }

    /// Return an iterator between the given two pages.
    pub fn range_inclusive(start: Page, end: Page) -> PageIter {
        PageIter {
            start: start,
            end: end,
        }
    }
}

impl Add<usize> for Page {
    type Output = Page;

    fn add(self, rhs: usize) -> Page {
        Page {
            number: self.number + rhs,
        }
    }
}

/// An iterator over pages between `start` and `end`.
#[derive(Copy, Clone)]
pub struct PageIter {
    start: Page,
    end: Page,
}

impl Iterator for PageIter {
    type Item = Page;

    fn next(&mut self) -> Option<Page> {
        if self.start <= self.end {
            let page = self.start;
            self.start.number += 1;
            Some(page)
        } else {
            None
        }
    }
}

/// The system's active page table.
pub struct ActivePageTable {
    mapper: Mapper,
}

impl Deref for ActivePageTable {
    type Target = Mapper;

    fn deref(&self) -> &Mapper {
        &self.mapper
    }
}

impl DerefMut for ActivePageTable {
    fn deref_mut(&mut self) -> &mut Mapper {
        &mut self.mapper
    }
}

impl ActivePageTable {
    pub unsafe fn new() -> ActivePageTable {
        ActivePageTable {
            mapper: Mapper::new(),
        }
    }

    /// Get the start address of the current P4 table as stored in `cr3`.
    pub fn address(&self) -> usize {
        use x86_64::registers::control_regs;
        unsafe { control_regs::cr3().0 as usize }
    }

    pub fn with<F>(
        &mut self,
        table: &mut InactivePageTable,
        temporary_page: &mut temporary_page::TemporaryPage,
        f: F,
    ) where
        F: FnOnce(&mut Mapper),
    {
        use x86_64::registers::control_regs;
        use x86_64::instructions::tlb;

        {
            // Get reference to current P4 table.
            let backup =
                Frame::containing_address(PhysicalAddress::new(control_regs::cr3().0 as usize));

            // map temporary_page to current P4 table
            let p4_table = temporary_page.map_table_frame(backup.clone(), self);

            // overwrite recursive mapping
            self.p4_mut()[511].set(
                table.p4_frame.clone(),
                EntryFlags::PRESENT | EntryFlags::WRITABLE,
            );
            tlb::flush_all();

            // execute f in the new context
            f(self);

            // restore recursive mapping to original P4 table
            p4_table[511].set(backup, EntryFlags::PRESENT | EntryFlags::WRITABLE);
            tlb::flush_all();
        }

        temporary_page.unmap(self);
    }

    /// Switch the active page table, and return the old page table.
    pub fn switch(&mut self, new_table: InactivePageTable) -> InactivePageTable {
        use x86_64;
        use x86_64::registers::control_regs;

        let old_table = InactivePageTable {
            p4_frame: Frame::containing_address(PhysicalAddress::new(
                control_regs::cr3().0 as usize,
            )),
        };

        unsafe {
            control_regs::cr3_write(x86_64::PhysicalAddress(
                new_table.p4_frame.start_address().get() as u64,
            ));
        }
        old_table
    }
}

/// A page table which has a frame wherein the P4 table lives.
pub struct InactivePageTable {
    p4_frame: Frame,
}

impl InactivePageTable {
    pub fn new(
        frame: Frame,
        active_table: &mut ActivePageTable,
        temporary_page: &mut TemporaryPage,
    ) -> InactivePageTable {
        {
            let table = temporary_page.map_table_frame(frame.clone(), active_table);
            table.zero();
            table[511].set(frame.clone(), EntryFlags::PRESENT | EntryFlags::WRITABLE);
        }
        temporary_page.unmap(active_table);

        InactivePageTable { p4_frame: frame }
    }
}

/// Identity map important sections and switch the page table, remapping the kernel one page above
/// and turning the previous kernel stack into a guard page - this prevents silent stack overflows, as
/// given that the guard page is unmapped, any stack overflow into this page will instantly cause a
/// page fault. Returns the currently active kernel page table.
pub fn init(boot_info: &BootInformation) -> (ActivePageTable, StackAllocator)
{
    use arch::memory::stack_allocator::{self, StackAllocator};
    
    // let mut allocator: AreaFrameAllocator = *ALLOCATOR.lock();
    let mut temporary_page = TemporaryPage::new(Page { number: 0xcafebabe });
    
    let mut stack_allocator: StackAllocator = unsafe { *(&*(0 as *const StackAllocator)) };

    let mut active_table = unsafe { ActivePageTable::new() };
    let mut new_table = {
        // Allocate a frame for the PML4.
        let frame = allocate_frames(1).expect("out of memory");
        InactivePageTable::new(frame, &mut active_table, &mut temporary_page)
    };
    
    // Do important mapping work.
    active_table.with(&mut new_table, &mut temporary_page, |mapper| {
        println!("[ vmm ] Initialising paging.");
        
        let elf_sections_tag = boot_info
            .elf_sections_tag()
            .expect("Memory map tag required");

        // identity map the entire kernel.
        for section in elf_sections_tag.sections() {
            if !section.is_allocated() {
                // section is not loaded to memory
                continue;
            }

            assert!(
                section.start_address() as usize % PAGE_SIZE == 0,
                "sections need to be page aligned"
            );
            println!(
                "[ vmm ] Identity mapping kernel section at addr: {:#x}, size: {} KiB",
                section.start_address(),
                section.size() / 1024,
            );
            
            // Translate ELF section flags to paging flags, and map the kernel sections
            // into the virtual address space using these flags.
            let flags = EntryFlags::from_elf_section_flags(&section);

            let start_frame =
                Frame::containing_address(PhysicalAddress::new(section.start_address() as usize));
            let end_frame =
                Frame::containing_address(PhysicalAddress::new((section.end_address() - 1) as usize));
            for frame in Frame::range_inclusive(start_frame, end_frame) {
                mapper.identity_map(frame, flags);
            }
        }

        // identity map the VGA text buffer
        println!("[ vmm ] Identity mapping the VGA text buffer.");
        let vga_buffer_frame = Frame::containing_address(PhysicalAddress::new(0xb8000));
        mapper.identity_map(vga_buffer_frame, EntryFlags::WRITABLE);

        // identity map the multiboot info structure.
        println!("[ vmm ] Identity mapping multiboot structures.");
        let multiboot_start =
            Frame::containing_address(PhysicalAddress::new(boot_info.start_address()));
        let multiboot_end =
            Frame::containing_address(PhysicalAddress::new(boot_info.end_address() - 1));
        for frame in Frame::range_inclusive(multiboot_start, multiboot_end) {
            mapper.identity_map(frame, EntryFlags::PRESENT);
        }

        use self::Page;
        use arch::memory::heap_allocator::{HEAP_SIZE, HEAP_START};

        let heap_start_page = Page::containing_address(VirtualAddress::new(HEAP_START));
        let heap_end_page = Page::containing_address(VirtualAddress::new(HEAP_START + HEAP_SIZE - 1));
        
        // Map the heap pages within the range we specified.
        println!("[ vmm ] Mapping heap pages.");
        for page in Page::range_inclusive(heap_start_page, heap_end_page) {
            mapper.map(page, EntryFlags::WRITABLE);
        }

        println!(
            "[ vmm ] Heap start: {:#x}",
            heap_start_page.start_address().get()
        );
        println!(
            "[ vmm ] Heap end: {:#x}",
            heap_end_page.start_address().get()
        );

        // Initialise the allocator API.
        unsafe {
            ::HEAP_ALLOCATOR.init(HEAP_START, HEAP_SIZE);
        }
        
        // Initialise a stack allocator.
        stack_allocator = {
            // Allocate stacks directly after the heap.
            let stack_alloc_start = heap_end_page + 1;
            // allocate stacks within a range of 400KiB.
            let stack_alloc_end = stack_alloc_start + 100;
            let stack_alloc_range = Page::range_inclusive(stack_alloc_start, stack_alloc_end);
            stack_allocator::StackAllocator::new(stack_alloc_range)
        };
    });

    let old_table = active_table.switch(new_table);
    println!(
        "[ vmm ] Switched to new page table. PML4 at {:#x}",
        active_table.address()
    );

    let old_p4_page = Page::containing_address(VirtualAddress::new(
        old_table.p4_frame.start_address().get(),
    ));
    
    active_table.unmap(old_p4_page);
    
    println!(
        "[ vmm ] Guard page at {:#x}.",
        old_p4_page.start_address().get()
    );

    (active_table, stack_allocator)
}
