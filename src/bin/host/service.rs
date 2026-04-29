use std::ffi::OsString;
use std::process::Command;
use std::time::Duration;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "WSLMemoryHost";
const DISPLAY_NAME: &str = "WSL Memory Host Agent";
const DESCRIPTION: &str = "Intelligent WSL2 vmmem memory management service";
const RECOVERY_RESET_SECONDS: &str = "86400";
const RECOVERY_ACTIONS: &str = "restart/5000/restart/30000/restart/60000";

define_windows_service!(ffi_service_main, service_main);

pub fn run_as_service() -> anyhow::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("service dispatcher failed: {}", e))
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service_inner() {
        tracing::error!("service error: {}", e);
    }
}

fn run_service_inner() -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let config = super::config::load_or_create()?;

    // Set up file-based logging for service mode
    let log_dir = super::config::config_dir().join("logs");
    let writer = super::logging::SharedRotatingWriter::new(
        log_dir.join("host.log"),
        config.logging.clone(),
    )?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(config.logging.level.clone()));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .try_init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tokio::select! {
            result = super::run_server(&config, false) => {
                if let Err(e) = result {
                    tracing::error!("server error: {}", e);
                }
            }
            _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()) => {
                tracing::info!("service stop signal received");
            }
        }
    });

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

fn configure_service_recovery() -> anyhow::Result<()> {
    let status = Command::new("sc.exe")
        .args([
            "failure",
            SERVICE_NAME,
            "reset=",
            RECOVERY_RESET_SECONDS,
            "actions=",
            RECOVERY_ACTIONS,
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to configure service recovery: {}", status);
    }

    let status = Command::new("sc.exe")
        .args(["failureflag", SERVICE_NAME, "1"])
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to enable service failure actions: {}", status);
    }

    let status = Command::new("sc.exe")
        .args(["config", SERVICE_NAME, "start=", "delayed-auto"])
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to enable delayed auto-start: {}", status);
    }

    Ok(())
}

pub fn install() -> anyhow::Result<()> {
    let exe_path = std::env::current_exe()?;
    let config = super::config::load_or_create()?;
    super::config::ensure_token(&config.token_path)?;
    super::config::save(&config)?;

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    let service = manager.create_service(
        &service_info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
    )?;
    service.set_description(DESCRIPTION)?;
    configure_service_recovery()?;
    service.start::<OsString>(&[])?;

    if let Some(port) = config.effective_listen_port() {
        super::firewall::add_rule(&[port], &config.remote_ips).map_err(anyhow::Error::msg)?;
    }

    tracing::info!("service installed and started");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    )?;

    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    service.delete()?;

    // Also remove firewall rule
    let _ = super::firewall::remove_rule();

    tracing::info!("service uninstalled");
    Ok(())
}
