fn main() {
    let mut vec = vec![1, 2, 3];
    let mut x = &mut 0;
    for i in vec.iter_mut() {
        x = &mut *i;
    }
    // PCG: bb14[9] post_main: Loop(bb8): [_9] -> [x↓'?12] under conditions bb10 -> bb12
    let y = *x;
}
