use serde::{Deserialize, Serialize};

/// Lowest linear value treated as silence for dB conversion.
pub const DB_FLOOR_LINEAR: f32 = 1.0e-6;

/// Converts a decibel value to a linear gain factor.
pub fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// Converts a linear gain factor to decibels, flooring silence at a finite value.
pub fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.abs().max(DB_FLOOR_LINEAR).log10()
}

/// Input trim range for strips, in dB.
pub const TRIM_MIN_DB: f32 = -20.0;
pub const TRIM_MAX_DB: f32 = 24.0;

/// Clamps an input trim value to the supported range.
pub fn clamp_trim_db(db: f32) -> f32 {
    db.clamp(TRIM_MIN_DB, TRIM_MAX_DB)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Gain(pub f32);

impl Gain {
    pub const MIN: f32 = 0.0;
    pub const MAX: f32 = 4.0;
    pub const UNITY: Gain = Gain(1.0);

    pub fn as_db(self) -> f32 {
        linear_to_db(self.0)
    }

    pub fn from_db(db: f32) -> Gain {
        Gain(db_to_linear(db))
    }

    pub fn clamped(self) -> Gain {
        Gain(self.0.clamp(Self::MIN, Self::MAX))
    }
}

impl Default for Gain {
    fn default() -> Self {
        Gain::UNITY
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pan(pub f32);

impl Pan {
    pub const LEFT: f32 = -1.0;
    pub const RIGHT: f32 = 1.0;
    pub const CENTER: Pan = Pan(0.0);

    pub fn clamped(self) -> Pan {
        Pan(self.0.clamp(Self::LEFT, Self::RIGHT))
    }
}

impl Default for Pan {
    fn default() -> Self {
        Pan::CENTER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_roundtrip() {
        for db in [-20.0, -6.0, 0.0, 6.0, 24.0] {
            let linear = db_to_linear(db);
            assert!((linear_to_db(linear) - db).abs() < 1.0e-3);
        }
    }

    #[test]
    fn unity_is_zero_db() {
        assert_eq!(db_to_linear(0.0), 1.0);
        assert_eq!(linear_to_db(1.0), 0.0);
    }

    #[test]
    fn trim_is_clamped() {
        assert_eq!(clamp_trim_db(-100.0), TRIM_MIN_DB);
        assert_eq!(clamp_trim_db(100.0), TRIM_MAX_DB);
        assert_eq!(clamp_trim_db(6.0), 6.0);
    }
}
