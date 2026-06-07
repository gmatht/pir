//! Hybrid numeric type: exact rationals and approximate `f64` (transcendentals, IEEE-only ops).

use num_bigint::BigInt;
use num_complex::Complex64;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cmp::Ordering;

/// Spreadsheet number: either an exact reduced rational, or a host float.
#[derive(Clone, Debug, PartialEq)]
pub enum Number {
    Exact(BigRational),
    Approx(f64),
    Complex(Complex64),
}

impl Number {
    pub fn exact_zero() -> Self {
        Number::Exact(BigRational::zero())
    }

    pub fn one() -> Self {
        Number::Exact(BigRational::one())
    }

    pub fn approx(f: f64) -> Self {
        Number::Approx(f)
    }

    /// Plain integer in formulas / cells.
    pub fn from_i64(n: i64) -> Self {
        Number::Exact(BigRational::from_integer(n.into()))
    }

    /// Non-zero finite float from builtins (PI, etc.).
    pub fn from_f64_unchecked(f: f64) -> Self {
        Number::Approx(f)
    }

    pub fn from_complex_unchecked(re: f64, im: f64) -> Self {
        Number::Complex(Complex64::new(re, im))
    }

    pub fn is_nan(&self) -> bool {
        match self {
            Number::Exact(_) => false,
            Number::Approx(f) => f.is_nan(),
            Number::Complex(c) => c.re.is_nan() || c.im.is_nan(),
        }
    }

    pub fn to_f64(&self) -> f64 {
        match self {
            Number::Exact(r) => r.to_f64().unwrap_or(f64::NAN),
            Number::Approx(f) => *f,
            Number::Complex(c) => {
                if c.im == 0.0 {
                    c.re
                } else {
                    f64::NAN
                }
            }
        }
    }

    pub fn to_complex64(&self) -> Complex64 {
        match self {
            Number::Exact(r) => Complex64::new(r.to_f64().unwrap_or(f64::NAN), 0.0),
            Number::Approx(f) => Complex64::new(*f, 0.0),
            Number::Complex(c) => *c,
        }
    }

    pub fn is_finite(&self) -> bool {
        match self {
            Number::Exact(_) => true,
            Number::Approx(f) => f.is_finite(),
            Number::Complex(c) => c.re.is_finite() && c.im.is_finite(),
        }
    }

    /// `true` for NaN, signed zero, or any value whose `f64` rounds to 0.0.
    /// Used for truthy / IF cond-style checks.
    pub fn is_zeroish(&self) -> bool {
        !self.is_nan() && self.to_f64() == 0.0
    }

    /// Negation; `-Exact` stays exact.
    pub fn neg(self) -> Self {
        match self {
            Number::Exact(r) => Number::Exact(-r),
            Number::Approx(f) => Number::Approx(-f),
            Number::Complex(c) => Number::Complex(-c),
        }
    }

    pub fn add(self, b: Number) -> Self {
        match (self, b) {
            (Number::Exact(ra), Number::Exact(rb)) => Number::Exact(ra + rb),
            (Number::Complex(a), Number::Complex(b)) => Number::Complex(a + b),
            (Number::Complex(a), b) => Number::Complex(a + b.to_complex64()),
            (a, Number::Complex(b)) => Number::Complex(a.to_complex64() + b),
            (a, b) => Number::Approx(a.to_f64() + b.to_f64()),
        }
    }

    pub fn sub(self, b: Number) -> Self {
        match (self, b) {
            (Number::Exact(ra), Number::Exact(rb)) => Number::Exact(ra - rb),
            (Number::Complex(a), Number::Complex(b)) => Number::Complex(a - b),
            (Number::Complex(a), b) => Number::Complex(a - b.to_complex64()),
            (a, Number::Complex(b)) => Number::Complex(a.to_complex64() - b),
            (a, b) => Number::Approx(a.to_f64() - b.to_f64()),
        }
    }

    pub fn mul(self, b: Number) -> Self {
        match (self, b) {
            (Number::Exact(ra), Number::Exact(rb)) => Number::Exact(ra * rb),
            (Number::Complex(a), Number::Complex(b)) => Number::Complex(a * b),
            (Number::Complex(a), b) => Number::Complex(a * b.to_complex64()),
            (a, Number::Complex(b)) => Number::Complex(a.to_complex64() * b),
            (a, b) => Number::Approx(a.to_f64() * b.to_f64()),
        }
    }

