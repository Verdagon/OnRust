// Point declared as [u8; 8] — layout_of override gives it align=4.
#[repr(C)]
struct Point([u8; 8]);

// get_x: Toylang-defined function. Body replaced by mir_built override.
// The stub unreachable!() is never reached at runtime.
pub fn get_x(_p: Point) -> i32 {
    unreachable!()
}

fn main() {
    let p = Point([0u8; 8]);
    let x = get_x(p);
    println!("get_x = {}", x);
    assert_eq!(x, 42);
}
