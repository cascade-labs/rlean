use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use rlean::daemon::{self, Request};

#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.cascadelabs.rleand";
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT: &str = "rleand.service";

#[derive(clap::Args)]
pub(crate) struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(clap::Subcommand)]
enum DaemonCommand {
    /// Install and start rleand at login/boot
    Install {
        /// Install as a system service instead of a per-user service
        #[arg(long)]
        system: bool,
    },
    /// Start the installed daemon
    Start {
        #[arg(long)]
        system: bool,
    },
    /// Stop the installed daemon
    Stop {
        #[arg(long)]
        system: bool,
    },
    /// Show service and control-socket state
    Status {
        #[arg(long)]
        system: bool,
    },
    /// Remove the installed service definition
    Uninstall {
        #[arg(long)]
        system: bool,
    },
}

pub(crate) fn run(args: DaemonArgs) -> Result<()> {
    match args.command {
        DaemonCommand::Install { system } => install(system),
        DaemonCommand::Start { system } => service_action("start", system),
        DaemonCommand::Stop { system } => service_action("stop", system),
        DaemonCommand::Status { system } => status(system),
        DaemonCommand::Uninstall { system } => uninstall(system),
    }
}

fn daemon_binary() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let candidate = current.with_file_name("rleand");
    if candidate.exists() {
        Ok(candidate)
    } else {
        bail!(
            "rleand was not found next to {}; build or install both rlean binaries",
            current.display()
        )
    }
}

fn install(system: bool) -> Result<()> {
    let program = daemon_binary()?;
    let home = daemon::rlean_home()?;
    std::fs::create_dir_all(&home)?;
    #[cfg(target_os = "macos")]
    {
        ensure_system_privilege(system)?;
        let path = launchd_path(system)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let plist = daemon::render_launchd_plist(
            &program,
            &home.join("rleand.log"),
            &home.join("rleand.error.log"),
        );
        std::fs::write(&path, plist)
            .with_context(|| format!("failed to write {}", path.display()))?;
        let domain = launchd_domain(system)?;
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        if command_ok("launchctl", &["print", &target]).is_ok() {
            // The program path is stable across upgrades. Restarting the
            // already-loaded job avoids launchd's bootout/bootstrap race.
            command_ok("launchctl", &["kickstart", "-k", &target])?;
        } else {
            command_ok("launchctl", &["bootstrap", &domain, path_str(&path)?])?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        ensure_system_privilege(system)?;
        let path = systemd_path(system)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, daemon::render_systemd_unit(&program, !system))?;
        systemctl(system, &["daemon-reload"])?;
        systemctl(system, &["enable", "--now", SYSTEMD_UNIT])?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("rleand service installation supports macOS and Linux");

    wait_for_daemon()?;
    println!(
        "rleand installed and running ({})",
        if system { "system" } else { "user" }
    );
    Ok(())
}

fn service_action(action: &str, system: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let domain = launchd_domain(system)?;
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let path = launchd_path(system)?;
        match action {
            "start" => {
                // Bootstrap is needed after a real stop (bootout). Ignore an
                // already-loaded error, then force a prompt start.
                let _ = command_ok("launchctl", &["bootstrap", &domain, path_str(&path)?]);
                command_ok("launchctl", &["kickstart", "-k", &target])
            }
            // `launchctl kill` is not a stop for a KeepAlive job: launchd would
            // immediately respawn it. Bootout unloads the installed job while
            // leaving its plist available for a later `rlean daemon start`.
            "stop" => command_ok("launchctl", &["bootout", &domain, path_str(&path)?]),
            _ => bail!("unsupported daemon action {action}"),
        }
    }
    #[cfg(target_os = "linux")]
    {
        systemctl(system, &[action, SYSTEMD_UNIT])
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("rleand service control supports macOS and Linux")
}

fn status(system: bool) -> Result<()> {
    let service = platform_status(system).unwrap_or_else(|error| format!("unknown ({error})"));
    let socket = daemon::request(&Request::Ping)
        .map(|response| format!("reachable pid={}", response.pid.unwrap_or_default()))
        .unwrap_or_else(|error| format!("unreachable ({error})"));
    println!("service: {service}\ncontrol: {socket}");
    Ok(())
}

fn uninstall(system: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        ensure_system_privilege(system)?;
        let path = launchd_path(system)?;
        let domain = launchd_domain(system)?;
        let _ = command_ok("launchctl", &["bootout", &domain, path_str(&path)?]);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        ensure_system_privilege(system)?;
        let _ = systemctl(system, &["disable", "--now", SYSTEMD_UNIT]);
        let path = systemd_path(system)?;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let _ = systemctl(system, &["daemon-reload"]);
    }
    println!("rleand service uninstalled");
    Ok(())
}

fn wait_for_daemon() -> Result<()> {
    for _ in 0..50 {
        if daemon::request(&Request::Ping).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("rleand service was installed but its control socket did not become ready")
}

fn command_ok(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{} {} failed: {}",
        program,
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(target_os = "macos")]
fn launchd_path(system: bool) -> Result<PathBuf> {
    if system {
        Ok(PathBuf::from(format!(
            "/Library/LaunchDaemons/{LAUNCHD_LABEL}.plist"
        )))
    } else {
        Ok(home_dir()?
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")))
    }
}

#[cfg(target_os = "macos")]
fn launchd_domain(system: bool) -> Result<String> {
    if system {
        Ok("system".to_owned())
    } else {
        Ok(format!("gui/{}", unsafe { libc::getuid() }))
    }
}

#[cfg(target_os = "linux")]
fn systemd_path(system: bool) -> Result<PathBuf> {
    if system {
        Ok(PathBuf::from("/etc/systemd/system").join(SYSTEMD_UNIT))
    } else {
        Ok(home_dir()?.join(".config/systemd/user").join(SYSTEMD_UNIT))
    }
}

#[cfg(target_os = "linux")]
fn systemctl(system: bool, args: &[&str]) -> Result<()> {
    let mut command = Command::new("systemctl");
    if !system {
        command.arg("--user");
    }
    let output = command.args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "systemctl failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn platform_status(system: bool) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let target = format!("{}/{}", launchd_domain(system)?, LAUNCHD_LABEL);
        let output = Command::new("launchctl")
            .args(["print", &target])
            .output()?;
        return Ok(if output.status.success() {
            "running"
        } else {
            "stopped"
        }
        .to_owned());
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("systemctl");
        if !system {
            command.arg("--user");
        }
        let output = command.args(["is-active", SYSTEMD_UNIT]).output()?;
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    #[allow(unreachable_code)]
    Ok("unsupported".to_owned())
}

fn ensure_system_privilege(system: bool) -> Result<()> {
    if system && unsafe { libc::geteuid() } != 0 {
        bail!("--system requires running rlean as root")
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().context("service path is not valid UTF-8")
}
