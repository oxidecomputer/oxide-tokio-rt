// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Examples in this crate are specifically intended to show replacing the
// `tokio::main` attribute, so including `fn main` in examples is didactic
// here.
#![allow(clippy::needless_doctest_main)]
#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use anyhow::Context;
use nix::sys::signal;
use std::fmt;
use std::future::Future;

pub use tokio::runtime::Builder;

pub struct OxideBuilder {
    signal_thread_set: Option<signal::SigSet>,
    tokio_builder: Builder,
}

impl OxideBuilder {
    pub const fn new(tokio_builder: Builder) -> Self {
        Self {
            signal_thread_set: None,
            tokio_builder,
        }
    }

    /// Convenience method for:
    ///
    /// ```rust
    /// oxide_tokio_rt::OxideBuilder::new(tokio::runtime::Builder::new_multi_thread())
    /// ```
    pub fn new_multi_thread() -> Self {
        Self::new(tokio::runtime::Builder::new_multi_thread())
    }

    /// Convenience method for:
    ///
    /// ```rust
    /// oxide_tokio_rt::OxideBuilder::new(tokio::runtime::Builder::new_current_thread())
    /// ```
    pub fn new_current_thread() -> Self {
        Self::new(tokio::runtime::Builder::new_current_thread())
    }

    pub fn signal_thread(&mut self, mut signals: signal::SigSet) -> &mut Self {
        // tokio uses SIGCHLD for tokio-process, so the application itself is
        // probably not using it.
        // XXX(eliza): it would be nicer if we were able to do this only if
        // tokio-process is enabled, but i don't think there's really a good
        // way to do that...
        signals.add(signal::Signal::SIGCHLD);
        self.signal_thread_set = Some(signals);
        self
    }

    /// Configure settings exposed by the [`tokio::runtime::Builder`] type.
    ///
    /// This method accepts a closure that takes a `&mut` reference to a
    /// [`tokio::runtime::Builder`]. This closure may call any number of methods
    /// on the [`tokio::runtime::Builder`]. This method returns a `&mut Self`.
    /// This interface is intended to allow convenient method chaining of
    /// `oxide-tokio-rt` and `tokio::runtime::Builder` configuration, without
    /// requiring temporary variables for either builder. For example:
    ///
    /// ```
    /// use oxide_tokio_rt::OxideBuilder;
    /// use tokio::runtime::Builder;
    /// use nix::sys::signal::{self, SigHandler, SigSet, Signal};
    ///
    ///
    /// fn main() {
    ///     let runtime = OxideBuilder::new(tokio::runtime::Builder::new_multi_thread())
    ///         .configure_tokio(|tokio| {
    ///             // Any number of `tokio::runtime::Builder` methods may
    ///             // be called here.,,
    ///             tokio.worker_threads(4)
    ///                 .thread_name("my-custom-name")
    ///                 .thread_stack_size(3 * 1024 * 1024);
    ///         })
    ///         // Since `configure_tokio` returns `&mut OxideBuilder`,
    ///         // we can chain additional oxide-tokio-rt specific
    ///         // configurations, such as setting up a dedicated signal
    ///         // handling thread.
    ///         .signal_thread(signal::SigSet::all());
    ///         .build()
    ///         .unwrap();
    ///
    ///    // Now, do whatever Tokio things you wanted to with your runtime...
    ///    # drop(runtime);
    /// }
    /// ```
    pub fn configure_tokio(&mut self, f: impl FnOnce(&mut tokio::runtime::Builder)) -> &mut Self {
        f(&mut self.tokio_builder);
        self
    }

    pub fn build(&mut self) -> anyhow::Result<tokio::runtime::Runtime> {
        struct FmtSigSet(signal::SigSet);
        impl fmt::Display for FmtSigSet {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut signals = self.0.iter();
                if let Some(sig) = signals.next() {
                    f.write_str(sig.as_str())?;
                    for sig in signals {
                        write!(f, " | {}", sig.as_str())?;
                    }
                }

                Ok(())
            }
        }

