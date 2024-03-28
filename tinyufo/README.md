# TinyUFO

TinyUFO is a fast and efficient in-memory cache. It adopts the state-of-the-art [S3-FIFO](https://s3fifo.com/) as well as [TinyLFU](https://arxiv.org/abs/1512.00727) algorithms to achieve high throughput and high hit ratio as the same time.

## Usage

See docs

## Performance Comparison
We compare TinyUFO with [lru](https://crates.io/crates/lru), the most commonly used cache algorithm and [moka](https://crates.io/crates/moka), another [great](https://github.com/rust-lang/crates.io/pull/3999) cache library that implements TinyLFU.

### Hit Ratio

The table below show the cache hit ratio of the compared algorithm under different size of cache, zipf=1.

|cache size / total assets | TinyUFO | TinyUFO - LRU | TinyUFO - moka (TinyLFU) |
| -------- | ------- | ------- | ------ |
| 0.5% | 45.26% | +14.21pp | -0.33pp
| 1% | 52.35% | +13.19pp | +1.69pp
| 5% | 68.89% | +10.14pp | +1.91pp
| 10% | 75.98% | +8.39pp | +1.59pp
| 25% | 85.34% | +5.39pp | +0.95pp

Both TinyUFO and moka greatly improves hit ratio from lru. TinyUFO is the one better in this workload.
[This paper](https://dl.acm.org/doi/pdf/10.1145/3600006.3613147) contains more thorough cache performance
evaluations S3-FIFO, which TinyUFO varies from, against many caching algorithms under a variety of workloads.

### Speed

The table below shows the number of operations performed per second for each cache library. The tests are performed using 8 threads on a x64 Linux desktop.

| Setup | TinyUFO | LRU | moka |
| -------- | ------- | ------- | ------ |
| Pure read | 148.7 million ops | 7.0 million ops | 14.1 million ops
| Mixed read/write | 80.9 million ops | 6.8 million ops | 16.6 million ops

Because of TinyUFO's lock-free design, it greatly outperforms the others.

### Memory overhead

TinyUFO provides a compact mode to trade raw read speed for more memory efficiency. Whether the saving worthy the trade off depends on the actual size and the work load. For small in-memory assets, the saved memory means more things can be cached.

The table below show the memory allocation (in bytes) of the compared cache library under certain workloads to store zero-sized assets.

| cache size | TinyUFO | TinyUFO compact | LRU | moka |
| -------- | ------- | ------- | ------- | ------ |
| 100 | 39,409 | 19,000 | 9,408 | 354,376
| 1000 | 236,053 | 86,352 | 128,512 | 535,888
| 10000 | 2,290,635 | 766,024|  1,075,648 | 2,489,088