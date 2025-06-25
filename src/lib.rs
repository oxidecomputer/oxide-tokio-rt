// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
pub use tokio::runtime::Builder;

/// Runs the provided `main` future, constructing a multi-threaded Tokio runtime
/// with the default settings.
///
/// To provide additional runtime configuration, or to use the current-thread
/// runtime, use [`run_builder()`], instead.
///
/// # Examples
///
/// Basic usage with an `async` block:
///
/// ```rust
/// fn main() {
///     oxide_tokio_rt::run(async {
///         // ... actually do async stuff ...
///    })
/// }
/// ```
///
/// `run()` returns the output of the `main` future, and can be used
/// with fallible `fn main()`:
///
/// ```rust
/// # mod anyhow { #[derive(Debug)] pub struct Error; }
/// fn main() -> Result<(), anyhow::Error> {
///     oxide_tokio_rt::run(async {
///         // ... actually do fallible async stuff ...
///
///         Ok(())
///     })
/// }
/// ```
///
/// # Panics
///
/// This function panics under the following conditions:
///
/// - On an illumos system, if initializing [`tokio-dtrace`] probes failed.
/// - The Tokio runtime could not be created (typically because a worker thread
///   could not be spawned).
///
/// [`tokio-dtrace`]: https://github.com/oxidecomputer/tokio-dtrace
#[cfg(feature = "rt-multi-thread")]
pub fn run<T>(main: impl Future<Output = T>) -> T {
    run_builder(&mut Builder::new_multi_thread(), main)
}

/// Runs the provided `main` future, constructing a Tokio runtime using the
/// provided builder.
///
/// This function may be used when additional runtime configuration is required
/// in addition to the configuration provided by this crate.
///
/// **Note** that the following builder settings are overridden by this
/// function:
///
/// - [`tokio::runtime::Builder::disable_lifo_slot`]
/// - [`tokio::runtime::Builder::on_task_spawn`]
/// - [`tokio::runtime::Builder::on_before_task_poll`]
/// - [`tokio::runtime::Builder::on_after_task_poll`]
/// - [`tokio::runtime::Builder::on_task_terminate`]
/// - [`tokio::runtime::Builder::on_thread_start`]
/// - [`tokio::runtime::Builder::on_thread_stop`]
/// - [`tokio::runtime::Builder::on_thread_park`]
/// - [`tokio::runtime::Builder::on_thread_unpark`]
///
/// Code which must set any of these configurations should probably just
/// use the builder "manually".
///
/// # Examples
///
/// Using a `current_thread` runtime:
///
/// ```rust
/// fn main() {
///     oxide_tokio_rt::run_builder(
///         &mut oxide_tokio_rt::Builder::new_current_thread(),
///         async {
///             // ... actually do async stuff ...
///         },
///     )
/// }
/// ```
///
/// Setting the number of worker threads:
///
/// ```rust
/// fn main() {
///     let mut builder = oxide_tokio_rt::Builder::new_multi_thread();
///     builder.worker_threads(4);
///
///     oxide_tokio_rt::run_builder(&mut builder, async {
///        // ... actually do async stuff ...
///     })
/// }
/// ```
///
/// # Panics
///
/// This function panics under the following conditions:
///
/// - On an illumos system, if initializing [`tokio-dtrace`] probes failed.
/// - The Tokio runtime could not be created (typically because a worker thread
///   could not be spawned).
///
/// [`tokio-dtrace`]: https://github.com/oxidecomputer/tokio-dtrace
pub fn run_builder<T>(builder: &mut Builder, main: impl Future<Output = T>) -> T {
    #[cfg(target_os = "illumos")]
    if let Err(e) = tokio_dtrace::register_hooks(&mut builder) {
        panic!("Failed to register tokio-dtrace hooks: {e}");
    }

    let rt = builder
        .enable_all()
        // Tokio's "LIFO slot optimization" will place the last task notified by
        // another task on a worker thread in a special slot that is polled
        // before any other tasks from that worker's run queue. This is intended
        // to reduce latency in message-passing systems. However, the LIFO slot
        // currently does not participate in work-stealing, meaning that it can
        // actually *increase* latency substantially when the task that caused
        // the wakeup goes CPU-bound for a long period of time. Therefore, we
        // disable this optimization until the LIFO slot is made stealable.
        //
        // See: https://github.com/tokio-rs/tokio/issues/4941
        .disable_lifo_slot()
        // May as well include a number in worker thread names...
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let n = ATOMIC_ID.fetch_add(1, Ordering::AcqRel);
            format!("tokio-runtime-worker-{n}")
        })
        .build();
    // If we can't construct the runtime, this is invariably fatal and there
    // is no way to recover. So, let's just panic here instead of making
    // the `main` function handle both the error returned by the main future
    // *and* errors from initializing the runtime.
    match rt {
        Ok(rt) => rt.block_on(main),
        Err(e) => panic!("Failed to initialize Tokio runtime: {e}"),
    }
}
