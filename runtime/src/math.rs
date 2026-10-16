//! Numeric runtime helpers for operators the IR lowers to a call rather than an
//! inline word op: today only `**` (exponentiation).
//!
//! The port targets the runtime's `nostd` (freestanding) build, so nothing here
//! may touch `std`/`libm`: `f64::powf`, `f64::log2`, `f64::exp2`, `f64::floor`
//! and friends live in `std`, not `core`. Everything below is spelled out with
//! ordinary arithmetic plus `to_bits`/`from_bits` (both `core`), so the same
//! sequence of IEEE operations can be mirrored operation-for-operation by the
//! ported runtime in `pickle/runtime/abi_math.pkl`. That mirroring is the point:
//! a single source of truth for the algorithm keeps the two runtimes bit-equal.

/// `base ** exp` for `int` operands, by binary exponentiation.
///
/// Overflow wraps exactly like the inline `Mul` op (a plain `imul`), so `**`
/// never panics and agrees with repeated multiplication. A negative exponent
/// yields the truncating integer reciprocal: `1` for a base of `1`, `±1` for a
/// base of `-1` (sign by parity), and `0` otherwise (every `|base| >= 2` rounds
/// toward zero), which is the only value representable in `int`.
pub extern "C" fn pickle_int_pow(base: i64, exp: i64) -> i64 {
    if exp < 0 {
        return match base {
            1 => 1,
            -1 => {
                if exp & 1 == 0 {
                    1
                } else {
                    -1
                }
            }
            _ => 0,
        };
    }
    let mut result: i64 = 1;
    let mut b = base;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            result = result.wrapping_mul(b);
        }
        e >>= 1;
        if e > 0 {
            b = b.wrapping_mul(b);
        }
    }
    result
}

/// `floor(x)` for finite `x`, using only a truncating int cast.
fn floor_f(x: f64) -> f64 {
    let t = x as i64 as f64;
    if t > x {
        t - 1.0
    } else {
        t
    }
}

/// Is `x` an exact integer (and not NaN/infinite)?
fn is_integral(x: f64) -> bool {
    if !x.is_finite() {
        return false;
    }
    if x.abs() >= 9.007199254740992e15 {
        // Every f64 with magnitude >= 2^53 is already an integer.
        return true;
    }
    x == (x as i64 as f64)
}

/// Is the (integral) value `x` odd? Only meaningful for integers.
fn is_odd(x: f64) -> bool {
    if x.abs() >= 9.007199254740992e15 {
        return false;
    }
    (x as i64) & 1 != 0
}

/// `2^n` for `n` in `-1074..=1024`, built by repeated doubling/halving.
///
/// Every intermediate is a power of two and powers of two multiply exactly, so
/// the scaling in `exp2_f` introduces no rounding of its own; the endpoints
/// reach the smallest subnormal and overflow to infinity respectively.
fn pow2_int(n: i64) -> f64 {
    let mut result = 1.0f64;
    if n >= 0 {
        let mut k = n;
        while k > 0 {
            result *= 2.0;
            k -= 1;
        }
    } else {
        let mut k = -n;
        while k > 0 {
            result *= 0.5;
            k -= 1;
        }
    }
    result
}

const LN_2: f64 = 0.6931471805599453;
const INV_LN_2: f64 = 1.4426950408889634;
const SQRT_2: f64 = 1.4142135623730951;

