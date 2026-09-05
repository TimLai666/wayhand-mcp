use std::fmt;

pub const ABSOLUTE_MAX: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordError {
    InvalidScreenSize,
    XOutOfRange { x: i64, width: u32 },
    YOutOfRange { y: i64, height: u32 },
    TransformOutOfRange,
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScreenSize => write!(f, "screen width and height must be positive"),
            Self::XOutOfRange { x, width } => {
                write!(f, "x={x} is outside screenshot width 0..{}", width - 1)
            }
            Self::YOutOfRange { y, height } => {
                write!(f, "y={y} is outside screenshot height 0..{}", height - 1)
            }
            Self::TransformOutOfRange => {
                write!(f, "coordinate transform produced an invalid ABS value")
            }
        }
    }
}

impl std::error::Error for CoordError {}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Transform {
    pub m11: f64,
    pub m12: f64,
    pub m13: f64,
    pub m21: f64,
    pub m22: f64,
    pub m23: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsolutePoint {
    pub x: u16,
    pub y: u16,
}

impl Transform {
    pub fn linear(width: u32, height: u32) -> Result<Self, CoordError> {
        if width == 0 || height == 0 {
            return Err(CoordError::InvalidScreenSize);
        }

        Ok(Self {
            m11: if width == 1 {
                0.0
            } else {
                f64::from(ABSOLUTE_MAX) / f64::from(width - 1)
            },
            m12: 0.0,
            m13: 0.0,
            m21: 0.0,
            m22: if height == 1 {
                0.0
            } else {
                f64::from(ABSOLUTE_MAX) / f64::from(height - 1)
            },
            m23: 0.0,
        })
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.m11 * x + self.m12 * y + self.m13,
            self.m21 * x + self.m22 * y + self.m23,
        )
    }

    pub fn map_pixel(
        &self,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
    ) -> Result<AbsolutePoint, CoordError> {
        if width == 0 || height == 0 {
            return Err(CoordError::InvalidScreenSize);
        }
        if x < 0 || x >= i64::from(width) {
            return Err(CoordError::XOutOfRange { x, width });
        }
        if y < 0 || y >= i64::from(height) {
            return Err(CoordError::YOutOfRange { y, height });
        }

        let (absolute_x, absolute_y) = self.apply(x as f64, y as f64);
        let rounded_x = absolute_x.round();
        let rounded_y = absolute_y.round();
        if !rounded_x.is_finite()
            || !rounded_y.is_finite()
            || rounded_x < 0.0
            || rounded_y < 0.0
            || rounded_x > f64::from(ABSOLUTE_MAX)
            || rounded_y > f64::from(ABSOLUTE_MAX)
        {
            return Err(CoordError::TransformOutOfRange);
        }

        Ok(AbsolutePoint {
            x: rounded_x as u16,
            y: rounded_y as u16,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AbsolutePoint, CoordError, Transform};

    #[test]
    fn linear_mapping_hits_all_corners() {
        let transform = Transform::linear(1920, 1080).unwrap();

        assert_eq!(
            transform.map_pixel(0, 0, 1920, 1080),
            Ok(AbsolutePoint { x: 0, y: 0 })
        );
        assert_eq!(
            transform.map_pixel(1919, 0, 1920, 1080),
            Ok(AbsolutePoint { x: 65535, y: 0 })
        );
        assert_eq!(
            transform.map_pixel(0, 1079, 1920, 1080),
            Ok(AbsolutePoint { x: 0, y: 65535 })
        );
        assert_eq!(
            transform.map_pixel(1919, 1079, 1920, 1080),
            Ok(AbsolutePoint { x: 65535, y: 65535 })
        );
    }

    #[test]
    fn mapping_rejects_negative_and_past_last_pixel() {
        let transform = Transform::linear(100, 80).unwrap();

        assert_eq!(
            transform.map_pixel(-1, 0, 100, 80),
            Err(CoordError::XOutOfRange { x: -1, width: 100 })
        );
        assert_eq!(
            transform.map_pixel(0, -1, 100, 80),
            Err(CoordError::YOutOfRange { y: -1, height: 80 })
        );
        assert_eq!(
            transform.map_pixel(100, 0, 100, 80),
            Err(CoordError::XOutOfRange { x: 100, width: 100 })
        );
        assert_eq!(
            transform.map_pixel(0, 80, 100, 80),
            Err(CoordError::YOutOfRange { y: 80, height: 80 })
        );
    }

    #[test]
    fn mapping_rounds_to_nearest_absolute_unit() {
        let transform = Transform::linear(10, 10).unwrap();

        assert_eq!(
            transform.map_pixel(1, 1, 10, 10),
            Ok(AbsolutePoint { x: 7282, y: 7282 })
        );
        assert_eq!(
            transform.map_pixel(2, 2, 10, 10),
            Ok(AbsolutePoint { x: 14563, y: 14563 })
        );
    }

    #[test]
    fn one_pixel_screen_maps_to_zero() {
        let transform = Transform::linear(1, 1).unwrap();

        assert_eq!(
            transform.map_pixel(0, 0, 1, 1),
            Ok(AbsolutePoint { x: 0, y: 0 })
        );
    }

    #[test]
    fn zero_sized_screen_is_rejected() {
        assert_eq!(
            Transform::linear(0, 100),
            Err(CoordError::InvalidScreenSize)
        );
        assert_eq!(
            Transform::linear(100, 0),
            Err(CoordError::InvalidScreenSize)
        );
    }
}
