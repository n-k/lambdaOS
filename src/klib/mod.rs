extern crate hole_list_allocator;
extern crate linked_list_allocator;

use self::hole_list_allocator::HEAP;
use alloc::heap::Layout;
use alloc::heap::Alloc;

//Size must be 2-aligned.
pub fn kalloc(size: usize) {
    //Manually create layout.
    let layout = Layout::from_size_align(2, size);

    if let Some(l) = layout {
        //Layout created successfully, allocate some memory on the heap with it.
        if size > (100 * 1024) {
            panic!("requested size is larger than the available heap memory");
        } else {
            let mut heap = HEAP.lock();
            let heap = heap.as_mut();
            let heap = heap.unwrap();

            unsafe { heap.alloc_zeroed(l).unwrap() };
        }
    } else {
        panic!("Invalid layout");
    }
}