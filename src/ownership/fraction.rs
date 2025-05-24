use std::cmp::Ordering;
use std::fmt::{Debug, Display};
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::{mem, u64};

use num_bigint::BigUint;

/// This represents the fraction num / (2^denom)
#[derive(Clone)]
pub struct Fraction {
    num: BigUint,
    denom: u64,
}

impl Fraction {
    pub fn one() -> Self {
        Self { num: 1u32.into(), denom: 0 }
    }
    pub fn is_one(&self) -> bool {
        self.num.bits() == self.denom + 1 && self.num.trailing_zeros() == Some(self.denom)
    }
    pub fn zero() -> Self {
        Self { num: 0u32.into(), denom: 0 }
    }
    pub fn is_zero(&self) -> bool {
        self.num.bits() == 0
    }
    pub fn scale_to_exponent(&mut self, exponent: u64) {
        assert!(exponent >= self.denom);
        let scale = exponent - self.denom;
        self.num <<= scale;
        self.denom = exponent;
    }
    #[allow(unused)]
    pub fn halve(mut self) -> (Self, Self) {
        let other = self.halve_in_place();
        (self, other)
    }
    pub fn halve_in_place(&mut self) -> Self {
        self.denom += 1;
        self.clone()
    }
}

impl Add for Fraction {
    type Output = Fraction;

    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl AddAssign for Fraction {
    fn add_assign(&mut self, mut rhs: Self) {
        let maxexp = self.denom.max(rhs.denom);
        self.scale_to_exponent(maxexp);
        rhs.scale_to_exponent(maxexp);
        self.num.add_assign(rhs.num);
    }
}

impl Sub for Fraction {
    type Output = Fraction;

    fn sub(mut self, rhs: Self) -> Self::Output {
        self -= rhs;
        self
    }
}

impl SubAssign for Fraction {
    fn sub_assign(&mut self, mut rhs: Self) {
        let maxexp = self.denom.max(rhs.denom);
        self.scale_to_exponent(maxexp);
        rhs.scale_to_exponent(maxexp);
        self.num.sub_assign(rhs.num);
    }
}

impl PartialEq for Fraction {
    fn eq<'a>(mut self: &'a Self, mut other: &'a Self) -> bool {
        if self.denom < other.denom {
            mem::swap(&mut self, &mut other);
        }
        let Some(self_ts) = self.num.trailing_zeros() else {
            return other.num.bits() == 0;
        };
        if self_ts + other.denom < self.denom {
            return false;
        }
        let offset = self.denom - other.denom;
        if self.num.bits() != other.num.bits() + offset {
            return false;
        }
        for idx in 0..other.num.bits() {
            if self.num.bit(idx + offset) != other.num.bit(idx) {
                return false;
            }
        }
        return true;
    }
}

impl Eq for Fraction {}

impl PartialOrd for Fraction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Fraction {
    fn cmp_inner(&self, other: &Self) -> std::cmp::Ordering {
        let offset = self.denom - other.denom;
        let highest_bit = self.num.bits().max(other.num.bits() + offset) - offset;
        for idx in (0..highest_bit).into_iter().rev() {
            match self.num.bit(idx + offset).cmp(&other.num.bit(idx)) {
                Ordering::Equal => continue,
                x => return x,
            }
        }
        let Some(x) = self.num.trailing_zeros() else { return Ordering::Equal };
        if x >= offset { return Ordering::Equal } else { return Ordering::Greater }
    }
}

impl Ord for Fraction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.num.bits() < other.num.bits() {
            other.cmp_inner(self).reverse()
        } else {
            self.cmp_inner(other)
        }
    }
}

