// Declare Point as a Rust type — layout_of override will intercept it.
// Default layout from [u8; 8] has align=1; our override gives align=4.
#[repr(C)]
struct Point([u8; 8]);

fn main() {
    println!("size  = {}", std::mem::size_of::<Point>());   // 8 in both cases
    println!("align = {}", std::mem::align_of::<Point>());  // 1 default, 4 with override
}