/// `log2(x)` for finite `x > 0`, via frexp-style range reduction and the
/// `atanh` series `ln(m) = 2 * atanh((m-1)/(m+1))` on `m` in `[1/sqrt2, sqrt2)`.
fn log2_f(x: f64) -> f64 {
    let mut bits = x.to_bits();
    let mut e: i64 = ((bits >> 52) & 0x7ff) as i64 - 1023;
    if e == -1023 {
        // Subnormal: renormalise by 2^52 and account for the shift.
        let scaled = x * 4503599627370496.0; // 2^52
        bits = scaled.to_bits();
        e = (((bits >> 52) & 0x7ff) as i64 - 1023) - 52;
    }
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if m > SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    // Horner over odd reciprocal coefficients 1/(2k+1), k = 8..0.
    let mut acc = 1.0 / 17.0;
    acc = acc * z2 + 1.0 / 15.0;
    acc = acc * z2 + 1.0 / 13.0;
    acc = acc * z2 + 1.0 / 11.0;
    acc = acc * z2 + 1.0 / 9.0;
    acc = acc * z2 + 1.0 / 7.0;
    acc = acc * z2 + 1.0 / 5.0;
    acc = acc * z2 + 1.0 / 3.0;
    acc = acc * z2 + 1.0;
    let ln_m = 2.0 * z * acc;
    (e as f64) + ln_m * INV_LN_2
}

/// `2^t` for finite `t`, as `2^floor(t) * 2^frac`, the fractional part from a
/// degree-17 Taylor series of `exp(frac * ln2)`.
fn exp2_f(t: f64) -> f64 {
    if t > 1024.0 {
        return f64::INFINITY;
    }
    if t < -1075.0 {
        return 0.0;
    }
    let n = floor_f(t);
    let f = t - n;
    let u = f * LN_2;
    // Horner over 1/k!, k = 17..0.
    let mut acc = 1.0 / 355687428096000.0;
    acc = acc * u + 1.0 / 20922789888000.0;
    acc = acc * u + 1.0 / 1307674368000.0;
    acc = acc * u + 1.0 / 87178291200.0;
    acc = acc * u + 1.0 / 6227020800.0;
    acc = acc * u + 1.0 / 479001600.0;
    acc = acc * u + 1.0 / 39916800.0;
    acc = acc * u + 1.0 / 3628800.0;
    acc = acc * u + 1.0 / 362880.0;
    acc = acc * u + 1.0 / 40320.0;
    acc = acc * u + 1.0 / 5040.0;
    acc = acc * u + 1.0 / 720.0;
    acc = acc * u + 1.0 / 120.0;
    acc = acc * u + 1.0 / 24.0;
    acc = acc * u + 1.0 / 6.0;
    acc = acc * u + 1.0 / 2.0;
    acc = acc * u + 1.0;
    acc = acc * u + 1.0;
    acc * pow2_int(n as i64)
}

/// `base ** exp` for `float` operands.
///
/// An integral exponent always takes the exact binary-exponentiation path (no
/// approximation), which is the common case (`x ** 2`, `2.0 ** 10`, `x ** -1`);
/// only a non-integral exponent falls through to `2^(exp * log2(base))`. A
/// negative base is legal only for integral exponents, matching IEEE `pow`
/// (which answers NaN otherwise).
pub extern "C" fn pickle_float_pow(base: f64, exp: f64) -> f64 {
    if base.is_nan() || exp.is_nan() {
        return f64::NAN;
    }
    if exp == 0.0 {
        return 1.0;
    }
    if base == 1.0 {
        return 1.0;
    }
    if exp == 1.0 {
        return base;
    }
    let y_int = is_integral(exp);
    let odd = is_odd(exp);
    let neg = base < 0.0 && odd;
    if base < 0.0 && !y_int {
        return f64::NAN;
    }
    let ax = if base < 0.0 { -base } else { base };
    if ax == 0.0 {
        let r = if exp > 0.0 { 0.0 } else { f64::INFINITY };
        return if neg { -r } else { r };
    }
    if y_int && exp.abs() < 9.0e18 {
        let r = powi_exact(ax, exp as i64);
        return if neg { -r } else { r };
    }
    let r = exp2_f(exp * log2_f(ax));
    if neg {
        -r
    } else {
        r
    }
}

