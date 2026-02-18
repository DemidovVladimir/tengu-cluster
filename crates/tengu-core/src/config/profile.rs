//! Runtime profile detection based on coarse hardware capabilities.
//!
//! Potential use case:
//! Auto-select minimal/desktop/cloud behavior when deploying the same binary on Pi and on desktop.

use serde::{Deserialize, Serialize};
use sysinfo::System;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeProfile {
    /// High-resource environment (cloud GPU/large RAM).
    Cloud,
    /// Typical desktop/laptop environment.
    Desktop,
    /// Constrained environment (SBC/VPS/low-RAM).
    Minimal,
}

#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    /// Total system RAM in MB.
    pub total_ram_mb: u64,
    /// Currently available RAM in MB.
    pub available_ram_mb: u64,
    /// Logical CPU core count.
    pub cpu_cores: usize,
    /// Current process architecture.
    pub arch: String,
    /// Coarse GPU availability signal.
    pub has_gpu: bool,
}

impl SystemCapabilities {
    /// Detect current host capabilities using lightweight heuristics.
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();

        let total_ram_mb = sys.total_memory() / (1024 * 1024);
        let available_ram_mb = sys.available_memory() / (1024 * 1024);
        let cpu_cores = sys.cpus().len();
        let arch = std::env::consts::ARCH.to_string();

        let has_gpu = Self::detect_gpu();

        Self {
            total_ram_mb,
            available_ram_mb,
            cpu_cores,
            arch,
            has_gpu,
        }
    }

    pub fn recommended_profile(&self) -> RuntimeProfile {
        match (self.available_ram_mb, self.has_gpu) {
            (ram, true) if ram > 16_000 => RuntimeProfile::Cloud,
            (ram, _) if ram > 4_000 => RuntimeProfile::Desktop,
            _ => RuntimeProfile::Minimal,
        }
    }

    /// Detect coarse GPU availability for profile resolution.
    ///
    /// Heuristic order:
    /// 1. Explicit operator override via `TENGU_GPU_HINT`
    /// 2. CUDA visibility env hint
    /// 3. Apple Silicon host (assume Metal-capable)
    fn detect_gpu() -> bool {
        if let Ok(hint) = std::env::var("TENGU_GPU_HINT") {
            let normalized = hint.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "none" | "cpu" | "off" | "false" => return false,
                "gpu" | "cuda" | "metal" | "mps" | "on" | "true" => return true,
                _ => {}
            }
        }

        if std::env::var("CUDA_VISIBLE_DEVICES").is_ok() {
            return true;
        }

        // Apple Silicon machines are treated as Metal-capable by default.
        if cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64" {
            return true;
        }
        false
    }
}

impl RuntimeProfile {
    /// Resolve profile from explicit config override or auto-detection.
    pub fn resolve(configured: Option<&str>) -> Self {
        match configured {
            Some("cloud") => RuntimeProfile::Cloud,
            Some("desktop") => RuntimeProfile::Desktop,
            Some("minimal") => RuntimeProfile::Minimal,
            _ => {
                let caps = SystemCapabilities::detect();
                let profile = caps.recommended_profile();
                info!(
                    arch = %caps.arch,
                    ram_mb = caps.total_ram_mb,
                    available_mb = caps.available_ram_mb,
                    cores = caps.cpu_cores,
                    gpu = caps.has_gpu,
                    profile = ?profile,
                    "Auto-detected runtime profile"
                );
                profile
            }
        }
    }
}
