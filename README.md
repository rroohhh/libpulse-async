# libpulse-async
async rust bindings for `libpulse`.

This uses the standard `libpulse` mainloop in a seperate thread to implement a API compatible with the standard async rust ecosystem.

## warning
This is very WIP and I basically have no clue about unsafe rust, so this could all be wrong and stupid :)

## basic example
```rust
#[async_std::main]
async fn main() {
    let mut proplist = Proplist::new().unwrap();
    proplist
        .set_str(pulse::proplist::properties::APPLICATION_NAME, "FooApp")
        .unwrap();

    let mut mainloop = PAAsyncMainloop::new().expect("Failed to create mainloop");

    let mut context = Context::new_with_proplist(mainloop, "FooAppContext", proplist)
        .await
        .expect("Failed to create new context");
}
```
