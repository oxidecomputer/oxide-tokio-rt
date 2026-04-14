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
#[cfg(unix)]
use nix::sys::signal;
use std::fmt;
use std::future::Future;

pub use tokio::runtime::Builder;

#[cfg(not(tokio_unstable))]
compile_error!(
    "`--cfg tokio_unstable` is required to build oxide-tokio-rt.\n\n\
     If your project already sets it in its .cargo/config.toml, make sure\n\
     you don't hava a global ~/.cargo/config.toml (for example, to use mold).\n"
);

/// A wrapper around [`tokio::runtime::Builder`] that adds additional
/// Oxide-specific configurations.
///
/// This builder allows configuring functionality provided by `oxide-tokio-rt`
/// in addition to the Tokio configurations set using
/// [`tokio::runtime::Builder`].
///
/// # Usage
///
/// An `OxideBuilder` contains a [`tokio::runtime::Builder`]. The Tokio builder
/// may be accessed mutably using the
/// [`configure_tokio()`](Self::configure_tokio) method. This may be used to set
/// Tokio configurations prior to building a runtime.
///
/// An `OxideBuilder` can be constructed from a [`tokio::runtime::Builder`]
/// using [`OxideBuilder::new`]. In addition, `OxideBuilder` implements both
/// [`From<tokio::runtime::Builder>`] *and* [`From<&mut
/// tokio::runtime::Builder>`]. These conversions may be used to construct an
/// `OxideBuilder` from a [`tokio::runtime::Builder`] with preexisting
/// configurations.
///
/// Alternatively, the [`OxideBuilder::new_multi_thread`] and
/// [`OxideBuilder::new_current_thread`] functions construct a new
/// `OxideBuilder` with the default [`tokio::runtime::Builder`] for a
/// multi-threaded and current-thread runtime, respectively.
///
/// Once the builder has been configured, use [`OxideBuilder::build`] to
/// construct a new [`tokio::runtime::Runtime`] with the requested
/// configuration. Alternatively, the [`OxideBuilder::run`] method provides a
/// convenience API to both construct a runtime and execute a provided future in
/// [`tokio::runtime::Runtime::block_on`] in a single function call.
///
/// # Overridden Tokio Builder Configurations
///
/// When constructing a [`tokio::runtime::Runtime`] using an `OxideBuilder`, the
/// following configuration options set on the [`tokio::runtime::Builder`] are
/// *always* overridden by `oxide-tokio-rt`:
///
/// - [`tokio::runtime::Builder::on_task_spawn`]
/// - [`tokio::runtime::Builder::on_before_task_poll`]
/// - [`tokio::runtime::Builder::on_after_task_poll`]
/// - [`tokio::runtime::Builder::on_task_terminate`]
/// - [`tokio::runtime::Builder::on_thread_start`]
/// - [`tokio::runtime::Builder::on_thread_stop`]
/// - [`tokio::runtime::Builder::on_thread_park`]
/// - [`tokio::runtime::Builder::on_thread_unpark`]
///
/// If an `OxideBuilder` is constructed from a `tokio::runtime::Builder` that
/// sets values for these configurations using [`OxideBuilder::new`], or if
/// [`OxideBuilder::configure_tokio`] is used to set any of these
/// configurations, the user-provided configuration is **always** clobbered!
///
/// Code which must set any of these configurations should probably just
/// use the [`tokio::runtime::Builder`] "manually".
///
/// [`From<tokio::runtime::Builder>`]:  #impl-From<Builder>-for-OxideBuilder<'static>
/// [`From<&mut tokio::runtime::Builder>`]: #impl-From<%26mut+Builder>-for-OxideBuilder<'a>
pub struct OxideBuilder<'a> {
    signal_thread_set: Option<signal::SigSet>,
    tokio_builder: TokioBuilderKind<'a>,
}

impl OxideBuilder<'static> {
    /// Constructs a new [`OxideBuilder`] from the provided
    /// [`tokio::runtime::Builder`].
    ///
    /// Any Tokio configuration options already set on `tokio_builder` will be
    /// used when constructing the runtime, with the exceptions of those listed
    /// [here](#overridden-tokio-builder-configurations).
    pub const fn new(tokio_builder: Builder) -> Self {
        Self {
            signal_thread_set: None,
            tokio_builder: TokioBuilderKind::Owned(tokio_builder),
        }
    }

    /// Convenience method for:
    ///
    /// ```rust
    /// # fn make_doctest_not_ugly() -> oxide_tokio_rt::OxideBuilder<'static> {
    /// oxide_tokio_rt::OxideBuilder::new(tokio::runtime::Builder::new_multi_thread())
    /// # }
    /// ```
    pub fn new_multi_thread() -> Self {
        Self::new(tokio::runtime::Builder::new_multi_thread())
    }

    /// Convenience method for:
    ///
    /// ```rust
    ///
    /// # fn make_doctest_not_ugly() -> oxide_tokio_rt::OxideBuilder<'static> {
    /// oxide_tokio_rt::OxideBuilder::new(tokio::runtime::Builder::new_current_thread())
    /// # }
    /// ```
    pub fn new_current_thread() -> Self {
        Self::new(tokio::runtime::Builder::new_current_thread())
    }
}

