# kernway-server — Pre-compiled Host + Hot Reload

## Purpose

Pre-compiled binary loads user app as dynamic library (`.so`/`.dll`/`.dylib`).  
Enables hot reload: rebuild the app → the server reloads without restarting.

## Architecture

```
kernway-server (pre-compiled, never rebuilds)
│
├── libloading::Library    ← dlopen user app .so
├── notify::Watcher        ← watch target/debug/*.so
├── Arc<Library>           ← reference-counted for graceful drain
│
└── On file change:
    1. Build new .so (cargo build --lib, ~2-5s)
    2. Wait for in-flight requests to drain (Arc refcount = 0)
    3. dlclose old .so
    4. dlopen new .so
    5. App ready with new code
```

## kernway-abi — Stable ABI

The user app exposes symbols through a stable ABI (no name mangling, no Rust ABI instability):

```rust
// User app must expose:
#[no_mangle]
pub extern "C" fn kernway_create_app() -> *mut dyn KernwayApp {
    Box::into_raw(Box::new(MyApp::new()))
}

#[no_mangle]
pub extern "C" fn kernway_destroy_app(app: *mut dyn KernwayApp) {
    unsafe { drop(Box::from_raw(app)) }
}

// KernwayApp trait — defined in kernway-abi, stable after v0.5
pub trait KernwayApp: Send + Sync {
    fn handle(&self, req: Request) -> Response;
    fn name(&self) -> &str;
    fn version(&self) -> &str;
}
```

## Hot Reload Flow

```
Developer saves file
│
├── notify detects change
├── kernway dev → cargo build --lib
│   (output: target/debug/libmyapp.so)
│
├── kernway-server detects new .so
├── Arc::strong_count(old_lib) waits → 0 (all in-flight drained)
├── drop old Arc<Library>           ← dlclose
├── Library::new("libmyapp.so")     ← dlopen
│
└── kernway dev prints:
    [kernway dev] Hot-reloaded in 2.3s ✓
```

## State on Reload

| State | Behavior |
|---|---|
| In-memory state (Vec, HashMap) | Reset — starts fresh |
| Database state | Preserved (external) |
| HTTP sessions | Lost if in-memory; preserved if using Redis |
| Environment variables | Preserved |

**Best practice for development**: use DB seeding to reset state after a reload.

## Cargo.toml for the user app

```toml
[lib]
crate-type = ["cdylib"]    # Dynamic library

[dependencies]
kernway = { version = "0.5", features = ["hot-reload"] }
```

## Limitations

- Windows: `.dll` load/unload has fewer limitations than Linux — tested, but less stable
- Debug symbols do not reload — attaching a debugger requires a restart
- A panic in app code = kernway-server catches and logs it, without crashing the server

## Production

Hot reload is only for `kernway dev`. In production (`kernway build`): it creates a single static binary with no dynamic loading.
