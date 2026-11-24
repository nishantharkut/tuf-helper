#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Balanced,
    Performance,
    Quiet,
    LowPower,
    Custom,
}

impl Profile {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(Self::Balanced),
            1 => Some(Self::Performance),
            2 => Some(Self::Quiet),
            3 => Some(Self::LowPower),
            4 => Some(Self::Custom),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
            Self::LowPower => "Low power",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sensors {
    pub cpu_temp: Option<f32>,
    pub cpu_critical_temp: Option<f32>,
    pub amd_gpu_temp: Option<f32>,
    pub amd_gpu_critical_temp: Option<f32>,
    pub cpu_fan_rpm: Option<u32>,
    pub gpu_fan_rpm: Option<u32>,
}

#[derive(Debug, PartialEq)]
pub struct BatteryInfo {
    pub capacity_percent: Option<u8>,
    pub status: Option<String>,
    pub power_watts: Option<f32>,
    pub charge_limit_percent: Option<u8>,
    pub health_percent: Option<f32>,
    pub cycle_count: Option<u32>,
}