    pub fn div(self, b: Number) -> Self {
        match b {
            Number::Exact(ref r) if r.is_zero() => Number::Approx(f64::NAN),
            Number::Approx(y) if y == 0.0 => Number::Approx(f64::NAN),
            Number::Complex(c) if c == Complex64::new(0.0, 0.0) => {
                Number::Complex(Complex64::new(f64::NAN, f64::NAN))
            }
            Number::Complex(c) => Number::Complex(self.to_complex64() / c),
            Number::Exact(rb) => match self {
                Number::Exact(ra) => Number::Exact(ra / rb),
                Number::Approx(x) => Number::Approx(x / rb.to_f64().unwrap_or(f64::NAN)),
                Number::Complex(a) => Number::Complex(a / Complex64::new(rb.to_f64().unwrap_or(f64::NAN), 0.0)),
            },
            Number::Approx(y) => match self {
                Number::Exact(ra) => Number::Approx(ra.to_f64().unwrap_or(f64::NAN) / y),
                Number::Approx(x) => Number::Approx(x / y),
                Number::Complex(a) => Number::Complex(a / Complex64::new(y, 0.0)),
            },
        }
    }

    /// Power operator `^` in formulas: exact integer exponents on exact bases; else float.
    pub fn pow(self, exp: Number) -> Self {
        if let (Number::Exact(base), Number::Exact(e)) = (&self, &exp) {
            if e.is_integer() {
                if let Some(n) = e.numer().to_i32() {
                    if n >= 0 {
                        if let Some(p) = int_pow_rational_nonnegative(base, n as u32) {
                            return Number::Exact(p);
                        }
                    } else {
                        let exp_u = n.unsigned_abs();
                        let p = int_pow_rational_nonnegative(
                            &base.clone().recip(),
                            exp_u,
                        );
                        if let Some(p) = p {
                            return Number::Exact(p);
                        }
                    }
                }
            }
        }
        let x = self.to_f64();
        let y = exp.to_f64();
        if x == 0.0 && y < 0.0 {
            return Number::Approx(f64::INFINITY);
        }
        let out = x.powf(y);
        if out.is_nan() {
            let complex_out = self.to_complex64().powc(exp.to_complex64());
            Number::Complex(complex_out)
        } else {
            Number::Approx(out)
        }
    }

    /// Used when a builtin needs IEEE semantics end-to-end.
    pub fn apply_unary_f64(self, f: fn(f64) -> f64) -> Self {
        Number::Approx(f(self.to_f64()))
    }

    pub fn apply_unary_f64_with_complex_fallback(
        self,
        real: fn(f64) -> f64,
        complex: fn(Complex64) -> Complex64,
    ) -> Self {
        let x = self.to_f64();
        let out = real(x);
        if out.is_nan() {
            Number::Complex(complex(self.to_complex64()))
        } else {
            Number::Approx(out)
        }
    }

    pub fn apply_binary_f64_with_complex_fallback(
        self,
        rhs: Number,
        real: fn(f64, f64) -> f64,
        complex: fn(Complex64, Complex64) -> Complex64,
    ) -> Self {
        let x = self.to_f64();
        let y = rhs.to_f64();
        let out = real(x, y);
        if out.is_nan() {
            Number::Complex(complex(self.to_complex64(), rhs.to_complex64()))
        } else {
            Number::Approx(out)
        }
    }

    /// Short string for re-serializing formula literals.
    pub fn to_formula_string(&self) -> String {
        match self {
            Number::Exact(r) => rational_to_formula_literal(r),
            Number::Approx(f) => f.to_string(),
            Number::Complex(c) => format_complex(*c, &mut |v| v.to_string()),
        }
    }

    /// Absolute value; preserves [`Number::Exact`] when the input is exact.
    pub fn abs(self) -> Self {
        match self {
            Number::Exact(r) => Number::Exact(r.abs()),
            Number::Approx(f) => Number::Approx(f.abs()),
            Number::Complex(c) => Number::Approx(c.norm()),
        }
    }