impl<'a> OxideBuilder<'a> {
    /// Route all signals in the provided [`signal::SigSet`] to a dedicated
    /// signal-handling thread.
    ///
    /// If this configuration is provided, prior to building the Tokio runtime,
    /// the builder will do the following:
    ///
    /// 1. Set a signal mask on the *current* thread (presumably the main
    ///    thread) which disables all signals in the provided
    ///    [`signal::SigSet`].
    ///
    ///    This is done by calling [`pthread_sigmask`]`(`[`SIG_BLOCK`]`, ...)`
    ///    with the provided [`signal::SigSet`] as the signal mask. The use of
    ///    [`SIG_BLOCK`] rather than [`SIG_SETMASK`] ensures that any other
    ///    signals already masked remain masked, in addition to the requested
    ///    signals.
    /// 2. Spawn a dedicated signal-handling thread (named "signal-thread"),
    ///    which calls [`sigsuspend(2)`] in a loop with a signal mask that is
    ///    the *inverse* of the provided [`signal::SigSet`].
    ///
    /// When the runtime is constructed, any worker threads it spawns will be
    /// children of the current thread, and will therefore inherit its signal
    /// mask, which disables the signals in the provided [`signal::SigSet`].
    /// This ensures that all of those signals will always be delivered to the
    /// dedicated signal-handling thread, while any signal *not* in the set
    /// may be delivered to any other thread in the process, and will *never*
    /// be delivered to the signal-handling thread.
    ///
    /// We assume that in applications using Tokio, `SIGCHLD` is being used by
    /// `tokio::process`, rather than the application itself. Therefore,
    /// `SIGCHLD` is *always* added to the set of signals to be delviered to the
    /// dedicated signal handling thread, and calling `signal_thread` with an
    /// empty [`signal::SigSet`] will still result in `SIGCHLD` being delivered
    /// to the signal therad. However, other signals may also be added to the
    /// set, and they will be routed to the signal thread as well.
    ///
    /// [`sigsuspend(2)`]: https://man7.org/linux/man-pages/man2/sigsuspend.2.html
    /// [`pthread_sigmask`]: https://man7.org/linux/man-pages/man3/pthread_sigmask.3.html
    /// [`SIG_BLOCK`]: https://man7.org/linux/man-pages/man2/sigprocmask.2.html#DESCRIPTION
    /// [`SIG_SETMASK`]: https://man7.org/linux/man-pages/man2/sigprocmask.2.html#DESCRIPTION
    #[cfg(unix)]
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
    /// requiring temporary variables for either builder.
    ///
    /// Note that the Tokio configuration options listed
    /// [here](#overridden-tokio-builder-configurations) will always be
    /// overridden by `oxide-tokio-rt` when constructing the runtime. **Any
    /// user-provided configurations for those settings will be overridden**.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxide_tokio_rt::OxideBuilder;
    /// use nix::sys::signal::{self, SigHandler, SigSet, Signal};
    ///
    /// fn main() {
    ///     let runtime = OxideBuilder::new_multi_thread()
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
    ///         .signal_thread(signal::SigSet::all())
    ///         .build()
    ///         .unwrap();
    ///
    ///    // Now, do whatever Tokio things you wanted to with your runtime...
    ///    # drop(runtime);
    /// }
    /// ```
    pub fn configure_tokio(
        &mut self,
        f: impl FnOnce(&mut tokio::runtime::Builder),
    ) -> &mut Self {
        f(self.tokio_builder.as_mut());
        self
    }

    /// Creates the configured [`tokio::runtime::Runtime`].
    ///
    /// The returned `Runtime` instance is ready to spawn tasks.
    ///
    /// This function is intended to be used when access to the
    /// [`Runtime`](tokio::runtime::Runtime) struct is required. For simpler
    /// use-cases, consider using [`OxideBuilder::run()`].
    ///
    /// # Examples
    ///
    /// ```
    /// use oxide_tokio_rt::OxideBuilder;
    ///
    /// let rt  = OxideBuilder::new_multi_thread().build().unwrap();
    ///
    /// rt.block_on(async {
    ///     println!("Hello from the Tokio runtime");
    /// });
    /// ```
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

        if let Some(mask) = self.signal_thread_set {
            // First, mask out the signals on the current thread. We use
            // `thread_block()` rather than `thread_set_mask()` as we would like
            // to respect any already-masked signals.
            mask.thread_block().with_context(|| {
                format!("failed to mask signal set {}", FmtSigSet(mask))
            })?;
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
        tokio_dtrace::register_hooks(self.tokio_builder.as_mut()).map_err(
            |e| {
                anyhow::anyhow!("failed to initialize tokio-dtrace probes: {e}")
            },
        )?;

        self.tokio_builder.as_mut().enable_all().build().map_err(|e| {
            anyhow::anyhow!("failed to initialize Tokio runtime: {e}")
        })
    }

