//! `boltffi`'s `#[export]` macro expands differently under these variables
//! (legacy short symbols without them), and cargo doesn't track environment
//! a proc macro reads. Without this, a cdylib built one way is reused the other.
fn main() {
    for var in [
        "BOLTFFI_BINDING_EXPANSION",
        "BOLTFFI_BINDING_EXPANSION_ROOT",
        "BOLTFFI_BINDING_EXPANSION_SOURCE",
        "BOLTFFI_BINDING_EXPANSION_SURFACE",
        "BOLTFFI_BINDING_METADATA",
        "BOLTFFI_BINDING_METADATA_FEATURES",
        "BOLTFFI_BINDING_METADATA_ROOT",
        "BOLTFFI_BINDING_METADATA_SOURCE",
        "BOLTFFI_BINDING_METADATA_SURFACE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