        #[cfg(target_os = "illumos")]
        tokio_dtrace::register_hooks(tokio_builder)
            .map_err(|e| anyhow::anyhow!("failed to initialize tokio-dtrace probes: {e}"))?;

        if let Some(mask) = self.signal_thread_set {
            // First, mask out the signals on the current thread. We use
            // `thread_block()` rather than `thread_set_mask()` as we would like
            // to respect any already-masked signals.
            mask.thread_block()
                .with_context(|| format!("failed to mask signal set {}", FmtSigSet(mask)))?;
            // Make the signal mask for the signal thread, which is the inverse
            // of the mask we just set.
            let mut sigthread_mask = signal::SigSet::all();
            for sig in mask.iter() {
                sigthread_mask.remove(sig);
            }
            std::thread::Builder::new()
                .name("signal-thread".to_string())
                .spawn(move || {
                    loop {
                        if let Err(e) = sigthread_mask.suspend() {
                            // `sigsuspend(2)` is only documented to return EINTR
                            // when a signal actually happens (per
                            // https://pubs.opengroup.org/onlinepubs/9699919799/functions/sigsuspend.html),
                            // and `SigSet::suspend` turns `EINTR` into `Ok(())`.
                            //
                            // So, any other errno *probably* means This probably
                            // means someone made a bad mask using
                            // `SigSet::from_sigset_t_unchecked`, which is
                            // programmer error, I think?
                            panic!(
                                "unexpected errno {e} from `sigsuspend({})`",
                                FmtSigSet(sigthread_mask)
                            );
                        }
                    }
                })
                .with_context(|| {
                    format!(
                        "failed to spawn signal rx thread with sigset {}",
                        FmtSigSet(sigthread_mask)
                    )
                })?;
        }

        #[cfg(target_os = "illumos")]
        tokio_dtrace::register_hooks(self.tokio_builder)
            .map_err(|e| anyhow::anyhow!("failed to initialize tokio-dtrace probes: {e}"))?;

        self.tokio_builder
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
            .build()
            .map_err(|e| anyhow::anyhow!("failed to initialize Tokio runtime: {e}"))
    }

    pub fn run<T>(&mut self, main: impl Future<Output = T>) -> T {
        // If we can't construct the runtime, this is invariably fatal and there
        // is no way to recover. So, let's just panic here instead of making
        // the `main` function handle both the error returned by the main future
        // *and* errors from initializing the runtime.
        match self.build() {
            Ok(rt) => rt.block_on(main),
            Err(e) => panic!("{e:?}"),
        }
    }
}

impl From<Builder> for OxideBuilder {
    fn from(tokio_builder: Builder) -> Self {
        Self {
            signal_thread_set: None,
            tokio_builder,
        }
    }
}

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
    run_builder(Builder::new_multi_thread(), main)
}

/// Runs the provided `main` future, constructing a Tokio runtime using the
/// provided builder.
///
/// This function may be used when additional runtime configuration is required
/// in addition to the configuration provided by this crate.
///
/// If direct access to the [`Runtime`](tokio::runtime::Runtime) struct is
/// required, consider using [`build()`], which returns a
/// [`Runtime`](tokio::runtime::Runtime).
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
pub fn run_builder<T>(builder: impl Into<OxideBuilder>, main: impl Future<Output = T>) -> T {
    builder.into().run(main)
}

/// Applies configuration options to the provided Tokio runtime builder and
/// constructs a new runtime.
///
/// This function is intended to be used when access to the
/// [`Runtime`](tokio::runtime::Runtime) struct is required. For simpler
/// use-cases, consider using [`run_builder()`].
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
/// # Errors
///
/// This function returns an error under the following conditions:
///
/// - On an illumos system, if initializing [`tokio-dtrace`] probes failed.
/// - The Tokio runtime could not be created (typically because a worker thread
///   could not be spawned).
///
/// [`tokio-dtrace`]: https://github.com/oxidecomputer/tokio-dtrace
pub fn build(builder: impl Into<OxideBuilder>) -> anyhow::Result<tokio::runtime::Runtime> {
    builder.into().build()
}
