// Example of https://github.com/oxidecomputer/oxide-tokio-rt/issues/3

use anyhow::Context;
use clap::Parser;
use nix::sys::signal::{self, SigHandler, SigSet, Signal};

#[derive(Parser)]
struct Args {
    #[clap(long)]
    sigthread: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let handler = SigHandler::Handler(handler);
    unsafe { signal::signal(Signal::SIGHUP, handler) }.context("couldn't set signal handler")?;

    let mut builder = oxide_tokio_rt::OxideBuilder::new_multi_thread();
    if args.sigthread {
        let sigset = SigSet::empty() | Signal::SIGHUP;
        builder.signal_thread(sigset);
    }

    let rt = builder.build()?;
    rt.block_on(async move {
        tokio::spawn(async move {
            eprintln!("running...");
            std::future::pending().await
        })
        .await
    })?;

    Ok(())
}

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
