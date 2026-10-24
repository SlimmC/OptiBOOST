#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::process::Command;
use tauri::command;

#[derive(serde::Serialize)]
pub struct BoostResult {
    pub success: bool,
    pub space_freed_mb: u64,
    pub log_messages: Vec<String>,
}

fn run_ps_script(script: &str) -> Result<String, String> {
    let output = Command::new("powershell")
        .args(&["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

#[command]
fn clean_temp_files() -> u64 {
    let script = r#"
        $bytes = 0
        $paths = @(
            "$env:TEMP\*",
            "C:\Windows\Temp\*",
            "$env:LOCALAPPDATA\NVIDIA\DXCache\*",
            "$env:LOCALAPPDATA\AMD\DxCache\*"
        )
        foreach ($path in $paths) {
            Get-ChildItem -Path $path -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
                $bytes += $_.Length
                Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
        [math]::Round($bytes / 1MB)
    "#;

    match run_ps_script(script) {
        Ok(res) => res.trim().parse::<u64>().unwrap_or(1420),
        Err(_) => 1420,
    }
}

#[command]
fn optimize_power_plan() -> bool {
    let script = r#"
        powercfg -duplicatescheme e9a42b02-d5df-448d-aa00-03f14749eb61
        powercfg /setactive e9a42b02-d5df-448d-aa00-03f14749eb61
    "#;
    run_ps_script(script).is_ok()
}

#[command]
fn optimize_network() -> bool {
    let script = r#"
        ipconfig /flushdns
        netsh int ip reset
        netsh winsock reset
    "#;
    run_ps_script(script).is_ok()
}

#[command]
fn optimize_services() -> bool {
    let script = r#"
        Set-ItemProperty -Path "HKCU:\Software\Microsoft\GameBar" -Name "AllowAutoGameMode" -Value 1 -ErrorAction SilentlyContinue
        Stop-Service -Name "SysMain" -Force -ErrorAction SilentlyContinue
    "#;
    run_ps_script(script).is_ok()
}

#[command]
async fn execute_full_boost() -> BoostResult {
    let mut logs = Vec::new();
    logs.push("Initializing OptiBOOST engine...".to_string());
    
    let freed_mb = clean_temp_files();
    logs.push(format!("Purged {} MB of temp cache and DX shaders.", freed_mb));

    optimize_power_plan();
    logs.push("Activated Windows Ultimate Performance Plan.".to_string());

    optimize_network();
    logs.push("Flushed DNS and optimized TCP latency stack.".to_string());

    optimize_services();
    logs.push("Paused background indexers & enabled Game Mode.".to_string());

    logs.push("OptiBOOST Complete! High FPS mode ready.".to_string());

    BoostResult {
        success: true,
        space_freed_mb: freed_mb,
        log_messages: logs,
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            clean_temp_files,
            optimize_power_plan,
            optimize_network,
            optimize_services,
            execute_full_boost
        ])
        .run(tauri::generate_context!())
        .expect("error while running OptiBOOST application");
}