    /// Cell/evaluation display strings: [`Number::Exact`] uses rational formatting (no `f64`
    /// round-trip) for human-scale values; extremes use exponential form like `format_approx` on
    /// floats. Approximate numbers use `format_approx` directly.
    pub fn format_eval_display(&self, format_approx: impl FnMut(f64) -> String) -> String {
        let mut format_approx = format_approx;
        match self {
            Number::Exact(r) => {
                if prefer_scientific_exact(r) {
                    if let Some(s) = exact_decimal_generic_scientific(&Number::Exact(r.clone())) {
                        return s;
                    }
                }
                rational_to_formula_literal(r)
            }
            Number::Approx(f) => format_approx(*f),
            Number::Complex(c) => format_complex(*c, &mut format_approx),
        }
    }
}

fn prefer_scientific_f64_abs(abs: f64) -> bool {
    if !abs.is_finite() {
        return true;
    }
    if abs == 0.0 {
        return false;
    }
    !(1e-4..1e10).contains(&abs)
}

fn prefer_scientific_exact(r: &BigRational) -> bool {
    if r.is_zero() {
        return false;
    }
    let Some(f) = r.to_f64().filter(|v| v.is_finite()) else {
        return true;
    };
    if f == 0.0 {
        return true;
    }
    prefer_scientific_f64_abs(f.abs())
}

fn format_complex(c: Complex64, format_approx: &mut impl FnMut(f64) -> String) -> String {
    let re_part = if c.re.abs() < 1e-12 { 0.0 } else { c.re };
    let im_part = if c.im.abs() < 1e-12 { 0.0 } else { c.im };
    let re = format_approx(re_part);
    let im = format_approx(im_part.abs());
    if im_part < 0.0 {
        format!("{re}-{im}i")
    } else {
        format!("{re}+{im}i")
    }
}


/// Multiply by `10^exp` (`exp` clamped modestly): `exp >= 1` ⇒ integer power-of-ten scale;
/// `exp <= -1` ⇒ division by matching power of ten (for `ROUND`, etc.).
pub(crate) fn pow10_rational(mut exp: i32) -> BigRational {
    const LIM: i32 = 30;
    if exp > LIM {
        exp = LIM;
    } else if exp < -LIM {
        exp = -LIM;
    }
    if exp >= 0 {
        BigRational::from_integer(BigInt::from(10u32).pow(exp as u32))
    } else {
        BigRational::new(BigInt::one(), BigInt::from(10u32).pow((-exp) as u32))
    }
}

/// Nearest [`BigInt`] to canonical `r`, tie-breaking halves away from zero (matches `ROUND`).
pub(crate) fn rat_round_half_away_to_integer(r: &BigRational) -> BigInt {
    let n = r.numer();
    let d = r.denom();
    if n.is_zero() || d.is_zero() {
        return BigInt::from(0u8);
    }
    assert!(d > &BigInt::from(0u8));
    if n > &BigInt::from(0u8) {
        (n + d / BigInt::from(2u8)) / d
    } else {
        -(((-n) + d / BigInt::from(2u8)) / d)
    }
}

/// `ROUND(x, digits)` scaling for spreadsheet-style rounding (nearest, ties away from zero).
pub(crate) fn round_rational_decimal_places(r: &BigRational, digits: i32) -> BigRational {
    let factor = pow10_rational(digits);
    let scaled = r * &factor;
    let q = rat_round_half_away_to_integer(&scaled);
    BigRational::from_integer(q) / factor
}

/// Remainder from truncating quotient (toward zero), matching `%`/`fmod` behavior for floats.
pub(crate) fn mod_trunc_toward_zero(a: &BigRational, divisor: &BigRational) -> Option<BigRational> {
    if divisor.is_zero() {
        return None;
    }
    let quotient_numer = a.numer() * divisor.denom();
    let quotient_den = a.denom() * divisor.numer();
    if quotient_den.is_zero() {
        return Some(BigRational::new(BigInt::from(0u8), BigInt::from(1u8)));
    }
    let q = quotient_numer / quotient_den;
    Some(a.clone() - (divisor * BigRational::from_integer(q)))
}

/// `|p/q|` compared to `10^exp` (integer `exp`), using only big-int scaling (no `f64` rounding).
fn cmp_abs_rational_to_pow10(p: &BigInt, q: &BigInt, exp: i128) -> Ordering {
    let p_abs = p.abs();
    let q_abs = q.abs();
    if exp >= 0 {
        let ten_e = BigInt::from(10u32).pow(exp as u32);
        p_abs.cmp(&(q_abs * ten_e))
    } else {
        let ten_e = BigInt::from(10u32).pow((-exp) as u32);
        (p_abs * ten_e).cmp(&q_abs)
    }
}