    /// Creates the configured [`tokio::runtime::Runtime`], and executes the
    /// provided `main` future in a call to
    /// [`Runtime::block_on()`](tokio::runtime::Runtime::block_on).
    ///
    /// If access to the [`Runtime`] is required, use [`OxideBuilder::build()`],
    /// which returns the [`Runtime`], instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxide_tokio_rt::OxideBuilder;
    ///
    /// let rt  = OxideBuilder::new_multi_thread().build().unwrap();
    ///
    /// rt.block_on(async {
    ///     println!("Hello from the Tokio runtime");
    /// });
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
    /// [`tokio-dtrace`]: https://github.com/oxidecomputer/tokio-dtrace
    /// [`Runtime`]: tokio::runtime::Runtime
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

/// A somewhat unfortunate, and arguably overengineered, bit of jank to allow us
/// more API flexibility when constructing `OxideBuilder`s. We would like the
/// common path for using the `OxideBuilder` to construct a Tokio `Runtime` to
/// have the `OxideBuilder` owning the `tokio::runtime::Builder`, so that the
/// user need not construct a `tokio::runtime::Builder` and stick it in a `let`
/// binding so that it can be provided to the `OxideBuilder` as an `&mut
/// tokio::runtime::Builder`. However, prior to adding our own builder type, we
/// also had the [`build()`] and [`run_builder()`] free functions, which took
/// *mutably borrowed* `&mut tokio::runtime::Builder`s and used them to
/// construct a runtime. We would like _those_ free functions to be
/// re-implemented by using the `OxideBuilder` type with its default
/// configurations, and we would _also_ like them to work with an `OxideBuilder`
/// in place of the `tokio::runtime::Builder`. Thus, we would like their
/// argument to be `impl Into<OxideBuilder>`, and we would like to ensure that
/// `&mut tokio::runtime::Builder` implements `Into<OxideBuilder>`, so that
/// existing code which called those functions with an `&mut
/// tokio::runtime::Builder` does not break.
///
/// And that's how we ended up here. Presenting the nicest possible API requires
/// us to be able to construct an `OxideBuilder` from an owned
/// `tokio::runtime::Builder` *or* a mutably borrowed `&mut
/// tokio::runtime::Builder`. So, we use this goofy little enum to allow that.
// Rather than boxing the `Owned` variant, we just tell clippy to shut up about
// this: we expect most programs will only use one or the other, and the owned
// variant is likely to be the more common one. We aren't passing these around
// in hot code, so I don't think the wasted stack size for the `Borrowed`
// variant matters all that much.
#[allow(clippy::large_enum_variant)]
enum TokioBuilderKind<'a> {
    Owned(Builder),
    Borrowed(&'a mut Builder),
}

impl<'a> TokioBuilderKind<'a> {
    fn as_mut(&mut self) -> &mut Builder {
        match self {
            TokioBuilderKind::Owned(builder) => builder,
            TokioBuilderKind::Borrowed(builder) => builder,
        }
    }
}

impl From<Builder> for OxideBuilder<'static> {
    fn from(tokio_builder: Builder) -> Self {
        Self {
            signal_thread_set: None,
            tokio_builder: TokioBuilderKind::Owned(tokio_builder),
        }
    }
}

impl<'a> From<&'a mut Builder> for OxideBuilder<'a> {
    fn from(tokio_builder: &'a mut Builder) -> Self {
        Self {
            signal_thread_set: None,
            tokio_builder: TokioBuilderKind::Borrowed(tokio_builder),
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
/// **Note** that the `tokio::runtime::Builder` configurations [listed
/// here](#overridden-tokio-builder-configurations) will *always* be
/// overriden by this function.
///
/// Code which must set any of these configurations should probably just
/// use the [`tokio::runtime::Builder`] "manually".
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
/// Since this function accepts any type implementing
/// `Into<`[`OxideBuilder`]`<'a>>`, it may be called with a
/// [`tokio::runtime::Builder`] *or* an [`OxideBuilder`]. For example:
///
/// ```rust
/// fn main() {
///     let builder = oxide_tokio_rt::OxideBuilder::new_current_thread()
///         // configure any OxideBuilder settings as desired...
///     ;
///     oxide_tokio_rt::run_builder(
///         builder,
///         async {
///             // ... actually do async stuff ...
///         },
///     )
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
pub fn run_builder<'a, T>(
    builder: impl Into<OxideBuilder<'a>>,
    main: impl Future<Output = T>,
) -> T {
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
pub fn build<'a>(
    builder: impl Into<OxideBuilder<'a>>,
) -> anyhow::Result<tokio::runtime::Runtime> {
    builder.into().build()
}
