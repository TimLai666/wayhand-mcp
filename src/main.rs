use std::sync::Arc;

use anyhow::Result;
use rmcp::ServiceExt;

mod coords;
mod inject;
mod instance_lock;
mod safety;
mod screenshot;
mod server;

use inject::{Injector, fake::FakeInjector, uinput::UInputInjector};
use safety::Budget;
use server::DesktopServer;

#[tokio::main]
async fn main() -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("desktop-driver refuses to run as root");
        std::process::exit(1);
    }

    let _instance_lock = match instance_lock::InstanceLock::acquire() {
        Ok(lock) => lock,
        Err(instance_lock::InstanceLockError::AlreadyHeld(path)) => {
            eprintln!(
                "desktop-driver is already running; lock held at {}",
                path.display()
            );
            std::process::exit(1);
        }
        Err(instance_lock::InstanceLockError::Other(error)) => return Err(error),
    };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let skip_wayland_check = env_flag("DESKTOP_DRIVER_SKIP_WAYLAND_CHECK");
    if !skip_wayland_check && std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland") {
        anyhow::bail!("desktop-driver only supports a Wayland session");
    }
    if skip_wayland_check {
        tracing::warn!(
            "DESKTOP_DRIVER_SKIP_WAYLAND_CHECK=1 is enabled; this is for sandbox verification only"
        );
    }

    let budget = Arc::new(Budget::new());

    let injector: Box<dyn Injector> = if env_flag("DESKTOP_DRIVER_FAKE_INJECTOR") {
        tracing::warn!(
            "DESKTOP_DRIVER_FAKE_INJECTOR=1 is enabled; this is for sandbox verification only"
        );
        Box::new(FakeInjector::new())
    } else {
        let injector = match UInputInjector::new() {
            Ok(injector) => injector,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "uinput is unavailable; input tools will return an error"
                );
                UInputInjector::unavailable(error.to_string())
            }
        };
        Box::new(injector)
    };
    let server = DesktopServer::new(injector, budget);
    let stop_server = server.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                stop_server.stop().await;
                tracing::warn!("SIGINT received; input injection has been stopped");
            }
            Err(error) => tracing::error!("could not install SIGINT wait: {error}"),
        }
    });

    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}
