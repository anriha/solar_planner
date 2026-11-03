//! Exact units, split by physical domain: AC, DC, plus time, money and price.
//!
//! The planner moves energy across three boundaries — PV DC to the pack, PV DC to the AC
//! bus, and the pack DC to the AC bus — and the two sides lose different amounts. So the
//! domains are distinct types: `AcKw + DcKw` does not compile, `AcKwh` never multiplies a
//! DC power, and the **only** bridge between the domains is a conversion curve whose
//! input and output types are fixed by its declaration. A price multiplies `AcKwh` alone,
//! because the meter bills AC.

use serde::Serialize;
use std::ops::{Add, AddAssign, Div, Mul, Sub, SubAssign};

/// The minimum every quantity must offer so that generic code (the efficiency curves)
/// can read a value and rebuild one of the same type.
pub trait Quantity: Copy {
    fn get(self) -> f64;
    fn from_f64(value: f64) -> Self;
}

macro_rules! scalar_unit {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize)]
        pub struct $name(f64);

        impl $name {
            pub const ZERO: Self = Self(0.0);

            pub fn new(value: f64) -> Self {
                Self(value)
            }

            pub fn get(self) -> f64 {
                self.0
            }

            pub fn min(self, other: Self) -> Self {
                Self(self.0.min(other.0))
            }

            pub fn max(self, other: Self) -> Self {
                Self(self.0.max(other.0))
            }

            pub fn clamp(self, low: Self, high: Self) -> Self {
                Self(self.0.clamp(low.0, high.0))
            }

            pub fn abs(self) -> Self {
                Self(self.0.abs())
            }

            pub fn is_zero(self) -> bool {
                self.0 == 0.0
            }
        }

        impl Quantity for $name {
            fn get(self) -> f64 {
                self.0
            }

            fn from_f64(value: f64) -> Self {
                Self(value)
            }
        }

        impl From<f64> for $name {
            fn from(value: f64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for f64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        /// Formats as the bare number, so `{kw}` and `{floor:0.4}` read normally while
        /// the type stays exact.
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl Add for $name {
            type Output = Self;

            fn add(self, other: Self) -> Self {
                Self(self.0 + other.0)
            }
        }

        impl Sub for $name {
            type Output = Self;

            fn sub(self, other: Self) -> Self {
                Self(self.0 - other.0)
            }
        }

        impl Mul<f64> for $name {
            type Output = Self;

            fn mul(self, factor: f64) -> Self {
                Self(self.0 * factor)
            }
        }

        impl Div<f64> for $name {
            type Output = Self;

            fn div(self, divisor: f64) -> Self {
                Self(self.0 / divisor)
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, other: Self) {
                self.0 += other.0;
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, other: Self) {
                self.0 -= other.0;
            }
        }
    };
}

scalar_unit!(AcKw, "AC power in kilowatts — the inverter terminal, the load, the meter.");
scalar_unit!(DcKw, "DC power in kilowatts — the array, or the pack terminals.");
scalar_unit!(AcKwh, "AC energy in kilowatt-hours — what the meter bills and pays for.");
scalar_unit!(DcKwh, "DC energy in kilowatt-hours — what the pack stores.");
scalar_unit!(Hours, "A duration in hours — 0.25 for a fifteen-minute MTU.");
scalar_unit!(Czk, "Money in Czech koruna.");
scalar_unit!(CzkPerKwh, "A price in CZK per AC kilowatt-hour.");

// Power × duration, per domain. Nothing crosses a domain boundary here.

impl Mul<Hours> for AcKw {
    type Output = AcKwh;

    fn mul(self, hours: Hours) -> AcKwh {
        AcKwh(self.get() * hours.get())
    }
}

impl Mul<Hours> for DcKw {
    type Output = DcKwh;

    fn mul(self, hours: Hours) -> DcKwh {
        DcKwh(self.get() * hours.get())
    }
}

impl Mul<AcKw> for Hours {
    type Output = AcKwh;

    fn mul(self, power: AcKw) -> AcKwh {
        AcKwh(power.get() * self.get())
    }
}

impl Mul<DcKw> for Hours {
    type Output = DcKwh;

    fn mul(self, power: DcKw) -> DcKwh {
        DcKwh(power.get() * self.get())
    }
}

// Energy ÷ duration gives back the power of the same domain.

impl Div<Hours> for AcKwh {
    type Output = AcKw;

    fn div(self, hours: Hours) -> AcKw {
        AcKw(self.get() / hours.get())
    }
}

impl Div<Hours> for DcKwh {
    type Output = DcKw;

    fn div(self, hours: Hours) -> DcKw {
        DcKw(self.get() / hours.get())
    }
}

// Shares: a state of charge is DC over DC.

impl Div<AcKwh> for AcKwh {
    type Output = f64;

    fn div(self, other: AcKwh) -> f64 {
        self.get() / other.get()
    }
}

impl Div<DcKwh> for DcKwh {
    type Output = f64;

    fn div(self, other: DcKwh) -> f64 {
        self.get() / other.get()
    }
}

// Money is earned and spent on metered AC energy only.

impl Mul<AcKwh> for CzkPerKwh {
    type Output = Czk;

    fn mul(self, energy: AcKwh) -> Czk {
        Czk(self.get() * energy.get())
    }
}

impl Mul<CzkPerKwh> for AcKwh {
    type Output = Czk;

    fn mul(self, price: CzkPerKwh) -> Czk {
        Czk(self.get() * price.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_times_duration_is_energy_in_the_same_domain() {
        assert!((AcKw::new(2.0) * Hours::new(0.25) - AcKwh::new(0.5)).abs().get() < 1e-12);
        assert!((DcKw::new(2.0) * Hours::new(0.25) - DcKwh::new(0.5)).abs().get() < 1e-12);
        assert!((Hours::new(0.25) * DcKw::new(2.0) - DcKwh::new(0.5)).abs().get() < 1e-12);
    }

    #[test]
    fn energy_over_duration_is_power_in_the_same_domain() {
        assert!((AcKwh::new(0.5) / Hours::new(0.25) - AcKw::new(2.0)).abs().get() < 1e-12);
        assert!((DcKwh::new(0.5) / Hours::new(0.25) - DcKw::new(2.0)).abs().get() < 1e-12);
    }

    #[test]
    fn price_times_ac_energy_is_money() {
        let revenue = CzkPerKwh::new(7.974) * AcKwh::new(12.0);
        assert!((revenue.get() - 95.688).abs() < 1e-9);
    }

    #[test]
    fn a_state_of_charge_is_dc_over_dc() {
        assert!((DcKwh::new(22.4) / DcKwh::new(32.0) - 0.7).abs() < 1e-12);
    }

    #[test]
    fn limits_of_one_domain_compose() {
        let draw = DcKw::new(12.5).min(DcKw::new(10.24)).min(DcKw::new(6.0));
        assert!((draw.get() - 6.0).abs() < 1e-12);
    }

    #[test]
    fn clamp_bounds_both_ways() {
        assert!((DcKw::new(20.0).clamp(DcKw::ZERO, DcKw::new(12.0)).get() - 12.0).abs() < 1e-12);
        assert!((DcKw::new(-5.0).clamp(DcKw::ZERO, DcKw::new(12.0)).get()).abs() < 1e-12);
    }
}
