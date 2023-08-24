use std::sync::Arc;

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::{criterion_group, criterion_main};

// This is a struct that tells Criterion.rs to use the "futures" crate's current-thread executor
use futures::StreamExt;

fn test_data_for_size(size: usize) -> Arc<Vec<Vec<u8>>> {
    Arc::new(vec![
        vec![0xFF; size],
        vec![1, 2],
        vec![0x0; size / 2],
        vec![0xFF; size],
        vec![1, 2],
        vec![0x0; size / 2],
        vec![0xFF; size],
        vec![1, 2],
        vec![0x0; size / 2],
        vec![0xFF; size],
        vec![1, 2],
        vec![0x0; size / 2],
        vec![0xFF; size],
        vec![1, 2],
        vec![0x0; size / 2],
        vec![0xFF; size],
        vec![1, 2],
        vec![0x0; size / 2],
    ])
}

// Here we have an async function to benchmark
async fn bench_raw_channels(size: usize, n: usize) {
    let data = test_data_for_size(size);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    // Spawn senders
    for _ in 0..n {
        let tx = tx.clone();
        let echo_data = data.clone();
        tokio::spawn(async move {
            for _ in 0..n {
                for data in echo_data.iter() {
                    tx.send(data.clone()).await.expect("send");
                }
            }
        });
    }
    std::mem::drop(tx);

    let n = data.len() * n * n;
    let mut counter = 0;
    while let Some(rx_data) = rx.recv().await {
        counter += 1;
        if rx_data.len() == size {
            assert_eq!(&rx_data, data[0].as_slice());
        }
    }

    assert_eq!(counter, n);
}

// Here we have an async function to benchmark
async fn bench_pipe(size: usize, n: usize) {
    let data = test_data_for_size(size);
    let (tx, mut rx) = framed_pipe::framed_pipe(size * 32, 16);

    // Spawn senders
    for _ in 0..n {
        let mut tx = tx.clone();
        let echo_data = data.clone();
        tokio::spawn(async move {
            for _ in 0..n {
                for data in echo_data.iter() {
                    tx.send(data).await.expect("send");
                }
            }
        });
    }
    std::mem::drop(tx);

    let n = data.len() * n * n;
    let mut counter = 0;
    while let Some(rx_data) = rx.next().await {
        let rx_data = rx_data.expect("rx");
        counter += 1;
        if rx_data.len() == size {
            assert_eq!(&rx_data, data[0].as_slice());
        }
    }

    assert_eq!(counter, n);
}

fn from_elem(c: &mut Criterion) {
    let size: usize = 1024;
    let n = 48;
    let rt = tokio::runtime::Builder::new_multi_thread().build().unwrap();
    let prt = &rt;

    c.bench_with_input(BenchmarkId::new("pipe", size), &size, |b, &s| {
        b.to_async(prt).iter(|| bench_pipe(s, n));
    });

    c.bench_with_input(BenchmarkId::new("raw ch", size), &size, |b, &s| {
        b.to_async(prt).iter(|| bench_raw_channels(s, n));
    });
}

criterion_group!(benches, from_elem);
criterion_main!(benches);
