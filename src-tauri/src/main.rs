#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;
use sysinfo::System;
use tauri::command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Stops a console window from flashing every time we run a command.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const HIGH_PERFORMANCE_GUID: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
const ULTIMATE_PERFORMANCE_GUID: &str = "e9a42b02-d5df-448d-aa00-03f14749eb61";

/// Your Lemon Squeezy product ID. Lemon Squeezy's validate endpoint says "valid"
/// for ANY store's key, so set this to your own product ID before you sell.
/// 0 = skip the check (testing only).
const EXPECTED_PRODUCT_ID: u64 = 0;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BoostResult {
    pub success: bool,
    pub space_freed_mb: u64,
    pub power_plan: String,
    pub game_mode_enabled: bool,
    pub log_messages: Vec<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SystemMetrics {
    pub total_memory_gb: f32,
    pub used_memory_gb: f32,
    pub memory_usage_percent: f32,
    pub cpu_usage_percent: f32,
}

// ---------------------------------------------------------------- helpers

fn run_env(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = cmd.output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(stderr)
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    run_env(program, args, &[])
}

fn round1(v: f64) -> f32 {
    ((v * 10.0).round() / 10.0) as f32
}

fn extract_guid(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|t| t.trim_matches(|c: char| !(c.is_ascii_hexdigit() || c == '-')))
        .find(|t| t.len() == 36 && t.matches('-').count() == 4)
        .map(|t| t.to_lowercase())
}

fn find_plan_guid(name: &str) -> Option<String> {
    let list = run("powercfg", &["/list"]).ok()?;
    list.lines().find(|l| l.contains(name)).and_then(extract_guid)
}

/// File that remembers the user's original power plan so we can undo.
fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("OptiBOOST").join("previous_power_plan.txt"))
}

// ------------------------------------------------------------ power plan

fn activate_best_power_plan() -> Result<String, String> {
    // Remember what the user had before we touch anything.
    let current = run("powercfg", &["/getactivescheme"])
        .ok()
        .and_then(|o| extract_guid(&o));
    if let (Some(g), Some(p)) = (current, state_file()) {
        if !p.exists() {
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(&p, g);
        }
    }

    // Reuse an existing Ultimate plan so repeated runs don't pile up copies.
    let ultimate = find_plan_guid("Ultimate Performance").or_else(|| {
        run("powercfg", &["/duplicatescheme", ULTIMATE_PERFORMANCE_GUID])
            .ok()
            .and_then(|o| extract_guid(&o))
    });

    if let Some(g) = ultimate {
        if run("powercfg", &["/setactive", g.as_str()]).is_ok() {
            return Ok("Ultimate Performance".to_string());
        }
    }

    run("powercfg", &["/setactive", HIGH_PERFORMANCE_GUID])?;
    Ok("High Performance".to_string())
}

fn restore_power_plan_blocking() -> bool {
    let Some(p) = state_file() else { return false };
    let Ok(guid) = fs::read_to_string(&p) else { return false };
    let guid = guid.trim().to_string();
    if run("powercfg", &["/setactive", guid.as_str()]).is_ok() {
        let _ = fs::remove_file(&p);
        true
    } else {
        false
    }
}

// ------------------------------------------------------------- game mode

fn enable_game_mode() -> Result<(), String> {
    for name in ["AllowAutoGameMode", "AutoGameModeEnabled"] {
        run(
            "reg",
            &[
                "add",
                r"HKCU\Software\Microsoft\GameBar",
                "/v",
                name,
                "/t",
                "REG_DWORD",
                "/d",
                "1",
                "/f",
            ],
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------ temp files

#[derive(Default)]
struct TempStats {
    bytes: u64,
    removed: u32,
    skipped: u32,
}

/// Only files untouched for 24h+ are removed, so running installers and
/// apps are never disturbed. Locked files are skipped, never forced.
fn clean_dir(dir: &Path, stats: &mut TempStats) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            clean_dir(&path, stats);
            let _ = fs::remove_dir(&path); // succeeds only if now empty
        } else {
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let old_enough = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .map(|d| d >= Duration::from_secs(24 * 3600))
                .unwrap_or(false);
            if old_enough && fs::remove_file(&path).is_ok() {
                stats.bytes += size;
                stats.removed += 1;
            } else {
                stats.skipped += 1;
            }
        }
    }
}

// ------------------------------------------------------ blocking workers

