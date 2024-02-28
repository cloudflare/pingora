use pingora_ketama::{Bucket, Continuum};

use criterion::{criterion_group, criterion_main, Criterion};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

#[cfg(feature = "heap-prof")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn buckets() -> Vec<Bucket> {
    let mut b = Vec::new();

    for i in 1..101 {
        b.push(Bucket::new(format!("127.0.0.{i}:6443").parse().unwrap(), 1));
    }

    b
}

fn random_string() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect()
}

pub fn criterion_benchmark(c: &mut Criterion) {
    #[cfg(feature = "heap-prof")]
    let _profiler = dhat::Profiler::new_heap();

    c.bench_function("create_continuum", |b| {
        b.iter(|| Continuum::new(&buckets()))
    });

    c.bench_function("continuum_hash", |b| {
        let continuum = Continuum::new(&buckets());

        b.iter(|| continuum.node(random_string().as_bytes()))
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
