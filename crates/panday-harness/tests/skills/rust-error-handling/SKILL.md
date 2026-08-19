---
name: rust-error-handling
description: Idiomatic Rust error handling — thiserror in libraries, anyhow in binaries, and when to add context.
license: MIT
allowed-tools:
  - read_file
  - edit_file
triggers:
  - error handling
  - Result
  - unwrap
---

# Rust error handling

## Libraries use `thiserror`

A library's errors are part of its API. Give each failure mode a variant so a
caller can match on it:

```rust
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found at {0}")]
    NotFound(PathBuf),
    #[error("invalid TOML: {0}")]
    Parse(String),
}
```

## Binaries use `anyhow`

A binary's caller is a human reading stderr, so a chain of context beats a
typed enum:

```rust
let config = load(&path).with_context(|| format!("loading {}", path.display()))?;
```

## Do not `unwrap` on anything a user controls

`unwrap` is for invariants the code itself guarantees. On user input it is a
crash report waiting to be filed.
