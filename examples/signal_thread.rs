// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Example demonstrating the use of `OxideBuilder::signal_thread()`.

#![cfg(unix)]

use anyhow::Context;
use clap::Parser;
use nix::sys::signal::{self, SigSet, Signal};

#[derive(Parser)]
struct Args {
    /// Enable a dedicated signal-handling thread for SIGUSR1.
    #[clap(long)]
    sigthread: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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

    let rt = builder.build()?;
    rt.block_on(async move {
        tokio::spawn(async move {
            let pid = std::process::id();
            eprintln!(
                "Hello! My PID is {pid}. I {} have a dedicated signal \
                 handling thread.\n\
                 Please send me SIGUSR1 and SIGUSR2 using `kill -USR1 {pid}` \
                 and see what\nhappens!\n\n{SIGHELP}",
                if sigthread_enabled { "do" } else { "don't" },
            );
            std::future::pending().await
        })
        .await
    })?;

    Ok(())
}

const SIGHELP: &str = "\
    If there is a dedicated signal handling thread, only SIGUSR1 is routed\n\
    to that thread. However, I will handle both SIGUSR1 *and* SIGUSR2, to\n\
    allow testing both signals that are routed to the dedicated signal\n\
    handling thread, and signals that are not.";

extern "C" fn handler(signal: libc::c_int) {
    let signal = match Signal::try_from(signal) {
        Ok(signal) => signal,
        Err(err) => {
            eprintln!("bad signal {signal}: {}", err);
            return;
        }
    };

    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<no name>");
    eprintln!(
        "we get signal: {signal:?} on thread: {thread_name} ({:?})",
        thread.id()
    )
}
