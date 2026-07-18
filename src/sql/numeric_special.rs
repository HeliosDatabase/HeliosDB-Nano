//! PostgreSQL special-numeric (`NaN` / `±Infinity`) semantics for the
//! String-backed [`crate::Value::Numeric`] representation.
//!
//! `Value::Numeric` stores its payload as a canonical decimal string because
//! `rust_decimal::Decimal` — the type every finite numeric parses into —
//! *cannot* represent `NaN` or `±Infinity`. This module extends the stored
//! invariant with exactly three additional canonical payloads:
//!
//! * `"NaN"`
//! * `"Infinity"`
//! * `"-Infinity"`
//!
//! Those tokens are the *exact* text PostgreSQL emits, which is why every
//! store / clone / bincode / PG-wire / MySQL-wire / COPY / `Display` site that
//! treats the payload opaquely (it just moves the `String` around) stays
//! correct with **no** change — the tokens already ARE their own correct
//! output form. Only the sites that *interpret* the payload numerically
//! (cast, comparison, equality, sort, arithmetic, min/max/sum, pushdown
//! pruning) must be taught the tokens, and those route through the helpers
//! here.
//!
//! # PostgreSQL contract (`src/backend/utils/adt/numeric.c`)
//!
//! * `NaN` sorts GREATER than every non-NaN value, and — unlike IEEE-754 —
//!   `NaN = NaN` is TRUE / `NaN <> NaN` is FALSE. This is deliberate so NaN
//!   is usable in b-tree indexes, `GROUP BY`, `DISTINCT` and `ORDER BY`.
//! * `+Infinity` is greater than every finite value; `-Infinity` is less.
//! * Arithmetic propagates `NaN`; infinities follow IEEE rules except that
//!   `Inf - Inf`, `Inf / Inf` and `Inf * 0` are `NaN`, and `finite / Inf`
//!   is `0`. `Inf / 0` still raises division-by-zero (only `NaN / 0` = NaN).
//! * Text output is exactly `NaN` / `Infinity` / `-Infinity`.

use crate::{Error, Result, Value};
use rust_decimal::Decimal;
use std::cmp::Ordering;

/// Canonical PG text form of numeric `NaN`.
pub const NAN_TEXT: &str = "NaN";
/// Canonical PG text form of numeric `+Infinity`.
pub const POS_INF_TEXT: &str = "Infinity";
/// Canonical PG text form of numeric `-Infinity`.
pub const NEG_INF_TEXT: &str = "-Infinity";

/// One of the three non-finite numeric values PG's `numeric` type admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// Not-a-number.
    NaN,
    /// Positive infinity.
    PosInf,
    /// Negative infinity.
    NegInf,
}

impl Special {
    /// Canonical PG text token for this special value.
    pub fn to_text(self) -> &'static str {
        match self {
            Special::NaN => NAN_TEXT,
            Special::PosInf => POS_INF_TEXT,
            Special::NegInf => NEG_INF_TEXT,
        }
    }

    /// Wrap this special into its canonical `Value::Numeric`.
    pub fn to_value(self) -> Value {
        Value::Numeric(self.to_text().to_string())
    }

    /// IEEE-754 `f64` mirror, for the numeric→float cast path.
    pub fn to_f64(self) -> f64 {
        match self {
            Special::NaN => f64::NAN,
            Special::PosInf => f64::INFINITY,
            Special::NegInf => f64::NEG_INFINITY,
        }
    }

    /// Build an infinity from a sign (`true` → `-Infinity`).
    fn from_neg(neg: bool) -> Special {
        if neg {
            Special::NegInf
        } else {
            Special::PosInf
        }
    }

    /// True for `-Infinity` (used for sign math on infinities).
    fn is_neg(self) -> bool {
        matches!(self, Special::NegInf)
    }

    /// `ABS` per PG: `|NaN| = NaN`, `|±Inf| = +Inf`.
    pub fn abs(self) -> Special {
        match self {
            Special::NaN => Special::NaN,
            _ => Special::PosInf,
        }
    }

    /// PG total-order rank: `-Inf(0) < finite(1) < +Inf(2) < NaN(3)`.
    fn rank(self) -> i8 {
        match self {
            Special::NegInf => 0,
            Special::PosInf => 2,
            Special::NaN => 3,
        }
    }
}

