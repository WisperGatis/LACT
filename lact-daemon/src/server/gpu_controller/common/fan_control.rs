use amdgpu_sysfs::{gpu_handle::fan_control::FanCurve as PmfwCurve, hw_mon::Temperature};
use anyhow::{anyhow, bail, Context};
use lact_schema::config::FanCurve;
use tracing::warn;

pub trait FanCurveExt {
    fn pwm_at_temp(&self, temp: Temperature) -> u8;

    fn into_pmfw_curve(self, current_pmfw_curve: PmfwCurve) -> anyhow::Result<PmfwCurve>;

    fn validate(&self) -> anyhow::Result<()>;
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
impl FanCurveExt for FanCurve {
    fn pwm_at_temp(&self, temp: Temperature) -> u8 {
        let current = temp.current.expect("No current temp");

        // This scenario is most likely unreachable as the kernel shuts down the GPU when it reaches critical temperature
        if temp.crit.filter(|crit| current > *crit).is_some() {
            warn!("GPU temperature is beyond critical values! {current}°C");
            return u8::MAX;
        }

        let current = current as i32;
        let maybe_lower = self.0.range(..current).next_back();
        let maybe_higher = self.0.range(current..).next();

        let percentage = match (maybe_lower, maybe_higher) {
            (Some((lower_temp, lower_speed)), Some((higher_temp, higher_speed))) => {
                let speed_ratio = (current - lower_temp) as f32 / (higher_temp - lower_temp) as f32;
                lower_speed + (higher_speed - lower_speed) * speed_ratio
            }
            (Some((_, lower_speed)), None) => *lower_speed,
            (None, Some((_, higher_speed))) => *higher_speed,
            (None, None) => panic!("Could not find fan speed on the curve! This is a bug."),
        };

        (f32::from(u8::MAX) * percentage) as u8
    }

