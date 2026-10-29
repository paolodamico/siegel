# 🧧siegel

> Siegel from the german word for seal

Siegel is a simple package that offers best-effort protected memory allocation for loading and using secrets. Read more in [Siegel](https://docs.rs/siegel).

## Foreign-code usage (`siegel-uniffi`)

A session carries a secret once, in either direction.

**Foreign to Rust.** The secret is filled in the foreign side (e.g. from the Keychain) and consumed once on the Rust side.

```text
1. let session = SiegelSession::new(len: u32) -> Arc<SiegelSession>
2. session.handle_id() -> u64
3. siegel_fill(handle, ptr, len) -> i32         [raw extern "C"]
4. application own function calls
   session.read_once(|bytes| ...)               [Rust closure]
```

**Rust to foreign.** The secret (e.g. a derived key) is written on the Rust side and taken once on the foreign side.

```text
1. application own function returns a session it wrote
   let out = SiegelSession::new_written(len, |buf| ...)??;   [Rust closure]
2. out.handle_id() -> u64
3. siegel_take(handle, ptr, len) -> i32         [raw extern "C"]
4. foreign code zeroizes `ptr` once done with it
5. foreign code releases `out` (`close()` in Kotlin, last reference dropped in Swift)
```

Read any input session inside the closure (`new_written(len, |buf| input.read_once(..))`), so the
input isn't used up when the output session can't be opened.

`siegel_take` copies into memory the foreign side owns, so wiping that copy is the foreign side's job.
It only takes secrets Rust wrote: a secret the foreign side filled can't be read back out.
On `TAKE_ERR_CANARY` and `TAKE_ERR_PROTECTION` `ptr` is zeroed; every other error leaves it untouched.
A consumed session still counts against the live-session cap until foreign code releases it.

A session Rust wrote can also be passed straight into another Rust function that calls
`read_once`, skipping the foreign copy. It stays takeable by any foreign code holding it, though.
