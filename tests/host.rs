#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

// make_vec: Toylang-defined function. Body replaced by mir_built override.
fn make_vec() -> Vec<Point> {
    unreachable!()
}

// vec_len: Toylang-defined function. Body replaced by mir_built override.
fn vec_len(v: &Vec<Point>) -> usize {
    unreachable!()
}

fn main() {
    let v = make_vec();
    let len = vec_len(&v);
    println!("Vec length: {}", len);
    assert_eq!(len, 2);
}