    fn into_pmfw_curve(self, current_pmfw_curve: PmfwCurve) -> anyhow::Result<PmfwCurve> {
        if current_pmfw_curve.points.len() != self.0.len() {
            return Err(anyhow!(
                "The GPU only supports {} curve points, given {}",
                current_pmfw_curve.points.len(),
                self.0.len()
            ));
        }
        let allowed_ranges = current_pmfw_curve
            .allowed_ranges
            .context("The GPU does not allow fan curve modifications")?;
        let min_percent = *allowed_ranges.speed_range.start();
        let max_percent = *allowed_ranges.speed_range.end();
        let min_temp = *allowed_ranges.temperature_range.start();
        let max_temp = *allowed_ranges.temperature_range.end();

        let points = self
            .0
            .into_iter()
            .map(|(temp, ratio)| {
                let custom_percent = (ratio * 100.0) as u8;

                if !(min_temp..=max_temp).contains(&temp) {
                    bail!("Temperature {temp}℃ is outside of the allowed range {min_temp}℃ to {max_temp}℃");
                }

                if !(min_percent..=max_percent).contains(&custom_percent) {
                    bail!("Speed {custom_percent}% is outside of the allowed range {min_percent}% to {max_percent}%");
                }

                Ok((temp, custom_percent))
            })
            .collect::<anyhow::Result<_>>()?;

        Ok(PmfwCurve {
            points,
            allowed_ranges: Some(allowed_ranges),
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        for percentage in self.0.values() {
            if !(0.0..=1.0).contains(percentage) {
                return Err(anyhow!("Fan speed percentage must be between 0 and 1"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FanCurve, PmfwCurve};
    use crate::server::gpu_controller::common::fan_control::FanCurveExt;
    use amdgpu_sysfs::{gpu_handle::fan_control::FanCurveRanges, hw_mon::Temperature};
    use anyhow::anyhow;

    fn simple_pwm(temp: f32) -> u8 {
        let curve = FanCurve([(0, 0.0), (100, 1.0)].into());
        let temp = Temperature {
            current: Some(temp),
            crit: Some(150.0),
            crit_hyst: Some(-100.0),
        };
        curve.pwm_at_temp(temp)
    }

    #[test]
    fn simple_curve_middle() {
        let pwm = simple_pwm(45.0);
        assert_eq!(pwm, 114);
    }

    #[test]
    fn simple_curve_start() {
        let pwm = simple_pwm(0.0);
        assert_eq!(pwm, 0);
    }

    #[test]
    fn simple_curve_end() {
        let pwm = simple_pwm(100.0);
        assert_eq!(pwm, 255);
    }

    #[test]
    fn simple_curve_before() {
        let pwm = simple_pwm(-5.0);
        assert_eq!(pwm, 0);
    }

    #[test]
    fn simple_curve_after() {
        let pwm = simple_pwm(105.0);
        assert_eq!(pwm, 255);
    }

    #[test]
    fn curve_crit() {
        let curve = FanCurve([(20, 0.0), (80, 100.0)].into());
        let temp = Temperature {
            current: Some(100.0),
            crit: Some(90.0),
            crit_hyst: Some(0.0),
        };
        let pwm = curve.pwm_at_temp(temp);
        assert_eq!(pwm, 255);
    }

    #[test]
    fn uneven_curve() {
        let curve = FanCurve([(30, 0.0), (40, 0.1), (55, 0.9), (61, 1.0)].into());
        let pwm_at_temp = |current: f32| {
            let temp = Temperature {
                current: Some(current),
                crit: Some(90.0),
                crit_hyst: Some(0.0),
            };
            curve.pwm_at_temp(temp)
        };

        assert_eq!(pwm_at_temp(30.0), 0);
        assert_eq!(pwm_at_temp(35.0), 12);
        assert_eq!(pwm_at_temp(40.0), 25);
        assert_eq!(pwm_at_temp(47.0), 120);
        assert_eq!(pwm_at_temp(52.0), 188);
        assert_eq!(pwm_at_temp(53.0), 202);
        assert_eq!(pwm_at_temp(54.0), 215);
    }

    #[test]
    fn default_curve() {
        let curve = FanCurve::default();
        let pwm_at_temp = |current: f32| {
            let temp = Temperature {
                current: Some(current),
                crit: Some(90.0),
                crit_hyst: Some(0.0),
            };
            curve.pwm_at_temp(temp)
        };
        assert_eq!(pwm_at_temp(40.0), 76);
        assert_eq!(pwm_at_temp(60.0), 127);
        assert_eq!(pwm_at_temp(65.0), 159);
        assert_eq!(pwm_at_temp(70.0), 191);
        assert_eq!(pwm_at_temp(79.0), 248);
        assert_eq!(pwm_at_temp(85.0), 255);
        assert_eq!(pwm_at_temp(100.0), 255);
    }

    #[test]
    fn default_curve_to_pmfw() {
        let curve = FanCurve::default();
        let current_pmfw_curve = PmfwCurve {
            points: Box::new([(0, 0); 5]),
            allowed_ranges: Some(FanCurveRanges {
                temperature_range: 25..=100,
                speed_range: 30..=100,
            }),
        };
        let pmfw_curve = curve.into_pmfw_curve(current_pmfw_curve).unwrap();
        let expected_points = [(40, 30), (50, 35), (60, 50), (70, 75), (80, 100)];
        assert_eq!(&expected_points, pmfw_curve.points.as_ref());
    }

    #[test]
    fn curve_outside_of_limits_to_pmfw() {
        let curve_invalid_temp =
            FanCurve([(20, 0.4), (50, 0.35), (60, 0.5), (70, 0.75), (80, 1.0)].into());
        let curve_invalid_speed =
            FanCurve([(40, 0.1), (50, 0.35), (60, 0.5), (70, 0.75), (80, 1.0)].into());

        let current_pmfw_curve = PmfwCurve {
            points: Box::new([(0, 0); 5]),
            allowed_ranges: Some(FanCurveRanges {
                temperature_range: 25..=100,
                speed_range: 30..=100,
            }),
        };
        assert_eq!(
            anyhow!("Temperature 20℃ is outside of the allowed range 25℃ to 100℃").to_string(),
            curve_invalid_temp
                .into_pmfw_curve(current_pmfw_curve.clone())
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            anyhow!("Speed 10% is outside of the allowed range 30% to 100%").to_string(),
            curve_invalid_speed
                .into_pmfw_curve(current_pmfw_curve)
                .unwrap_err()
                .to_string()
        );
    }
}
