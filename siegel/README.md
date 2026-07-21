# 🧧siegel

> Siegel from the german word for seal

Siegel is a simple package that offers best-effort protected memory allocation for loading and using secrets.

## Motivation

Loading secrets into memory always comes with risks. Using hardware-backed secure elements (e.g. Apple's [Secure Enclave](https://support.apple.com/guide/security/the-secure-enclave-sec59b0b31ff/web)) will provide better security and should be used where possible. However, not all use cases can leverage devices' secure elements. Some examples include:
- Unsupported curves (e.g. curve `secp256k1` is currently unsupported on iOS).
- Unsupported operations (e.g. specific hashing functions, key derivation functions).

For these use cases, Siegel provides a type-safe mechanism to loaad secrets into application memory and perform operations with them. 

Siegel particularly focuses on secrets that must cross foreign boundaries. For example, if you have a zero-knowledge proof system relying on a secret stored in the device's keychain but the specific operations must be performed on the Rust side.


## Design

The design focuses on making it harder to do unsafe behavior. For example, it takes the opinionated approach of secrets being one-time use so that secrets only live in application memory for the time they are actually required. In addition to this, when secrets are in memory they are:
- `mlock`ed to prevent swapping to disk (best-effort).
- `mprotect`-sealed to prevent reading outside of a very explicit scope.
- Zeroized on drop.
- Page-aligned and protected by a canary for page overflows.
- Guarded at the beginning and end to prevent accidental over/underflows.


## What's protected

- ✅ Bugs within the process where memory where the secret is stored is accidentally read.
- ✅ Stale pointer dereferences the sealed secret (`PROT_NONE` segfaults)
- ✔️ Secret swapped to disk (best effort; already not applicable on iOS).
- ✅ UniFFI copies the secret into additional buffers for lowering/lifting which results in unzeroized copies of the secret.
- ✅ Closure panics mid-operation. Secret is still zeroized as long as there is panic unwinding.
- ✅ Makes accidental logging of secrets hard. Secret is only accessible within an explicit closure.

## What's NOT Protected

- 🔴 **Compromised device** where an attacker has arbitrary code execution within the app process. Memory permissions can simply be updated and the secret extracted.
- 🔴 Jailbroken / rooted devices. Memory protections may be bypassed and memory location inspected.
- 🔴 Memory hardware attacks.
- 🔴 The brief window during the read and write inside closures. This window cannot be reduced.
- 🔴 Mistakes by consumers of this package. Particularly, callers could accidentally log the secret, copy it elsewhere, etc.
- 🔴 Panics where the process aborts without triggering `Drop`s. The secret may live in memory until the OS clears it.



## Example


```rust
use siegel::{Siegel, Empty};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secret_bytes = [0x42; 32]; // load your secret from it's safe storage place
    let empty: Siegel<Empty> = Siegel::new(32)?;
    let loaded = empty.write(&secret_bytes)?;
    let signature = loaded.read_once(|secret| {
        // sign_something(secret, &payload)
    })?;
    Ok(())
}
```