/// Recognise one of the three special tokens from arbitrary user/stored text.
///
/// Accepts (after trimming) `NaN`, `Infinity`, `Inf` with an optional leading
/// `+`/`-`, case-insensitively — the exact set PG's `numeric_in` accepts.
/// Stored `Value::Numeric` payloads are always canonical, so this doubles as
/// the classifier for already-stored values.
pub fn parse_special(s: &str) -> Option<Special> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("nan") {
        return Some(Special::NaN);
    }
    // Optional leading sign, then inf / infinity.
    let (neg, rest) = if let Some(r) = t.strip_prefix('+') {
        (false, r)
    } else if let Some(r) = t.strip_prefix('-') {
        (true, r)
    } else {
        (false, t)
    };
    if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
        return Some(Special::from_neg(neg));
    }
    None
}

/// Classify a *stored* numeric payload (canonical), returning its special
/// kind or `None` when it is an ordinary finite decimal string.
#[inline]
pub fn special_of(s: &str) -> Option<Special> {
    // Fast path on the canonical tokens; fall back to the tolerant parser for
    // defensiveness (case / sign variants can never appear in stored data but
    // are cheap to accept).
    match s {
        "NaN" => Some(Special::NaN),
        "Infinity" => Some(Special::PosInf),
        "-Infinity" => Some(Special::NegInf),
        _ => parse_special(s),
    }
}

/// True iff the string payload is a special (`NaN`/`±Infinity`) token.
#[inline]
pub fn is_special(s: &str) -> bool {
    special_of(s).is_some()
}

/// Parse user text for a `NUMERIC`/`DECIMAL` literal or cast target.
///
/// Returns the canonical `Value::Numeric` payload string on success — a
/// special token for `NaN`/`±Infinity`, or `format!("{}", decimal)` for a
/// finite value (matching the pre-existing cast path exactly) — and `None`
/// for anything that is not a valid numeric (e.g. `"NotANumber"`), which the
/// caller turns into the usual cast error.
pub fn parse_numeric_text(s: &str) -> Option<String> {
    if let Some(sp) = parse_special(s) {
        return Some(sp.to_text().to_string());
    }
    // PG's numeric-in skips surrounding whitespace; `Decimal::from_str` does
    // not, so trim first (same rationale as the finite cast path).
    s.trim().parse::<Decimal>().ok().map(|d| format!("{}", d))
}

/// Canonical `Value::Numeric` payload for an `f64` (the float→numeric cast).
///
/// A finite float keeps its `Display` form (matching the pre-existing cast);
/// `NaN`/`±Infinity` map to the canonical PG tokens rather than Rust's
/// `"inf"`/`"-inf"`/`"NaN"` spellings, so a `'Infinity'::float8::numeric`
/// round-trips as `Infinity`, not the non-canonical `inf`.
pub fn float_to_numeric_text(f: f64) -> String {
    if f.is_nan() {
        NAN_TEXT.to_string()
    } else if f.is_infinite() {
        if f.is_sign_positive() {
            POS_INF_TEXT.to_string()
        } else {
            NEG_INF_TEXT.to_string()
        }
    } else {
        format!("{f}")
    }
}

/// Total-order comparator for two *stored* numeric strings, for the ORDER BY /
/// MIN / MAX / GROUP-BY sort path.
///
/// Two finite payloads compare **numerically** by exact decimal value (the
/// same `rust_decimal::Decimal` mechanism the evaluator's finite comparison
/// uses), so `"2" < "10" < "100"` as NUMBERS — NOT lexicographically, where
/// `"10" < "2"`. As soon as either side is special, PG rank ordering
/// (`-Inf < finite < +Inf < NaN`) takes over. This keeps `Ord` numeric while
/// the string-based `Value` `Hash`/`Eq` still agree with it, because a
/// canonical decimal string is unique per numeric value.
pub fn cmp_sort(a: &str, b: &str) -> Ordering {
    match (special_of(a), special_of(b)) {
        // Fall back to a byte compare only if a payload parses as neither an
        // exact `Decimal` nor an `f64` (never for canonical stored numerics),
        // so the result is always a total order.
        (None, None) => cmp_finite_numeric(a, b).unwrap_or_else(|| a.cmp(b)),
        (sa, sb) => rank_of(sa).cmp(&rank_of(sb)),
    }
}

