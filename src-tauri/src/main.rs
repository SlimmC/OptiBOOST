#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;
use sysinfo::{Disks, System};
use tauri::command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Stops a console window from flashing every time we run a command.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const HIGH_PERFORMANCE_GUID: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
const ULTIMATE_PERFORMANCE_GUID: &str = "e9a42b02-d5df-448d-aa00-03f14749eb61";
const BALANCED_GUID: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
const POWER_SAVER_GUID: &str = "a1841308-3541-4fab-bc81-f71556f20b4a";

/// Set once if nvidia-smi is missing, so we stop trying every few seconds.
static NVSMI_MISSING: AtomicBool = AtomicBool::new(false);

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
pub struct ProcInfo {
    pub name: String,
    pub memory_mb: u64,
    pub cpu_percent: f32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SystemMetrics {
    pub total_memory_gb: f32,
    pub used_memory_gb: f32,
    pub memory_usage_percent: f32,
    pub cpu_usage_percent: f32,
    pub logical_cores: u32,
    pub physical_cores: u32,
    pub process_count: u32,
    pub uptime_hours: f32,
    pub disk_used_gb: f32,
    pub disk_total_gb: f32,
    pub disk_usage_percent: f32,
    pub top_processes: Vec<ProcInfo>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GpuStats {
    pub available: bool,
    pub usage_percent: Option<f32>,
    pub temp_c: Option<f32>,
    pub vram_used_mb: Option<u64>,
    pub vram_total_mb: Option<u64>,
    pub source: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HardwareInfo {
    pub cpu_name: String,
    pub gpu_name: String,
    pub total_ram_gb: f32,
    pub logical_cores: u32,
    pub physical_cores: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    pub id: String,
    pub title: String,
    /// "good", "warn" (clearly hurting performance) or "check" (worth a look)
    pub status: String,
    pub detail: String,
    pub fix_id: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CheckupReport {
    pub items: Vec<CheckItem>,
    pub good: u32,
    pub attention: u32,
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

/// (total_bytes, available_bytes) of the drive Windows is installed on.
fn system_disk() -> (u64, u64) {
    let drive = std::env::var("SystemDrive")
        .unwrap_or_else(|_| "C:".to_string())
        .to_uppercase();
    let disks = Disks::new_with_refreshed_list();
    for d in disks.list() {
        if d.mount_point().to_string_lossy().to_uppercase().starts_with(&drive) {
            return (d.total_space(), d.available_space());
        }
    }
    (0, 0)
}

fn metrics_blocking() -> SystemMetrics {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu();
    sys.refresh_processes();
    sleep(Duration::from_millis(250)); // CPU % needs two samples
    sys.refresh_cpu();
    sys.refresh_processes();

    let gb = 1024.0 * 1024.0 * 1024.0;
    let total = sys.total_memory() as f64;
    let used = sys.used_memory() as f64;
    let pct = if total > 0.0 { used / total * 100.0 } else { 0.0 };
    let logical = sys.cpus().len().max(1);

    // Group processes by name (a browser can have 20+ processes).
    let mut by_name: HashMap<String, (u64, f32)> = HashMap::new();
    for (_pid, p) in sys.processes() {
        let e = by_name.entry(p.name().to_string()).or_insert((0, 0.0));
        e.0 += p.memory();
        e.1 += p.cpu_usage();
    }
    let hidden = ["System Idle Process", "System", "Memory Compression", "Registry"];
    let mut procs: Vec<ProcInfo> = by_name
        .into_iter()
        .filter(|(name, _)| !hidden.contains(&name.as_str()))
        .map(|(name, (mem, cpu))| ProcInfo {
            name,
            memory_mb: mem / (1024 * 1024),
            cpu_percent: round1((cpu as f64) / logical as f64),
        })
        .collect();
    procs.sort_by(|a, b| b.memory_mb.cmp(&a.memory_mb));
    procs.truncate(5);

    let (d_total, d_avail) = system_disk();
    let d_used = d_total.saturating_sub(d_avail);
    let d_pct = if d_total > 0 {
        d_used as f64 / d_total as f64 * 100.0
    } else {
        0.0
    };

    SystemMetrics {
        total_memory_gb: round1(total / gb),
        used_memory_gb: round1(used / gb),
        memory_usage_percent: round1(pct),
        cpu_usage_percent: round1(sys.global_cpu_info().cpu_usage() as f64),
        logical_cores: logical as u32,
        physical_cores: sys.physical_core_count().unwrap_or(0) as u32,
        process_count: sys.processes().len() as u32,
        uptime_hours: round1(System::uptime() as f64 / 3600.0),
        disk_used_gb: round1(d_used as f64 / gb),
        disk_total_gb: round1(d_total as f64 / gb),
        disk_usage_percent: round1(d_pct),
        top_processes: procs,
    }
}

fn gpu_blocking() -> GpuStats {
    // NVIDIA cards: nvidia-smi ships with the driver and gives usage, temp and VRAM.
    if !NVSMI_MISSING.load(Ordering::Relaxed) {
        match run(
            "nvidia-smi",
            &[
                "--query-gpu=utilization.gpu,temperature.gpu,memory.used,memory.total",
                "--format=csv,noheader,nounits",
            ],
        ) {
            Ok(out) => {
                if let Some(line) = out.lines().next() {
                    let p: Vec<&str> = line.split(',').map(|x| x.trim()).collect();
                    if p.len() >= 4 {
                        return GpuStats {
                            available: true,
                            usage_percent: p[0].parse().ok(),
                            temp_c: p[1].parse().ok(),
                            vram_used_mb: p[2].parse().ok(),
                            vram_total_mb: p[3].parse().ok(),
                            source: "NVIDIA".to_string(),
                        };
                    }
                }
            }
            Err(_) => NVSMI_MISSING.store(true, Ordering::Relaxed),
        }
    }

    // Other GPUs (AMD / Intel): Windows' built-in GPU counters give usage only.
    let script = r#"$ErrorActionPreference = 'SilentlyContinue'
$c = (Get-Counter '\GPU Engine(*engtype_3D)\Utilization Percentage').CounterSamples
if ($c) {
  $sum = ($c | Measure-Object CookedValue -Sum).Sum
  ([math]::Min(100, [math]::Round($sum, 1))).ToString([cultureinfo]::InvariantCulture)
}"#;
    let usage = run("powershell", &["-NoProfile", "-NonInteractive", "-Command", script])
        .ok()
        .and_then(|o| o.trim().parse::<f32>().ok());

    GpuStats {
        available: usage.is_some(),
        usage_percent: usage,
        temp_c: None,
        vram_used_mb: None,
        vram_total_mb: None,
        source: "Windows".to_string(),
    }
}

fn hardware_blocking() -> HardwareInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu();
    let cpu_name = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();

    let script = r#"$ErrorActionPreference = 'SilentlyContinue'
(Get-CimInstance Win32_VideoController | Where-Object { $_.Name -notmatch 'Basic|Remote|Virtual' } | ForEach-Object { $_.Name }) -join ' | '"#;
    let gpu_name = run("powershell", &["-NoProfile", "-NonInteractive", "-Command", script])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    HardwareInfo {
        cpu_name,
        gpu_name,
        total_ram_gb: round1(sys.total_memory() as f64 / (1024.0 * 1024.0 * 1024.0)),
        logical_cores: sys.cpus().len() as u32,
        physical_cores: sys.physical_core_count().unwrap_or(0) as u32,
    }
}

// --------------------------------------------------------------- check-up

const SCAN_SCRIPT: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
$gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object {
  $age = $null
  if ($_.DriverDate) { $age = [int]((Get-Date) - $_.DriverDate).TotalDays }
  [pscustomobject]@{ name = $_.Name; refresh = $_.CurrentRefreshRate; driverAgeDays = $age }
})
$ram = @(Get-CimInstance Win32_PhysicalMemory | ForEach-Object {
  [pscustomobject]@{ speed = $_.Speed; configured = $_.ConfiguredClockSpeed }
})
$bat = Get-CimInstance Win32_Battery | Select-Object -First 1
$batStatus = $null
if ($bat) { $batStatus = [int]$bat.BatteryStatus }
$startup = @(Get-CimInstance Win32_StartupCommand | ForEach-Object { $_.Name })
[pscustomobject]@{ gpus = $gpus; ram = $ram; battery = $batStatus; startup = $startup } | ConvertTo-Json -Compress -Depth 4"#;

fn scan_hardware() -> Option<Value> {
    let out = run("powershell", &["-NoProfile", "-NonInteractive", "-Command", SCAN_SCRIPT]).ok()?;
    serde_json::from_str(out.trim()).ok()
}

fn reg_dword(key: &str, name: &str) -> Option<u32> {
    let out = run("reg", &["query", key, "/v", name]).ok()?;
    let line = out.lines().find(|l| l.contains("REG_DWORD"))?;
    let val = line.split_whitespace().last()?;
    u32::from_str_radix(val.trim_start_matches("0x"), 16).ok()
}

fn item(
    id: &str,
    title: &str,
    status: &str,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> CheckItem {
    CheckItem {
        id: id.to_string(),
        title: title.to_string(),
        status: status.to_string(),
        detail: detail.into(),
        fix_id: fix.map(|f| f.to_string()),
    }
}

fn is_real_gpu(g: &Value) -> bool {
    let n = g["name"].as_str().unwrap_or("").to_lowercase();
    !n.is_empty() && !(n.contains("basic") || n.contains("remote") || n.contains("virtual"))
}

fn is_dedicated_gpu(g: &Value) -> bool {
    let n = g["name"].as_str().unwrap_or("").to_lowercase();
    n.contains("nvidia") || n.contains("geforce") || n.contains("radeon rx") || n.contains("radeon pro") || n.contains(" arc")
}

fn checkup_blocking() -> CheckupReport {
    let mut items: Vec<CheckItem> = Vec::new();
    let hw = scan_hardware();
    let empty: Vec<Value> = Vec::new();
    let gpus = hw.as_ref().and_then(|v| v["gpus"].as_array()).unwrap_or(&empty);
    let mods = hw.as_ref().and_then(|v| v["ram"].as_array()).unwrap_or(&empty);
    let startup = hw.as_ref().and_then(|v| v["startup"].as_array()).unwrap_or(&empty);

    // 1. Power plan
    match run("powercfg", &["/getactivescheme"]).ok().and_then(|o| extract_guid(&o)) {
        Some(g) if g == POWER_SAVER_GUID => items.push(item(
            "power_plan", "Power plan", "warn",
            "Windows is on Power Saver, which holds your processor back while gaming.",
            Some("power_plan"),
        )),
        Some(g) if g == BALANCED_GUID => items.push(item(
            "power_plan", "Power plan", "check",
            "Windows is on the Balanced plan. A high-performance plan can help a little on desktops; on laptops it drains the battery faster.",
            Some("power_plan"),
        )),
        Some(_) => items.push(item(
            "power_plan", "Power plan", "good",
            "A performance-oriented power plan is already active.", None,
        )),
        None => {}
    }

    // 2. Laptop on battery
    if let Some(code) = hw.as_ref().and_then(|v| v["battery"].as_i64()) {
        if code == 1 {
            items.push(item(
                "battery", "Battery power", "warn",
                "Your laptop is running on battery. Most laptops slow down a lot unplugged, so plug in the charger before gaming.",
                None,
            ));
        } else {
            items.push(item("battery", "Battery power", "good", "Your laptop is plugged in.", None));
        }
    }

    // 3. Game Mode
    match reg_dword(r"HKCU\Software\Microsoft\GameBar", "AutoGameModeEnabled") {
        Some(0) => items.push(item(
            "game_mode", "Windows Game Mode", "warn",
            "Game Mode is turned off. It helps Windows focus resources on your game.",
            Some("game_mode"),
        )),
        _ => items.push(item("game_mode", "Windows Game Mode", "good", "Game Mode is on.", None)),
    }

    // 4. Background game recording
    match reg_dword(r"HKCU\Software\Microsoft\Windows\CurrentVersion\GameDVR", "HistoricalCaptureEnabled") {
        Some(1) => items.push(item(
            "game_dvr", "Background game recording", "check",
            "Windows is recording your gameplay in the background, which can cost FPS on weaker PCs. You can turn it back on in Settings > Gaming > Captures.",
            Some("game_dvr"),
        )),
        _ => items.push(item("game_dvr", "Background game recording", "good", "Background recording is off.", None)),
    }

    // 5. Refresh rate
    let refresh = gpus.iter().filter_map(|g| g["refresh"].as_u64()).filter(|&r| r > 0).max();
    match refresh {
        Some(r) if r <= 60 => items.push(item(
            "refresh", "Screen refresh rate", "check",
            format!("Your screen is running at {r} Hz. Many gaming monitors support 120 to 240 Hz, but Windows often leaves them at 60. Check: Settings > System > Display > Advanced display > Choose a refresh rate. Only change it if your monitor supports more."),
            None,
        )),
        Some(r) => items.push(item(
            "refresh", "Screen refresh rate", "good",
            format!("Your screen is running at {r} Hz."), None,
        )),
        None => {}
    }

    // 6. RAM speed (XMP / EXPO) and channels
    let rated = mods.iter().filter_map(|m| m["speed"].as_u64()).max();
    let running = mods.iter().filter_map(|m| m["configured"].as_u64()).filter(|&v| v > 0).min();
    if let (Some(rated), Some(running)) = (rated, running) {
        if running * 100 < rated * 90 {
            items.push(item(
                "ram_speed", "Memory speed", "check",
                format!("Your RAM is rated for {rated} MHz but is running at {running} MHz. That usually means XMP/EXPO is off in the BIOS. Fix: restart, press Del or F2 at startup, find XMP, EXPO or DOCP, enable it, then save and exit. (This app can't change BIOS settings.)"),
                None,
            ));
        } else {
            items.push(item(
                "ram_speed", "Memory speed", "good",
                format!("Your RAM is running at {running} MHz."), None,
            ));
        }
    }
    if mods.len() == 1 {
        items.push(item(
            "ram_channels", "Memory channels", "check",
            "Only one memory stick was detected. Two matching sticks (dual channel) give a noticeable boost in many games. That's a hardware change, so it may not apply to laptops with soldered memory.",
            None,
        ));
    }

    // 7. Startup apps
    if !hw.is_none() {
        let n = startup.len();
        if n > 10 {
            let names: Vec<&str> = startup.iter().filter_map(|x| x.as_str()).take(6).collect();
            items.push(item(
                "startup", "Startup apps", "check",
                format!("{n} apps start with Windows (for example: {}). Open Task Manager > Startup apps and disable ones you don't need.", names.join(", ")),
                None,
            ));
        } else {
            items.push(item("startup", "Startup apps", "good", format!("{n} apps start with Windows."), None));
        }
    }

    // 8. Disk space
    let (d_total, d_avail) = system_disk();
    if d_total > 0 {
        let free_pct = d_avail as f64 / d_total as f64 * 100.0;
        let free_gb = d_avail as f64 / (1024.0 * 1024.0 * 1024.0);
        if free_pct < 10.0 {
            items.push(item(
                "disk", "Free disk space", "warn",
                format!("Only {free_gb:.0} GB free on your Windows drive ({free_pct:.0}%). A nearly full drive slows loading and Windows itself."),
                None,
            ));
        } else if free_pct < 20.0 {
            items.push(item(
                "disk", "Free disk space", "check",
                format!("{free_gb:.0} GB free on your Windows drive ({free_pct:.0}%). Try to keep at least 20% free."),
                None,
            ));
        } else {
            items.push(item("disk", "Free disk space", "good", format!("{free_gb:.0} GB free on your Windows drive."), None));
        }
    }

    // 9. Graphics driver age
    let gpu = gpus
        .iter()
        .find(|g| is_real_gpu(g) && is_dedicated_gpu(g))
        .or_else(|| gpus.iter().find(|g| is_real_gpu(g)));
    if let Some(age) = gpu.and_then(|g| g["driverAgeDays"].as_i64()) {
        if age > 365 {
            items.push(item(
                "driver", "Graphics driver", "check",
                format!("Your graphics driver is about {} months old. New game releases often need a newer one. Download it from the NVIDIA, AMD or Intel website (avoid third-party driver updater tools).", age / 30),
                None,
            ));
        } else {
            items.push(item("driver", "Graphics driver", "good", "Your graphics driver is reasonably recent.", None));
        }
    }

    // 10. Uptime
    let days = System::uptime() / 86400;
    if days >= 7 {
        items.push(item(
            "uptime", "Restart", "check",
            format!("Your PC has been on for {days} days. A restart clears memory leaks and pending updates."),
            None,
        ));
    } else {
        items.push(item("uptime", "Restart", "good", "Your PC was restarted recently.", None));
    }

    if hw.is_none() {
        items.push(item(
            "scan", "Hardware scan", "check",
            "Some hardware checks (screen, memory, graphics driver, startup apps) couldn't run on this PC.",
            None,
        ));
    }

    let good = items.iter().filter(|i| i.status == "good").count() as u32;
    let attention = items.len() as u32 - good;
    CheckupReport { items, good, attention }
}

fn apply_fix_blocking(id: &str) -> Result<String, String> {
    match id {
        "power_plan" => activate_best_power_plan().map(|n| format!("Power plan set to {n}.")),
        "game_mode" => enable_game_mode().map(|_| "Game Mode turned on.".to_string()),
        "game_dvr" => run(
            "reg",
            &[
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\GameDVR",
                "/v",
                "HistoricalCaptureEnabled",
                "/t",
                "REG_DWORD",
                "/d",
                "0",
                "/f",
            ],
        )
        .map(|_| "Background recording turned off.".to_string()),
        _ => Err("Unknown fix".to_string()),
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
async fn get_gpu_stats() -> Result<GpuStats, String> {
    tauri::async_runtime::spawn_blocking(gpu_blocking)
        .await
        .map_err(|e| e.to_string())
}

#[command]
async fn get_hardware_info() -> Result<HardwareInfo, String> {
    tauri::async_runtime::spawn_blocking(hardware_blocking)
        .await
        .map_err(|e| e.to_string())
}

#[command]
async fn run_checkup() -> Result<CheckupReport, String> {
    tauri::async_runtime::spawn_blocking(checkup_blocking)
        .await
        .map_err(|e| e.to_string())
}

#[command]
async fn apply_fix(id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || apply_fix_blocking(&id))
        .await
        .map_err(|e| e.to_string())?
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
            get_gpu_stats,
            get_hardware_info,
            run_checkup,
            apply_fix,
            execute_full_boost,
            restore_power_plan,
            launch_client,
            verify_license
        ])
        .run(tauri::generate_context!())
        .expect("error while running OptiBOOST application");
}