fn rat_pow10_i128(exp: i128) -> Option<BigRational> {
    const LIM: i128 = 120_000;
    if exp < -LIM || exp > LIM {
        return None;
    }
    Some(if exp >= 0 {
        BigRational::from_integer(BigInt::from(10u32).pow(exp as u32))
    } else {
        BigRational::new(
            BigInt::one(),
            BigInt::from(10u32).pow((-exp) as u32),
        )
    })
}

/// Largest integer `e` with `|r| >= 10^e` (i.e. `floor(log10 |r|)` for positive rationals).
fn floor_log10_positive_rational(r: &BigRational) -> Option<i128> {
    if r.is_zero() {
        return None;
    }
    let p = r.numer();
    let q = r.denom();
    if p.is_zero() || q.is_zero() {
        return None;
    }
    let lp = p.abs().to_string().len() as i128 - 1;
    let lq = q.abs().to_string().len() as i128 - 1;
    let est = lp - lq;
    let mut lo = (est - 35).max(-120_000);
    let mut hi = (est + 35).min(120_000);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        match cmp_abs_rational_to_pow10(p, q, mid) {
            Ordering::Less => hi = mid - 1,
            Ordering::Equal | Ordering::Greater => lo = mid,
        }
    }
    Some(lo)
}

/// Scientific `me{exp}` notation for Generic (decimal) display when the value is [`Number::Exact`].
///
/// Needed for extremes like `=10^-999` where truncating numerator/denominator to `f64` fails and
/// `to_f64()` underflows to `0`.
pub(crate) fn exact_decimal_generic_scientific(n: &Number) -> Option<String> {
    let Number::Exact(r) = n else {
        return None;
    };
    if r.is_zero() || r.numer().is_zero() {
        return Some("0".to_string());
    }
    let sign = if r.is_negative() { "-" } else { "" };
    let r_pos = r.abs();
    let exponent = floor_log10_positive_rational(&r_pos)?;
    let ten_to_exp = rat_pow10_i128(exponent)?;
    let ten = BigRational::from_integer(BigInt::from(10u32));
    let one = BigRational::one();

    let mut mant = &r_pos / &ten_to_exp;
    let mut exp_out = exponent;
    while mant >= ten {
        mant = mant / &ten;
        exp_out += 1;
    }
    while mant < one {
        mant = mant * &ten;
        exp_out -= 1;
    }

    let mf = mant.to_f64().filter(|v| v.is_finite() && *v > 0.0)?;
    let mant_str = format!("{mf:.15}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string();
    Some(format!("{sign}{mant_str}e{exp_out}"))
}

/// Parse a formula literal: digits, optional one `.` (no exponent). Produces an exact rational.
pub fn parse_number_literal(s: &str) -> Option<Number> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(c) = parse_complex_literal(t) {
        return Some(Number::Complex(c));
    }
    if t.matches('.').count() > 1 {
        return None;
    }
    let body = t.strip_prefix('-').unwrap_or(t);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    parse_decimal_rational(t).map(Number::Exact)
}

fn parse_complex_literal(s: &str) -> Option<Complex64> {
    let imag_body = s.strip_suffix('i')?;
    let split = imag_body
        .char_indices()
        .skip(1)
        .filter(|(_, ch)| *ch == '+' || *ch == '-')
        .map(|(idx, _)| idx)
        .last()?;
    let (re_s, im_s) = imag_body.split_at(split);
    let re = parse_decimal_rational(re_s)?.to_f64()?;
    let im = parse_decimal_rational(im_s.strip_prefix('+').unwrap_or(im_s))?.to_f64()?;
    Some(Complex64::new(re, im))
}

/// Decimal string to rational (e.g. `0.1` → 1/10). Leading `-` for negative integers/decimals.
fn parse_decimal_rational(s: &str) -> Option<BigRational> {
    if s.is_empty() {
        return None;
    }
    let (sign, rest) = if s.starts_with('-') {
        (-1i8, s.get(1..).unwrap_or(""))
    } else {
        (1i8, s)
    };
    if rest.is_empty() {
        return None;
    }
    if let Some(pos) = rest.find('.') {
        let int_s = &rest[..pos];
        let frac = &rest[pos + 1..];
        if !frac.is_empty() && !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if !int_s.is_empty() && !int_s.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let int_part: BigInt = if int_s.is_empty() {
            0u8.into()
        } else {
            int_s.parse().ok()?
        };
        let frac_int: BigInt = if frac.is_empty() {
            0u8.into()
        } else {
            frac.parse().ok()?
        };
        let k = frac.len() as u32;
        let denom = BigInt::from(10u8).pow(k);
        let num = int_part * &denom + frac_int;
        let mut r = BigRational::new(num, denom);
        if sign < 0 {
            r = -r;
        }
        Some(r)
    } else {
        if !rest.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let n: BigInt = rest.parse().ok()?;
        let mut r = BigRational::from_integer(n);
        if sign < 0 {
            r = -r;
        }
        Some(r)
    }
}