/// Numeric (value, not lexicographic) ordering of two *finite* canonical
/// decimal strings.
///
/// Uses `rust_decimal::Decimal` — the exact type every finite `Value::Numeric`
/// parses into, and the mechanism the evaluator's `(Numeric, Numeric)`
/// comparison uses — so precision is preserved. Falls back to an `f64` compare
/// for the rare value outside `Decimal`'s ~28-digit range, and returns `None`
/// only when a payload parses as neither (a non-finite/garbage string, which a
/// canonical stored numeric never is).
fn cmp_finite_numeric(a: &str, b: &str) -> Option<Ordering> {
    if let (Ok(ad), Ok(bd)) = (a.parse::<Decimal>(), b.parse::<Decimal>()) {
        return Some(ad.cmp(&bd));
    }
    let af: f64 = a.parse().ok()?;
    let bf: f64 = b.parse().ok()?;
    af.partial_cmp(&bf)
}

fn rank_of(s: Option<Special>) -> i8 {
    match s {
        None => 1, // finite
        Some(sp) => sp.rank(),
    }
}

/// PG rank of any numeric-ish `Value`, or `None` if it is not numeric.
fn numeric_rank(v: &Value) -> Option<i8> {
    Some(match v {
        Value::Numeric(s) => rank_of(special_of(s)),
        Value::Float4(f) => float_rank(f64::from(*f)),
        Value::Float8(f) => float_rank(*f),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => 1,
        _ => return None,
    })
}

fn float_rank(f: f64) -> i8 {
    if f.is_nan() {
        3
    } else if f.is_infinite() {
        if f.is_sign_positive() {
            2
        } else {
            0
        }
    } else {
        1
    }
}

#[inline]
fn is_special_numeric(v: &Value) -> bool {
    matches!(v, Value::Numeric(s) if is_special(s))
}

/// PG operator-comparison ordering when a special `Value::Numeric` participates.
///
/// Returns `Some(ordering)` **only** when at least one operand is a special
/// `Value::Numeric` and both operands are numeric-comparable; otherwise `None`
/// so the caller's existing finite-path comparison runs unchanged. This is the
/// hook for both the `=/<>/</>/…` operators and `values_equal`, giving the PG
/// rules `NaN = NaN` (true), `NaN > +Inf > finite > -Inf`, and
/// `NaN`/`Inf` vs float `NaN`/`Inf` via matching ranks.
pub fn special_operator_cmp(left: &Value, right: &Value) -> Option<Ordering> {
    if !is_special_numeric(left) && !is_special_numeric(right) {
        return None;
    }
    let lr = numeric_rank(left)?;
    let rr = numeric_rank(right)?;
    Some(lr.cmp(&rr))
}

/// The four binary arithmetic operators special-numeric arithmetic covers.
#[derive(Debug, Clone, Copy)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
}

#[derive(Debug, Clone, Copy)]
enum Operand {
    Special(Special),
    Finite { neg: bool, zero: bool },
}

fn arith_operand(v: &Value) -> Option<Operand> {
    Some(match v {
        Value::Numeric(s) => match special_of(s) {
            Some(sp) => Operand::Special(sp),
            None => {
                let d = s.parse::<Decimal>().ok()?;
                let zero = d == Decimal::from(0);
                Operand::Finite {
                    neg: d.is_sign_negative() && !zero,
                    zero,
                }
            }
        },
        Value::Int2(i) => Operand::Finite {
            neg: *i < 0,
            zero: *i == 0,
        },
        Value::Int4(i) => Operand::Finite {
            neg: *i < 0,
            zero: *i == 0,
        },
        Value::Int8(i) => Operand::Finite {
            neg: *i < 0,
            zero: *i == 0,
        },
        _ => return None,
    })
}

/// PG arithmetic when a special `Value::Numeric` participates.
///
/// Fires **only** when at least one operand is a special `Value::Numeric` and
/// neither operand is a float (the evaluator's `Numeric × Float` arms already
/// parse the special tokens as `f64` natively and yield a float result). In
/// every other case returns `Ok(None)`, leaving the caller's finite Decimal
/// arithmetic untouched. `Inf / 0` yields a division-by-zero `Err`; `NaN / 0`
/// yields `NaN` (NaN dominates before the zero check).
pub fn special_arith(op: ArithOp, left: &Value, right: &Value) -> Result<Option<Value>> {
    if !is_special_numeric(left) && !is_special_numeric(right) {
        return Ok(None);
    }
    if matches!(left, Value::Float4(_) | Value::Float8(_)) || matches!(right, Value::Float4(_) | Value::Float8(_)) {
        return Ok(None);
    }
    let (l, r) = match (arith_operand(left), arith_operand(right)) {
        (Some(l), Some(r)) => (l, r),
        _ => return Ok(None),
    };
    compute_special(op, l, r).map(Some)
}

