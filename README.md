# cairo-toolchain-xtasks

Build scripts that are shared between all Cairo Toolchain projects of Software Mansion.

## Usage

We release this crate to [crates.io](https://crates.io/crates/cairo-toolchain-xtasks).
Put this crate as a dependency in your `xtask/Cargo.toml`:

```toml
[dependencies]
cairo-toolchain-xtasks = "1"
```

Using major-specific version spec helps Dependabot pick up new versions.
For further details, copy-paste the logic from other projects, like Scarb or CairoLS.

## Development

Try as much as possible to not break existing workflows anywhere.
Follow semantic versioning.
Ideally, it'd be the best for this crate to always be backwards-compatible and stay on `1` major version number.

To ship your changes, just `cargo publish` a new release to crates.io.
Then make sure the project of your interest `Cargo.lock` points to the new release.
No need to update other projects, Dependabot will do this job for you sometime in the future.

### Adding a new toolchain project

If you're working on a new Cairo Toolchain project,
make sure you add all necessary information in [`src/upgrade.rs`](src/upgrade.rs) xtask.  