/// Render exact rationals in a way that round-trips with the parser (decimal when possible in reasonable space).
fn rational_to_formula_literal(r: &BigRational) -> String {
    if *r.denom() == BigInt::one() {
        return r.numer().to_string();
    }
    if is_denominator_powers_of_2_and_5(r.denom()) {
        if let Some(s) = decimal_string_fixed(r, 20) {
            return s;
        }
    }
    // Terminating decimals did not succeed: show canonical reduced numerator/denominator (exact).
    let n = r.numer();
    let d = r.denom();
    if *d != BigInt::one() && !n.is_zero() {
        format!("{}/{d}", n)
    } else if n.is_zero() {
        "0".to_string()
    } else {
        format!(
            "{:.15}",
            r.to_f64().unwrap_or(f64::NAN)
        )
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
    }
}

fn is_denominator_powers_of_2_and_5(d: &BigInt) -> bool {
    let mut n = d.clone();
    for p in [2i64, 5] {
        let p_big = BigInt::from(p);
        while &n % &p_big == BigInt::from(0u8) {
            n /= &p_big;
        }
    }
    n == BigInt::one() || n == BigInt::from(0u8)
}

/// Fixed-point string for r when denom divides 10^k.
fn decimal_string_fixed(r: &BigRational, max_decimals: usize) -> Option<String> {
    let a = r.numer();
    let b = r.denom();
    let sign = if a < &BigInt::from(0) { -1i8 } else { 1i8 };
    let abs_a = a.abs();
    for k in 0..=max_decimals {
        let ten_k = BigInt::from(10u8).pow(k as u32);
        if &ten_k % b != BigInt::from(0) {
            continue;
        }
        let m = &ten_k / b;
        let t = &abs_a * m;
        let int_val = t.clone() / &ten_k;
        let frac = &t % &ten_k;
        let mut s = if sign < 0 {
            format!("-{}", int_val)
        } else {
            int_val.to_string()
        };
        if k > 0 {
            s.push('.');
            let frac_s = format!("{:0>width$}", frac, width = k);
            s.push_str(frac_s.trim_end_matches('0'));
            if s.ends_with('.') {
                s.pop();
            }
        }
        return Some(s);
    }
    None
}

fn int_pow_rational_nonnegative(r: &BigRational, n: u32) -> Option<BigRational> {
    if n == 0 {
        return Some(BigRational::one());
    }
    if n == 1 {
        return Some(r.clone());
    }
    let p = n;
    if r.numer().is_zero() {
        return Some(BigRational::zero());
    }
    Some(BigRational::new(
        r.numer().pow(p),
        r.denom().pow(p),
    ))
}

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Number::Exact(r) => write!(f, "{}", rational_to_formula_literal(r)),
            Number::Approx(v) => write!(f, "{v}"),
            Number::Complex(c) => write!(f, "{}", format_complex(*c, &mut |v| v.to_string())),
        }
    }
}

impl PartialOrd for Number {
    /// Compare rationals vs floats using each [`f64`]’s exact IEEE rational when possible, so ordering
    /// does not silently collapse unlike values that share the same `f64`.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        match (self, other) {
            (Number::Exact(a), Number::Exact(b)) => Some(a.cmp(b)),
            (Number::Approx(x), Number::Approx(y)) => x.partial_cmp(y),
            (Number::Exact(r), Number::Approx(f)) => cmp_exact_vs_ieee_float(r, f),
            (Number::Approx(f), Number::Exact(r)) => cmp_exact_vs_ieee_float(r, f).map(Ordering::reverse),
            (Number::Complex(a), Number::Complex(b)) => {
                if a.im == 0.0 && b.im == 0.0 {
                    a.re.partial_cmp(&b.re)
                } else {
                    None
                }
            }
            (Number::Complex(a), b) => {
                if a.im == 0.0 {
                    a.re.partial_cmp(&b.to_f64())
                } else {
                    None
                }
            }
            (a, Number::Complex(b)) => {
                if b.im == 0.0 {
                    a.to_f64().partial_cmp(&b.re)
                } else {
                    None
                }
            }
        }
    }
}

