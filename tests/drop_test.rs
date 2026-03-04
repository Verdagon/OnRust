#[repr(C)]
struct Point([u8; 8]);

// Drop impl stub: body is replaced by the mir_shims override.
// Required so rustc generates DropGlue(def_id, Some(Point)) instead of None.
impl Drop for Point {
    fn drop(&mut self) {
        unreachable!("toylang drop should be intercepted by mir_shims")
    }
}

extern "C" {
    fn __toylang_drop_Point(ptr: *mut Point);
}

fn main() {
    let mut p = Point([1, 2, 3, 4, 5, 6, 7, 8]);
    unsafe {
        std::ptr::drop_in_place(&mut p as *mut Point);
        std::mem::forget(p);  // prevent double-drop
    }
    println!("done");
}
