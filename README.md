# Rcu128

Rcu128 is a Rust library that provides a concurrent data structure for read-copy-update (RCU) style access to a value. It allows multiple readers to access the value concurrently, while ensuring safe updates by blocking writes until all current readers have finished reading the old value.

## Limitation

Only available on platforms that support atomic loads and stores of u128.

## Usage

### Add to your `Cargo.toml`

```toml
[dependencies]
rcu_128 = { git = "https://github.com/cyborg42/rcu_128.git" }
```

## Example

```rust
use rcu_128::RcuCell;

fn main() {
    let rcu_cell = RcuCell::new(42);

    // Read the value
    {
        let guard = rcu_cell.read();
        assert_eq!(*guard, 42);
    }

    // Write a new value
    rcu_cell.write(100);

    // Read the updated value
    {
        let guard = rcu_cell.read();
        assert_eq!(*guard, 100);
    }
}
```

## License

This project is licensed under the MIT License. See the [LICENCE](./LICENSE) file for details.
