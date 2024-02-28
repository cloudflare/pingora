use pingora_ketama::{Bucket, Continuum};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn buckets() -> Vec<Bucket> {
    let mut b = Vec::new();

    for i in 1..254 {
        b.push(Bucket::new(
            format!("127.0.0.{i}:6443").parse().unwrap(),
            10,
        ));
    }

    b
}

pub fn main() {
    let _profiler = dhat::Profiler::new_heap();
    let _c = Continuum::new(&buckets());
}
