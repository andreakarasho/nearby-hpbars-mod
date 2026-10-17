# Nearby HP Bars

A ClassicUO Rust mod that shows nearby mobiles in a movable, resizable list.

Current features:

- Lists mobiles within 18 tiles and shows `?` until an unknown HP status arrives.
- Requests mobile status when HP is unknown, with retries throttled to once every five seconds per mobile.
- Sorts by distance (nearest first), displays distance, and shows HP percentage below 100%.
- Colors each mobile button using its notoriety.
- Click a mobile name to open its context menu.
- Double-click a mobile name in war mode to attack it.
- Click either quick-action button to target that mobile; right-click the button to cycle its action through Bandage, Heal, Greater Heal, Cure, Arch Cure, Remove Curse, Close Wounds, and Cleansing Winds.
- Supports a 120×70 minimum resize grip and dragging the window; its size is saved across sessions.

Per-gump color and notoriety filters are not configurable yet. Quick-action selections and window size are saved in the mod's global storage.

## Build

Requires Rust and the `wasm32-wasip2` target. Cargo fetches the client mod SDK from its pinned Git revision.

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
```

The component is written to `target/wasm32-wasip2/release/cuo_nearby_hpbars.wasm`. `make build` runs the same build, and `make check` checks the wasm target.