impl Display for Fraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.num.bits() <= self.denom {
            write!(f, "0")?
        } else {
            for bidx in (self.denom..self.num.bits()).into_iter().rev() {
                write!(f, "{}", if self.num.bit(bidx) { 1 } else { 0 })?;
            }
        }
        let lowest_nz_bit = self.num.trailing_zeros().unwrap_or(u64::MAX);
        if self.denom > lowest_nz_bit {
            write!(f, ".")?
        }
        for bidx in (lowest_nz_bit..self.denom).into_iter().rev() {
            write!(f, "{}", if self.num.bit(bidx) { 1 } else { 0 })?;
        }
        write!(f, "b2")
    }
}

impl Debug for Fraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

#[cfg(test)]
mod test {
    use std::cmp::Ordering;

    use super::Fraction;

    #[test]
    pub fn test_fractions() {
        let one = Fraction::one();
        let zero = Fraction::zero();
        let two = Fraction::one() + Fraction::one();
        let (onehalf, otherhalf) = Fraction::one().halve();
        let num_of_men = two + onehalf;
        assert_eq!(format!("{num_of_men}"), "10.1b2");
        assert_eq!(format!("{otherhalf}"), "0.1b2");
        assert_eq!(format!("{one}"), "1b2");
        assert_eq!(format!("{zero}"), "0b2");
        let three = num_of_men + otherhalf;
        assert_eq!(format!("{three}"), "11b2");
    }

    #[test]
    pub fn test_fractions_eq() {
        let frac1: Fraction = Fraction { num: 4u32.into(), denom: 0 };
        let frac2: Fraction = Fraction { num: 8u32.into(), denom: 1 };
        let frac3: Fraction = Fraction { num: 17u32.into(), denom: 2 };
        let frac4: Fraction = Fraction { num: 34u32.into(), denom: 3 };
        assert_eq!(frac1, frac1);
        assert_eq!(frac1, frac2);
        assert_ne!(frac1, frac3);
        assert_ne!(frac1, frac4);
        assert_eq!(frac2, frac1);
        assert_eq!(frac2, frac2);
        assert_ne!(frac2, frac3);
        assert_ne!(frac2, frac4);
        assert_ne!(frac3, frac1);
        assert_ne!(frac3, frac2);
        assert_eq!(frac3, frac3);
        assert_eq!(frac3, frac4);
        assert_ne!(frac4, frac1);
        assert_ne!(frac4, frac2);
        assert_eq!(frac4, frac3);
        assert_eq!(frac4, frac4);
    }

    #[test]
    pub fn test_fractions_cmp() {
        let frac1: Fraction = Fraction { num: 4u32.into(), denom: 0 };
        let frac2: Fraction = Fraction { num: 8u32.into(), denom: 1 };
        let frac3: Fraction = Fraction { num: 17u32.into(), denom: 2 };
        let frac4: Fraction = Fraction { num: 34u32.into(), denom: 3 };
        assert_eq!(frac1.cmp(&frac1), Ordering::Equal);
        assert_eq!(frac1.cmp(&frac2), Ordering::Equal);
        assert_eq!(frac1.cmp(&frac3), Ordering::Less);
        assert_eq!(frac1.cmp(&frac4), Ordering::Less);
        assert_eq!(frac2.cmp(&frac1), Ordering::Equal);
        assert_eq!(frac2.cmp(&frac2), Ordering::Equal);
        assert_eq!(frac2.cmp(&frac3), Ordering::Less);
        assert_eq!(frac2.cmp(&frac4), Ordering::Less);
        assert_eq!(frac3.cmp(&frac1), Ordering::Greater);
        assert_eq!(frac3.cmp(&frac2), Ordering::Greater);
        assert_eq!(frac3.cmp(&frac3), Ordering::Equal);
        assert_eq!(frac3.cmp(&frac4), Ordering::Equal);
        assert_eq!(frac4.cmp(&frac1), Ordering::Greater);
        assert_eq!(frac4.cmp(&frac2), Ordering::Greater);
        assert_eq!(frac4.cmp(&frac3), Ordering::Equal);
        assert_eq!(frac4.cmp(&frac4), Ordering::Equal);
    }
}
