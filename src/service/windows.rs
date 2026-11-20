//! Windows SCM integration via the `windows-service` crate. The service runs as LocalSystem and is started
//! by the SCM with `gangli.exe run --service`, which hands control to [`run`].

use std::ffi::OsString;
use std::io;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::paths::{install_bin, SERVICE_DISPLAY, SERVICE_NAME};

pub fn install(join: Option<&str>) -> io::Result<()> {
    crate::elevate::ensure_root()?;
    let bin = install_bin();
    super::copy_self_to(&bin)?;
    super::create_state_dir()?;
    if let Some(code) = join {
        crate::config::store_pending_join(code)?;
    }

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
        .map_err(to_io)?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: bin,
        launch_arguments: vec![OsString::from("run"), OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(to_io)?;
    let _ = service.set_description("Peer-to-peer remote management agent and control center.");
    service.start::<&str>(&[]).map_err(to_io)?;
    println!("installed and started the gangli service");
    Ok(())
}

pub fn uninstall() -> io::Result<()> {
    crate::elevate::ensure_root()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(to_io)?;
    if let Ok(service) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        let _ = service.stop();
        // Give the SCM a moment to release the running process before deleting.
        std::thread::sleep(Duration::from_secs(2));
        service.delete().map_err(to_io)?;
    }
    let _ = std::fs::remove_dir_all(crate::paths::state_dir());
    let _ = std::fs::remove_file(install_bin());
    println!("removed the gangli service");
    Ok(())
}

/// Entry when launched by the SCM (`run --service`): hand the thread to the SCM dispatcher.
pub fn run() -> io::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(to_io)
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        // Nowhere to print once detached from a console; swallow so the process exits cleanly.
        let _ = e;
    }
}

fn run_service() -> io::Result<()> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Preshutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle =
        service_control_handler::register(SERVICE_NAME, event_handler).map_err(to_io)?;

    let set = |state: ServiceState, accept: ServiceControlAccept| {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };

    set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN)
        .map_err(to_io)?;

    // Run the agent body on a worker thread; wake when the SCM asks us to stop.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = stop.clone();
    let worker = std::thread::spawn(move || crate::agent_loop(stop_worker));
    let _ = shutdown_rx.recv();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = worker.join();

    set(ServiceState::Stopped, ServiceControlAccept::empty()).map_err(to_io)?;
    Ok(())
}

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}
