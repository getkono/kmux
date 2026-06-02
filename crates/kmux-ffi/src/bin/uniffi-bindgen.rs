//! Binding generator for `kmux-ffi` (uniffi *library mode*).
//!
//! Generate the Swift package from the built cdylib:
//!
//! ```sh
//! cargo build -p kmux-ffi
//! cargo run -p kmux-ffi --bin uniffi-bindgen -- \
//!   generate --library target/debug/libkmux_ffi.dylib \
//!   --language swift --out-dir crates/kmux-ffi/bindings/swift
//! ```
fn main() {
    uniffi::uniffi_bindgen_main()
}
