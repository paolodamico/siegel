# 🧧siegel

> Siegel from the german word for seal

Siegel is a simple package that offers best-effort protected memory allocation for loading and using secrets. Read more in [Siegel](https://docs.rs/siegel).

## Foreign-code usage (`siegel-uniffi`)

The secret is filled in the foreign side (e.g. from the Keychain) and consumed once on the Rust side.

```text
1. let session = SiegelSession::new(len: u32) -> Arc<SiegelSession>
2. session.handle_id() -> u64
3. siegel_fill(handle, ptr, len) -> i32         [raw extern "C"]
4. application own function calls
   session.read_once(|bytes| ...)               [Rust closure]
```

And the reverse, for a secret output (e.g. a derived key):

```text
1. application own function returns
   let out = SiegelSession::new_written(len, |buf| input.read_once(|bytes| ...))??;
2. out.handle_id() -> u64
3. siegel_take(handle, ptr, len) -> i32         [raw extern "C"]
4. foreign code zeroizes `ptr`, then releases `out`
```