fn cmp_exact_vs_ieee_float(r: &BigRational, f: &f64) -> Option<std::cmp::Ordering> {
    if f.is_nan() {
        return None;
    }
    match BigRational::from_float(*f) {
        Some(fr) => Some(r.cmp(&fr)),
        None => match r.to_f64() {
            Some(rf) => rf.partial_cmp(f),
            None => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_tenth_is_exact() {
        let n = parse_number_literal("0.1").unwrap();
        match n {
            Number::Exact(r) => {
                assert_eq!(r, BigRational::new(1.into(), 10.into()));
            }
            _ => panic!("expected exact"),
        }
    }

    #[test]
    fn tenth_plus_tenth_is_fifth() {
        let a = parse_number_literal("0.1").unwrap();
        let b = parse_number_literal("0.1").unwrap();
        let s = a.add(b);
        match s {
            Number::Exact(r) => {
                assert_eq!(r, BigRational::new(1.into(), 5.into()));
            }
            _ => panic!("expected exact sum"),
        }
    }

    #[test]
    fn partial_cmp_exact_decimal_neq_approx_ieee_same_literal() {
        let exact = parse_number_literal("0.1").unwrap();
        let approx = Number::from_f64_unchecked(0.1);
        assert_ne!(exact.partial_cmp(&approx), Some(std::cmp::Ordering::Equal));
    }

    #[test]
    fn partial_cmp_exact_vs_approx_matches_exact_rational_to_float_bits() {
        let exact = parse_number_literal("0.1").unwrap();
        let r = match &exact {
            Number::Exact(rr) => rr,
            _ => panic!("expected Exact"),
        };
        let fr = BigRational::from_float(0.1_f64).expect(" finite f64 rational ");
        assert_eq!(
            exact.partial_cmp(&Number::from_f64_unchecked(0.1)),
            Some(r.cmp(&fr))
        );
    }

    #[test]
    fn pow_falls_back_to_complex_for_real_domain_error() {
        let out = Number::from_i64(-1).pow(Number::from_f64_unchecked(0.5));
        match out {
            Number::Complex(c) => {
                assert!(c.re.abs() < 1e-9);
                assert!((c.im.abs() - 1.0).abs() < 1e-9);
            }
            other => panic!("expected complex, got {other:?}"),
        }
    }

    #[test]
    fn complex_display_uses_a_plus_bi_shape() {
        let n = Number::from_complex_unchecked(2.5, -1.25);
        assert_eq!(n.to_formula_string(), "2.5-1.25i");
        assert_eq!(n.format_eval_display(|v| format!("{v:.2}")), "2.50-1.25i");
    }

    #[test]
    fn complex_arithmetic_propagates() {
        let a = Number::from_complex_unchecked(0.0, 1.0);
        let b = Number::from_i64(2);
        match a.add(b) {
            Number::Complex(c) => {
                assert_eq!(c.re, 2.0);
                assert_eq!(c.im, 1.0);
            }
            other => panic!("expected complex, got {other:?}"),
        }
    }

    #[test]
    fn parse_number_literal_supports_a_plus_bi() {
        let n = parse_number_literal("12.5+3.75i").expect("complex literal");
        match n {
            Number::Complex(c) => {
                assert_eq!(c.re, 12.5);
                assert_eq!(c.im, 3.75);
            }
            other => panic!("expected complex, got {other:?}"),
        }
    }

    #[test]
    fn parse_number_literal_supports_a_minus_bi() {
        let n = parse_number_literal("12.5-3.75i").expect("complex literal");
        match n {
            Number::Complex(c) => {
                assert_eq!(c.re, 12.5);
                assert_eq!(c.im, -3.75);
            }
            other => panic!("expected complex, got {other:?}"),
        }
    }

    #[test]
    fn decimal_generic_sci_one_over_pow10_999() {
        let denom = BigInt::from(10u32).pow(999);
        let r = BigRational::new(BigInt::one(), denom);
        let s = exact_decimal_generic_scientific(&Number::Exact(r)).expect("sci display");
        assert!(s.starts_with('1'), "{s}");
        assert!(s.contains("e-999"), "{s}");
    }
}
