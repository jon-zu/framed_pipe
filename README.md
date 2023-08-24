# Framed Pipe

MPSC-based Pipe to read/write frames, somewhat like a message box over bytes

# Example

```rust
    let (tx, mut rx) = framed_pipe(n * 2, 4);

    let echo_data = vec![vec![0xFF; n], vec![1, 2], vec![], vec![0x0; n / 2]];
    for _ in 0..100 {
        for data in echo_data.iter() {
            tx.clone().try_send(data).expect("send");
        }
        for data in echo_data.iter() {
            let rx_data = rx.next().await.unwrap().expect("rx");
            assert_eq!(&rx_data, data.as_slice());
        }
    }
```