fn compute_special(op: ArithOp, l: Operand, r: Operand) -> Result<Value> {
    use Operand::{Finite, Special as Sp};

    // NaN dominates every operator (including `NaN / 0`), so test it first.
    if matches!(l, Sp(Special::NaN)) || matches!(r, Sp(Special::NaN)) {
        return Ok(Special::NaN.to_value());
    }

    // At least one operand is a (non-NaN) special — guaranteed by the caller —
    // so the `(Finite, Finite)` arms below are unreachable; they return NaN as
    // a non-panicking defensive default rather than `unreachable!()`.
    let out: Special = match op {
        ArithOp::Add => match (l, r) {
            (Sp(a), Sp(b)) => {
                if a == b {
                    a // +Inf + +Inf = +Inf ; -Inf + -Inf = -Inf
                } else {
                    Special::NaN // +Inf + -Inf = NaN
                }
            }
            (Sp(a), Finite { .. }) | (Finite { .. }, Sp(a)) => a,
            (Finite { .. }, Finite { .. }) => Special::NaN,
        },
        ArithOp::Sub => match (l, r) {
            (Sp(a), Sp(b)) => {
                if a == b {
                    Special::NaN // +Inf - +Inf = NaN ; -Inf - -Inf = NaN
                } else {
                    a // +Inf - -Inf = +Inf ; -Inf - +Inf = -Inf
                }
            }
            (Sp(a), Finite { .. }) => a,                              // Inf - finite = Inf
            (Finite { .. }, Sp(b)) => Special::from_neg(!b.is_neg()), // finite - Inf = -Inf
            (Finite { .. }, Finite { .. }) => Special::NaN,
        },
        ArithOp::Mul => match (l, r) {
            (Sp(a), Sp(b)) => Special::from_neg(a.is_neg() ^ b.is_neg()),
            (Sp(a), Finite { zero, neg }) | (Finite { zero, neg }, Sp(a)) => {
                if zero {
                    Special::NaN // Inf * 0 = NaN
                } else {
                    Special::from_neg(a.is_neg() ^ neg)
                }
            }
            (Finite { .. }, Finite { .. }) => Special::NaN,
        },
        ArithOp::Div => match (l, r) {
            (Sp(_), Sp(_)) => Special::NaN, // Inf / Inf = NaN
            (Sp(a), Finite { zero, neg }) => {
                if zero {
                    return Err(Error::query_execution("division by zero"));
                }
                Special::from_neg(a.is_neg() ^ neg)
            }
            (Finite { .. }, Sp(_)) => return Ok(Value::Numeric("0".to_string())), // finite / Inf = 0
            (Finite { .. }, Finite { .. }) => Special::NaN,
        },
    };
    Ok(out.to_value())
}

/// Fold the special contribution of a whole value set for `SUM`.
///
/// Returns `Some(special)` iff at least one value is a special numeric,
/// combining them by PG rules (`NaN` dominates; `+Inf` and `-Inf` together
/// give `NaN`; a lone infinity sign survives). Finite values never change a
/// special sum, so the caller only falls back to Decimal accumulation when
/// this returns `None`.
pub fn fold_sum_specials<'a, I>(values: I) -> Option<Special>
where
    I: IntoIterator<Item = &'a Value>,
{
    let mut acc: Option<Special> = None;
    for v in values {
        if let Value::Numeric(s) = v {
            if let Some(sp) = special_of(s) {
                acc = Some(combine_sum(acc, sp));
            }
        }
    }
    acc
}

/// Combine a running special-sum accumulator with an incoming special value.
pub fn combine_sum(acc: Option<Special>, incoming: Special) -> Special {
    match acc {
        None => incoming,
        Some(cur) => {
            if cur == Special::NaN || incoming == Special::NaN {
                Special::NaN
            } else if cur == incoming {
                cur // +Inf + +Inf = +Inf ; -Inf + -Inf = -Inf
            } else {
                Special::NaN // +Inf + -Inf = NaN
            }
        }
    }
}

