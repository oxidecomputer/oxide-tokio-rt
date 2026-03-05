// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Example demonstrating the use of `OxideBuilder::signal_thread()`.

#![cfg(unix)]

use anyhow::Context;
use clap::Parser;
use nix::sys::pthread::{Pthread, pthread_self};
use nix::sys::signal::{self, SigSet, Signal};
use std::borrow::Cow;
use std::mem::size_of;
use std::os::fd::IntoRawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::io::AsyncReadExt;

#[derive(Parser)]
struct Args {
    /// Enable a dedicated signal-handling thread for SIGUSR1.
    #[clap(long)]
    sigthread: bool,
}

static PIPE_TX: AtomicI32 = AtomicI32::new(-1);

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Make a pipe to allow our signal handler to inform the rest of the process
    // what signal it received.
    let pipe_rx_fd = {
        let (pipe_rx, pipe_tx) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_NONBLOCK)
                .context("can't get ye pipe")?;
        PIPE_TX.store(pipe_tx.into_raw_fd(), Ordering::Relaxed);
        pipe_rx
    };

    let sigaction = signal::SigAction::new(
        signal::SigHandler::Handler(handler),
        signal::SaFlags::empty(),
        SigSet::empty() | Signal::SIGUSR1 | Signal::SIGUSR2,
    );
    unsafe { signal::sigaction(Signal::SIGUSR1, &sigaction) }
        .context("couldn't set sigaction for SIGUSR1")?;

    unsafe { signal::sigaction(Signal::SIGUSR2, &sigaction) }
        .context("couldn't set sigaction for SIGUSR1")?;

    let mut builder = oxide_tokio_rt::OxideBuilder::new_multi_thread();
    let sigthread_enabled = args.sigthread;
    if sigthread_enabled {
        let sigset = SigSet::empty() | Signal::SIGUSR1;
        builder.signal_thread(sigset);
    }

    let mains_tid = pthread_self();
    let rt = builder.build()?;
    rt.block_on(async move {
        tokio::spawn(async move {
            // Make sure the runtime has something else to do.
            tokio::spawn(std::future::pending::<()>());

            let pid = std::process::id();
            eprintln!(
                "Hello! My PID is {pid}. I {} have a dedicated signal \
                 handling thread.\n\
                 Please send me SIGUSR1 and SIGUSR2 using `kill -USR1 {pid}` \
                 and see what\nhappens!\n\n{SIGHELP}\n",
                if sigthread_enabled { "do" } else { "don't" },
            );
            let mut pipe =
                tokio::net::unix::pipe::Receiver::from_owned_fd(pipe_rx_fd)
                    .context("can't get ye pipe")?;
            let mut tidbytes = [0u8; size_of::<Pthread>()];
            while pipe.read_exact(&mut tidbytes).await.is_ok() {
                let mut sigbytes = [0u8; size_of::<libc::c_int>()];
                pipe.read_exact(&mut sigbytes)
                    .await
                    .context("couldn't read signal from pipe")?;
                let tid = Pthread::from_ne_bytes(tidbytes);
                let sig =
                    Signal::try_from(libc::c_int::from_ne_bytes(sigbytes));

                // on Linux, thread names are always truncated to 16 characters.
                // on illumos, however, they are not, and instead,
                // `pthread_getname_np` will return `ERANGE` if the buffer isn't
                // big enough. we expect a number of the threads to be named
                // "tokio-runtime-worker", which is 20 characters. so just make
                // it Plenty Big and accept that we are giving Linux twice as
                // many bytes as it will fill. this is example code and i don't
                // care.
                let mut namebuf = [0u8; 32];
                let tname = pthread_getname_sp(tid, &mut namebuf);
                let mut tname = tname.as_deref().unwrap_or("<unknown name>");

                // on Linux, the name of the main thread returned by
                // `pthread_getname_np` appears to be the name of the
                // executable. this is generally a reasonable thing for it to
                // return, except for the unfortunate fact that the name of the
                // dedicated signal thread is hard-coded to be "signal-thread",
                // and the name of the binary is..."signal_thread". note the `-`
                // vs `_`. "signal_thread" is, unfortunately, the best name I
                // can come up with for the example, since that's the name of
                // the API it demonstrates. but this is very confusing for the
                // hapless individual who runs the example on Linux, and
                // mistakenly believes that the signal thread is receiving ALL
                // signals.
                //
                // meanwhile, on illumos, the name of the main thread appears to
                // be "" (i.e., empty string). this is also a bit weird but also
                // not entirely unreasonable.
                //
                // for both of these reasons, we just special-case the main
                // thread's pthread ID and don't use the name from
                // `pthread_getname_np`. this hopefully makes the example's
                // behavior less confusing, rather than more confusing?
                if tid == mains_tid {
                    tname = "<the main thread>";
                }
                println!("we get signal {sig:?} on thread {tid} ({tname})");
            }

            Ok::<(), anyhow::Error>(())
        })
        .await
    })??;

    Ok(())
}

const MSGSIZE: usize = size_of::<Pthread>() + size_of::<libc::c_int>();
const SIGHELP: &str = "\
    If there is a dedicated signal handling thread, only SIGUSR1 is routed\n\
    to that thread. However, I will handle both SIGUSR1 *and* SIGUSR2, to\n\
    allow testing both signals that are routed to the dedicated signal\n\
    handling thread, and signals that are not.\n\
";

extern "C" fn handler(signal: libc::c_int) {
    let fd = PIPE_TX.load(Ordering::Relaxed);
    if fd < 0 {
        // no one has made the pipe yet, nothing we can do...
        return;
    }

    let mut buf = [0u8; MSGSIZE];
    let tidbytes = pthread_self().to_ne_bytes();
    buf[0..size_of::<Pthread>()].copy_from_slice(&tidbytes);
    let sigbytes = signal.to_ne_bytes();
    buf[size_of::<Pthread>()..size_of::<Pthread>() + size_of::<libc::c_int>()]
        .copy_from_slice(&sigbytes);
    unsafe {
        // we intentionally use libc::write here, rather than
        // `nix::unistd::write`, as the latter takes `T: AsFd`, which forces us
        // to turn the fd into an `OwnedFd`, which closes the fd when dropped.
        // we could do this but would have to `mem::forget` it after so that it
        // doesn't close the fd, which i felt sad about.
        //
        // also if this sets errno there is nothing we can do because we are a
        // signal handler lol.
        libc::write(fd, buf.as_ptr().cast(), MSGSIZE);
    }
}

/// "pthread_getname sorta-portable"
#[cfg(any(target_os = "linux", target_os = "illumos"))]
fn pthread_getname_sp(tid: Pthread, buf: &mut [u8]) -> Option<Cow<'_, str>> {
    let ret = unsafe {
        // SAFETY: the `_np` means "non portable" ha ha ha!
        libc::pthread_getname_np(
            tid,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    if ret != 0 {
        return None; // give up
    }
    let cstr = std::ffi::CStr::from_bytes_until_nul(&buf[..]).ok()?;
    Some(cstr.to_string_lossy())
}

#[cfg(not(any(target_os = "linux", target_os = "illumos")))]
fn pthread_getname_sp(_: Pthread, _: &mut [u8]) -> Option<Cow<'_, str>> {
    // on macOS, at least, `pthread_getname_np` works differently and i was too
    // lazy to figure it out.
    None
}
