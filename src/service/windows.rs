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

use crate::paths::{SERVICE_DISPLAY, SERVICE_NAME};

pub fn install(o: &super::InstallOpts) -> io::Result<()> {
    crate::elevate::ensure_root()?;
    if o.user.is_some() && o.password.is_none() {
        return Err(io::Error::other("Windows needs the account's password to run a service as it: add --password"));
    }
    let bin = super::prepare_install(o)?;

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
        // LocalSystem unless an account is given (".\name" for a local one).
        account_name: o.user.as_ref().map(|u| OsString::from(if u.contains('\\') || u.contains('@') { u.clone() } else { format!(".\\{u}") })),
        account_password: o.password.as_ref().map(OsString::from),
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(to_io)?;
    let _ = service.set_description("gangli remote management: this machine as a managed node.");
    // Restart after any exit: crashes, and the deliberate exit after a self-update.
    let restart = || windows_service::service::ServiceAction { action_type: windows_service::service::ServiceActionType::Restart, delay: Duration::from_secs(5) };
    let _ = service.update_failure_actions(windows_service::service::ServiceFailureActions {
        reset_period: windows_service::service::ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart(), restart(), restart()]),
    });
    let _ = service.set_failure_actions_on_non_crash_failures(true);
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
    super::finish_uninstall()?;
    println!("removed the gangli service; the node keeps its identity in gangli.node (delete the folder to remove everything)");
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