/// PG-correct ordering of two numeric strings for predicate pushdown / zone
/// maps. Two finite payloads compare by exact **numeric** value (`Decimal`,
/// with an `f64` fallback for out-of-range magnitudes — strictly more precise
/// than the historical `f64`-only pushdown, and consistent with `cmp_sort` so
/// pruning never disagrees with the sort/scan comparators); any special uses
/// PG rank ordering. `None` only when a finite payload parses as neither
/// `Decimal` nor `f64`.
pub fn cmp_numeric_pushdown(a: &str, b: &str) -> Option<Ordering> {
    match (special_of(a), special_of(b)) {
        (None, None) => cmp_finite_numeric(a, b),
        (sa, sb) => Some(rank_of(sa).cmp(&rank_of(sb))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_special_case_and_sign_insensitively() {
        assert_eq!(parse_numeric_text("NaN").as_deref(), Some("NaN"));
        assert_eq!(parse_numeric_text("nan").as_deref(), Some("NaN"));
        assert_eq!(parse_numeric_text("  NAN ").as_deref(), Some("NaN"));
        assert_eq!(parse_numeric_text("Infinity").as_deref(), Some("Infinity"));
        assert_eq!(parse_numeric_text("inf").as_deref(), Some("Infinity"));
        assert_eq!(parse_numeric_text("+infinity").as_deref(), Some("Infinity"));
        assert_eq!(parse_numeric_text("-Infinity").as_deref(), Some("-Infinity"));
        assert_eq!(parse_numeric_text("-inf").as_deref(), Some("-Infinity"));
        // finite still canonicalises
        assert_eq!(parse_numeric_text("6").as_deref(), Some("6"));
        assert_eq!(parse_numeric_text(" 6.50 ").as_deref(), Some("6.50"));
        // genuine garbage is rejected
        assert_eq!(parse_numeric_text("NotANumber"), None);
        assert_eq!(parse_numeric_text("infi"), None);
    }

    #[test]
    fn nan_sorts_greatest_and_equals_itself() {
        // rank order: -Inf < finite < +Inf < NaN
        assert_eq!(cmp_sort("NaN", "Infinity"), Ordering::Greater);
        assert_eq!(cmp_sort("NaN", "5"), Ordering::Greater);
        assert_eq!(cmp_sort("NaN", "NaN"), Ordering::Equal);
        assert_eq!(cmp_sort("Infinity", "5"), Ordering::Greater);
        assert_eq!(cmp_sort("-Infinity", "-999999"), Ordering::Less);
        assert_eq!(cmp_sort("-Infinity", "-Infinity"), Ordering::Equal);
    }

    #[test]
    fn cmp_sort_orders_finite_numerically_not_lexicographically() {
        // Multi-digit values: numeric order is 2 < 9 < 10 < 25 < 100, whereas a
        // lexicographic (byte) compare would wrongly give "10" < "100" < "2".
        assert_eq!(cmp_sort("2", "10"), Ordering::Less);
        assert_eq!(cmp_sort("10", "2"), Ordering::Greater);
        assert_eq!(cmp_sort("9", "10"), Ordering::Less);
        assert_eq!(cmp_sort("25", "100"), Ordering::Less);
        assert_eq!(cmp_sort("100", "9"), Ordering::Greater);
        // Sorting the full set yields ascending numeric order.
        let mut v = vec!["2", "10", "9", "100", "25"];
        v.sort_by(|a, b| cmp_sort(a, b));
        assert_eq!(v, vec!["2", "9", "10", "25", "100"]);
        // Scale-only differences compare equal (1.0 == 1.00 numerically).
        assert_eq!(cmp_sort("1.0", "1.00"), Ordering::Equal);
        assert_eq!(cmp_sort("2.5", "2.50"), Ordering::Equal);
        // Negatives and fractions order by value.
        assert_eq!(cmp_sort("-10", "-2"), Ordering::Less);
        assert_eq!(cmp_sort("0.1", "0.09"), Ordering::Greater);
        // Finite always below +Inf/NaN and above -Inf, whatever its digits.
        assert_eq!(cmp_sort("100", "Infinity"), Ordering::Less);
        assert_eq!(cmp_sort("100", "-Infinity"), Ordering::Greater);
        assert_eq!(cmp_sort("100", "NaN"), Ordering::Less);
    }

    #[test]
    fn cmp_numeric_pushdown_orders_finite_numerically() {
        assert_eq!(cmp_numeric_pushdown("2", "10"), Some(Ordering::Less));
        assert_eq!(cmp_numeric_pushdown("100", "25"), Some(Ordering::Greater));
        assert_eq!(cmp_numeric_pushdown("1.0", "1.00"), Some(Ordering::Equal));
        assert_eq!(cmp_numeric_pushdown("-Infinity", "5"), Some(Ordering::Less));
        assert_eq!(cmp_numeric_pushdown("NaN", "Infinity"), Some(Ordering::Greater));
    }

    #[test]
    fn operator_cmp_matches_pg() {
        // NaN = NaN true, NaN <> finite
        assert_eq!(
            special_operator_cmp(&Value::Numeric("NaN".into()), &Value::Numeric("NaN".into())),
            Some(Ordering::Equal)
        );
        assert_eq!(
            special_operator_cmp(&Value::Numeric("NaN".into()), &Value::Int4(5)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            special_operator_cmp(&Value::Numeric("-Infinity".into()), &Value::Int4(5)),
            Some(Ordering::Less)
        );
        // NaN numeric vs NaN float — both rank 3 → equal (PG treats them equal)
        assert_eq!(
            special_operator_cmp(&Value::Numeric("NaN".into()), &Value::Float8(f64::NAN)),
            Some(Ordering::Equal)
        );
        // two finite numerics → not our concern
        assert_eq!(
            special_operator_cmp(&Value::Numeric("1".into()), &Value::Numeric("2".into())),
            None
        );
    }

    #[test]
    fn arithmetic_follows_pg_infinity_rules() {
        let n = |s: &str| Value::Numeric(s.into());
        let go = |op, a: Value, b: Value| special_arith(op, &a, &b).unwrap();

        assert_eq!(go(ArithOp::Add, n("NaN"), n("5")), Some(n("NaN")));
        assert_eq!(go(ArithOp::Add, n("Infinity"), n("Infinity")), Some(n("Infinity")));
        assert_eq!(go(ArithOp::Add, n("Infinity"), n("-Infinity")), Some(n("NaN")));
        assert_eq!(go(ArithOp::Sub, n("Infinity"), n("Infinity")), Some(n("NaN")));
        assert_eq!(go(ArithOp::Sub, n("Infinity"), Value::Int4(3)), Some(n("Infinity")));
        assert_eq!(go(ArithOp::Mul, n("Infinity"), Value::Int4(-2)), Some(n("-Infinity")));
        assert_eq!(go(ArithOp::Mul, n("Infinity"), Value::Int4(0)), Some(n("NaN")));
        assert_eq!(go(ArithOp::Div, n("Infinity"), n("Infinity")), Some(n("NaN")));
        assert_eq!(go(ArithOp::Div, Value::Int4(5), n("Infinity")), Some(n("0")));
        // NaN / 0 = NaN (NaN dominates the zero check)
        assert_eq!(go(ArithOp::Div, n("NaN"), Value::Int4(0)), Some(n("NaN")));
        // Inf / 0 = division by zero error
        assert!(special_arith(ArithOp::Div, &n("Infinity"), &Value::Int4(0)).is_err());
        // no special operand → passthrough
        assert_eq!(go(ArithOp::Add, n("1"), n("2")), None);
        // float operand → passthrough (handled by the evaluator's float arms)
        assert_eq!(go(ArithOp::Add, n("NaN"), Value::Float8(1.0)), None);
    }

    #[test]
    fn sum_fold_combines_specials() {
        let vals = vec![
            Value::Numeric("1".into()),
            Value::Numeric("NaN".into()),
            Value::Numeric("2".into()),
        ];
        assert_eq!(fold_sum_specials(&vals), Some(Special::NaN));

        let inf = vec![Value::Int4(1), Value::Numeric("Infinity".into())];
        assert_eq!(fold_sum_specials(&inf), Some(Special::PosInf));

        let mixed = vec![Value::Numeric("Infinity".into()), Value::Numeric("-Infinity".into())];
        assert_eq!(fold_sum_specials(&mixed), Some(Special::NaN));

        let finite = vec![Value::Int4(1), Value::Numeric("2".into())];
        assert_eq!(fold_sum_specials(&finite), None);
    }
}
