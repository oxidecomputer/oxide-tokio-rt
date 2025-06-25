# oxide-tokio-rt

Common [Tokio] runtime configuration for Oxide software.

# Overview

Tokio's [async runtime][runtime] exposes a variety of configuration options, many of which cannot be set using the [`#\[tokio::main\]`] attribute. We have determined that some of these configuration options are generally desirable for production software at Oxide. This crate provides common functions for constructing Tokio runtimes with the recommended configurations.

In particular, it currently does the following:

- On illumos, configures the runtime to emit DTrace probes, using 
  [`tokio-dtrace`].
- Disables Tokio's [LIFO slot optimization]. This feature is intended to 
  improve message-passing latency, but because tasks in the LIFO slot do not
  currently participate in work-stealing, it can result in extreme latency spikes in some cases (see [omicron#8334] for a worked example).
  
## When to Use This Crate

In general, the runtime configuration provided by this crate should be
preferred for *all production software at Oxide* that uses Tokio. 

The main reason **not** to use this crate is that it requires Tokio's
[unstable features], as discussed in 
[the following section](#enabling-tokio_unstable_features). Library crates,
especially those which we expect will see use outside of Oxide, generally must
compile without unstable features enabled. Typically, library crates leave
runtime construction up to the application, but may use `#[tokio::main]` in
examples. In some cases, `#[tokio::main]` may also be used in libraries to
provide a blocking interface to an async codebase. If these blocking
interfaces are used in production Oxide software, it may be preferable to
conditionally use `oxide_tokio_rt` rather than `#[tokio::main]` when the
`tokio_unstable` config flag is set, and use `#[tokio::main]` otherwise.

`cargo xtask`s and other development tools which run on a developer's system
locally *may* choose not to use this crate at the author's discretion. This
may be preferable if minimizing dependencies improves build time for such
binaries.

Of course, at the end of the day, it's your software. If you believe that the
configurations provided by this crate don't benefit your particular use case,
it may no tbe necessary for you.

# Usage

## Enabling `tokio_unstable` Features

Some of the runtime settings configured by this crate require Tokio's [unstable
features]. These are features of Tokio that do not yet have stable APIs, and
may change in 1.x releases. Unlike other optional features, Tokio requires 
that only the top-level binary workspace may opt in to these features (i.e.,

they may not be enabled by library dependencies). This means that the unstable
features are enabled using a `RUSTFLAGS` config, rather than a Cargo feature.

The simplest way to enable Tokio's unstable features is to add the
following to your workspace's `.cargo/config.toml` file:

```toml
[build]
rustflags = ["--cfg", "tokio_unstable"]
```

<div class="warning">
The <code>[build]</code> section does <strong>not</strong> go in a
<code>Cargo.toml</code> file. Instead it must be placed in the Cargo config
file <code>.cargo/config.toml</code>.
</div>

For more details, see [Tokio's documentation on unstable features][unstable
features].

### A Warning To `mold` Users

A number of engineers at Oxide use the `mold` linker to improve build times
in local development. Often, `cargo` is configured to use `mold` via a global
`RUSTFLAGS` setting in `~/.cargo/config.toml`. If you're using this crate,
**you gotta stop doing it that way**, as the global `RUSTFLAGS` configuration
will interfere with workspace-local `RUSTFLAGS` configurations required to
enable `tokio_unstable`. `.cargo/config.toml` settings are **not additive**.

Instead, consider using `mold -run cargo` to build with `mold`, as described [here][mold-run].

## Replacing `#[tokio::main]`

To replace basic uses of `#[tokio::main]` with no additional options, use
the `oxide_tokio_rt::run` function in a non-async `fn main()`.  

For example, consider the following `main` function:

```rust
#[tokio::main]
async fn main() {
    // ... actually do async stuff ...
}
```

Using `oxide_tokio_rt::run()`, this becomes the following:

```rust
fn main() {
    oxide_tokio_rt::run(async {
        // ... actually do async stuff ...
    })  
}
```

When additional configuration of the runtime is required, the
`oxide_tokio_rt::run_builder()` function takes a [`tokio::runtime::Builder`] as
an argument, and applies the common configurations to that builder before
using it to construct the runtime.

For example, if we are setting the number of worker threads in the 
`#[tokio::main]` macro, like this:

```rust
#[tokio::main(worker_threads = 10)]
async fn main() {
    // ... actually do async stuff ...
}
```

...we can use `run_builder()` to configure the runtime to have 10 worker
threads, like this:

```rust
fn main() {
    // `oxide-tokio-rt` re-exports theTtokio runtime builder type.
    let mut builder = oxide_tokio_rt::Builder::new_multi_thread();
    // Set the desired number of worker threads to 10.
    builder.worker_threads(10);
    
    // Run the application using the configured builder.
    oxide_tokio_rt::run_builder(&mut builder, async {
        // ... actually do async stuff ...
    })
}
```

Note that `oxide_tokio_rt::run` will construct a 
[multi-threaded Tokio runtime][rt-mt], and therefore requires the 
`"rt-multi-thread"` feature flag. This feature flag is enabled by default. If
an application requires a  single-threaded Tokio runtime, instead, first
disable the "`rt-multi-thread"` feature in your `Cargo.toml`:

```toml
[dependencies.oxide-tokio-rt]
git = "https://github.com/oxidecomputer/oxide-tokio-rt"
default-features = false
```

...and then use `oxide_tokio_rt::run_builder()` with the builder returned by
[`tokio::runtime::Builder::new_current_thread()`][new-current], like so:

```rust
fn main() {
    oxide_tokio_rt::run_builder(&mut
        oxide_tokio_rt::Builder::new_current_thread(),
        async {
            // ... actually do async stuff ...
        })
}
```

## Warning on Use of `#[tokio::main]`

[Clippy]'s [`disallowed_macros`] lint can be used to configure Clippy to emit
a warning when the `#[tokio::main]` attribute is used, to ensure that
`oxide_tokio_rt` is used instead. This is particularly useful in workspaces
that contain a large number of binaries, such as Omicron, to prevent
developers from forgetting to use this crate when adding new binaries.

Adding the following to `clippy.toml` in the root of the workspace will cause Clippy to warn when `#[tokio::main]` is used.

```toml
[[disallowed-macros]]
path = "tokio::main"
reason = "prefer `oxide_tokio_rt` for production software"
replacement = "oxide_tokio_rt::run"
```

[Intentional uses of `#[tokio::main]`](#when-to-use-this-crate) in a workspace
that enables this lint can be annotated with 
`#[expect(clippy::disallowed_macros)]`, ideally along with a `reason` string
explaining why `#[tokio::main]` is in use. For example:

```rust
#[expect(
    clippy::disallowed_macros,
    reason = "this is an example",
)]
#[tokio::main]
async fn main() {
    // ...
}
```

[Tokio]: https://tokio.rs
[runtime]: https://docs.rs/tokio/latest/tokio/runtime/index.html
[`#\[tokio::main\]`]: https://docs.rs/tokio/latest/tokio/attr.main.html
[`tokio-dtrace`]: https://github.com/oxidecomputer/tokio-dtrace
[LIFO slot optimization]: https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html#method.disable_lifo_slot
[omicron#8334]: https://github.com/oxidecomputer/omicron/issues/8334#issuecomment-2993159283
[unstable features]: https://docs.rs/tokio/latest/tokio/#unstable-features
[rt-mt]: https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html#method.new_multi_thread
[`tokio::runtime::Builder`]: https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html
[new-current]: https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html#method.new_current_thread
[Clippy]: https://github.com/rust-lang/rust-clippy
[`disallowed_macros`]: https://rust-lang.github.io/rust-clippy/master/#disallowed_macros
[mold-run]: https://github.com/rui314/mold#how-to-use