fn metrics_blocking() -> SystemMetrics {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu();
    sleep(Duration::from_millis(250)); // CPU % needs two samples
    sys.refresh_cpu();

    let total = sys.total_memory() as f64;
    let used = sys.used_memory() as f64;
    let gb = 1024.0 * 1024.0 * 1024.0;
    let pct = if total > 0.0 { used / total * 100.0 } else { 0.0 };

    SystemMetrics {
        total_memory_gb: round1(total / gb),
        used_memory_gb: round1(used / gb),
        memory_usage_percent: round1(pct),
        cpu_usage_percent: round1(sys.global_cpu_info().cpu_usage() as f64),
    }
}

fn boost_blocking() -> BoostResult {
    let mut logs = Vec::new();
    let mut ok = true;

    let power_plan = match activate_best_power_plan() {
        Ok(name) => {
            logs.push(format!("Power plan set to {name}."));
            name
        }
        Err(e) => {
            ok = false;
            logs.push(format!("Could not change power plan: {e}"));
            String::new()
        }
    };

    let game_mode = match enable_game_mode() {
        Ok(_) => {
            logs.push("Windows Game Mode enabled.".to_string());
            true
        }
        Err(e) => {
            ok = false;
            logs.push(format!("Could not enable Game Mode: {e}"));
            false
        }
    };

    let mut stats = TempStats::default();
    clean_dir(&std::env::temp_dir(), &mut stats);
    logs.push(format!(
        "Temp cleanup: removed {} old files, freed {:.1} MB ({} files in use or recent were left alone).",
        stats.removed,
        stats.bytes as f64 / (1024.0 * 1024.0),
        stats.skipped
    ));

    BoostResult {
        success: ok,
        space_freed_mb: stats.bytes / (1024 * 1024),
        power_plan,
        game_mode_enabled: game_mode,
        log_messages: logs,
    }
}

fn launch_blocking(client: &str) -> bool {
    let uri = match client {
        "steam" => "steam://open/main",
        "epic" => "com.epicgames.launcher://apps",
        "xbox" => "xbox:",
        _ => return false,
    };
    run("cmd", &["/C", "start", "", uri]).is_ok()
}

fn verify_license_blocking(key: &str) -> bool {
    // Test key works in debug builds only; it is compiled out of the installer.
    if cfg!(debug_assertions) && key == "PRO-TEST-KEY" {
        return true;
    }

    let key = key.trim();
    if key.is_empty()
        || key.len() > 100
        || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return false;
    }

    // Key goes in via an environment variable, never into the script text.
    let script = r#"
$ErrorActionPreference = 'Stop'
$res = Invoke-RestMethod -Uri 'https://api.lemonsqueezy.com/v1/licenses/validate' -Method Post -Headers @{ Accept = 'application/json' } -Body @{ license_key = $env:OPTIBOOST_KEY }
if ($res.valid -eq $true -and ($env:OPTIBOOST_PRODUCT -eq '0' -or "$($res.meta.product_id)" -eq $env:OPTIBOOST_PRODUCT)) { 'true' } else { 'false' }
"#;

    let product = EXPECTED_PRODUCT_ID.to_string();
    match run_env(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        &[("OPTIBOOST_KEY", key), ("OPTIBOOST_PRODUCT", product.as_str())],
    ) {
        Ok(out) => out.trim().eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

// -------------------------------------------------------------- commands
// Async + spawn_blocking so slow work never freezes the app window.

#[command]
async fn get_system_metrics() -> Result<SystemMetrics, String> {
    tauri::async_runtime::spawn_blocking(metrics_blocking)
        .await
        .map_err(|e| e.to_string())
}

#[command]
async fn execute_full_boost() -> Result<BoostResult, String> {
    tauri::async_runtime::spawn_blocking(boost_blocking)
        .await
        .map_err(|e| e.to_string())
}

#[command]
async fn restore_power_plan() -> bool {
    tauri::async_runtime::spawn_blocking(restore_power_plan_blocking)
        .await
        .unwrap_or(false)
}

#[command]
async fn launch_client(name: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || launch_blocking(&name))
        .await
        .unwrap_or(false)
}

#[command]
async fn verify_license(key: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || verify_license_blocking(&key))
        .await
        .unwrap_or(false)
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_system_metrics,
            execute_full_boost,
            restore_power_plan,
            launch_client,
            verify_license
        ])
        .run(tauri::generate_context!())
        .expect("error while running OptiBOOST application");
}