/// `base^n` for `base > 0` and `n` an integer, exactly, by binary exponentiation.
fn powi_exact(base: f64, n: i64) -> f64 {
    if n == 0 {
        return 1.0;
    }
    let neg = n < 0;
    let mut e: u64 = if neg {
        (-(n as i128)) as u64
    } else {
        n as u64
    };
    let mut result = 1.0f64;
    let mut b = base;
    while e > 0 {
        if e & 1 == 1 {
            result *= b;
        }
        e >>= 1;
        if e > 0 {
            b *= b;
        }
    }
    if neg {
        1.0 / result
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_pow_matches_repeated_multiplication() {
        assert_eq!(pickle_int_pow(2, 10), 1024);
        assert_eq!(pickle_int_pow(3, 0), 1);
        assert_eq!(pickle_int_pow(5, 1), 5);
        assert_eq!(pickle_int_pow(-2, 3), -8);
        assert_eq!(pickle_int_pow(-2, 4), 16);
        assert_eq!(pickle_int_pow(0, 0), 1);
        assert_eq!(pickle_int_pow(1, -5), 1);
        assert_eq!(pickle_int_pow(-1, -5), -1);
        assert_eq!(pickle_int_pow(-1, -4), 1);
        assert_eq!(pickle_int_pow(2, -3), 0);
    }

    #[test]
    fn int_pow_wraps_like_mul() {
        let base = 1_000_000_000_000_000_007i64;
        let mut expect: i64 = 1;
        for _ in 0..5 {
            expect = expect.wrapping_mul(base);
        }
        assert_eq!(pickle_int_pow(base, 5), expect);
    }

    #[test]
    fn float_pow_integral_is_exact() {
        for &b in &[1.5f64, 2.0, 0.5, 3.25, 100.0] {
            for n in -6i64..=12 {
                let got = pickle_float_pow(b, n as f64);
                let want = b.powi(n as i32);
                assert_eq!(got.to_bits(), want.to_bits(), "b={b} n={n} got={got} want={want}");
            }
        }
    }

    #[test]
    fn pow_parity_vectors() {
        // Exact bit patterns the ported runtime (pickle/runtime/abi_math.pkl) emits
        // for these inputs; the two runtimes must agree bit-for-bit, not just to
        // within a tolerance.
        assert_eq!(pickle_int_pow(2, 10), 1024);
        assert_eq!(pickle_int_pow(-2, 3), -8);
        assert_eq!(pickle_int_pow(10, 18), 1_000_000_000_000_000_000);
        assert_eq!(pickle_float_pow(2.0, 0.5).to_bits(), 1.414213562373095f64.to_bits());
        assert_eq!(pickle_float_pow(9.0, 0.5).to_bits(), 2.9999999999999996f64.to_bits());
        assert_eq!(pickle_float_pow(2.0, 3.0).to_bits(), 8.0f64.to_bits());
    }

    #[test]
    fn float_pow_specials() {
        assert!(pickle_float_pow(f64::NAN, 2.0).is_nan());
        assert!(pickle_float_pow(2.0, f64::NAN).is_nan());
        assert_eq!(pickle_float_pow(2.0, 0.0), 1.0);
        assert_eq!(pickle_float_pow(1.0, 123.0), 1.0);
        assert_eq!(pickle_float_pow(2.0, 1.0), 2.0);
        assert!(pickle_float_pow(-2.0, 0.5).is_nan());
        assert_eq!(pickle_float_pow(-2.0, 3.0), -8.0);
        assert_eq!(pickle_float_pow(-2.0, 2.0), 4.0);
        assert_eq!(pickle_float_pow(0.0, 3.0), 0.0);
        assert!(pickle_float_pow(0.0, -1.0).is_infinite());
    }

    #[test]
    fn float_pow_non_integral_is_close_to_reference() {
        let cases = [
            (2.0f64, 0.5f64),
            (2.0, 10.0),
            (10.0, 0.30103),
            (0.5, 0.5),
            (7.0, 3.3),
            (1.0e6, 0.5),
            (12345.6789, -1.5),
        ];
        for (b, e) in cases {
            let got = pickle_float_pow(b, e);
            let want = b.powf(e);
            let rel = ((got - want) / want).abs();
            assert!(rel < 1e-12, "b={b} e={e} got={got} want={want} rel={rel}");
        }
    }
}
