use serde::{Deserialize, Serialize};
use sysinfo::System;
use tracing::info;

/// Hardware runtime profile — determines what capabilities are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeProfile {
    Cloud,
    Desktop,
    Minimal,
}

/// Detected system capabilities.
#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
    pub cpu_cores: usize,
    pub arch: String,
    pub has_gpu: bool,
}

impl SystemCapabilities {
    /// Detect coarse hardware capabilities for runtime profile selection.
    ///
    /// TODO(epic-profile-detection): Replace heuristic GPU detection with provider/runtime
    /// probing (Metal/CUDA/ROCm availability and usable memory), then wire profile
    /// outputs to Candle backend selection for local acceleration.
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();

        let total_ram_mb = sys.total_memory() / (1024 * 1024);
        let available_ram_mb = sys.available_memory() / (1024 * 1024);
        let cpu_cores = sys.cpus().len();
        let arch = std::env::consts::ARCH.to_string();

        // Basic GPU detection — check for known GPU indicators
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
        // TODO(epic-profile-tuning): Calibrate thresholds with benchmark data and
        // allow override knobs per deployment environment, including explicit
        // "prefer CUDA/Metal" hints for Candle-enabled local optimization.
        match (self.available_ram_mb, self.has_gpu) {
            (ram, true) if ram > 16_000 => RuntimeProfile::Cloud,
            (ram, _) if ram > 4_000 => RuntimeProfile::Desktop,
            _ => RuntimeProfile::Minimal,
        }
    }

    fn detect_gpu() -> bool {
        // Check for CUDA
        if std::env::var("CUDA_VISIBLE_DEVICES").is_ok() {
            return true;
        }
        // macOS Metal is available on all Apple Silicon
        if cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64" {
            return true;
        }
        false
    }
}

impl RuntimeProfile {
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
