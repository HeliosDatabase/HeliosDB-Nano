//! R3.4 vectorized kernels over typed columnar batches.
//!
//! Predicate evaluation produces a per-batch **selection mask** (one 0/1 byte
//! per slot — byte-expanded rather than bit-packed so the loops below are
//! pure byte ops LLVM autovectorizes); aggregation kernels then fold the
//! selected lanes of a typed buffer into partial results the engine merges
//! into its existing aggregate states.
//!
//! # Semantics contract
//!
//! Every kernel mirrors `FilterPredicate::evaluate` / the engine's scalar
//! aggregate updates EXACTLY — including the quirks:
//!
//! - NULL lanes evaluate to `pred.evaluate(&Value::Null)` (precomputed once
//!   per predicate). Note `NotEq` ACCEPTS NULL under these semantics.
//! - Cross-family comparisons (int column vs string constant, float column
//!   vs int constant, …) are `false` for Eq/Lt/LtEq/Gt/GtEq and `true` for
//!   NotEq on every non-NULL lane, exactly like `compare_eq/lt/gt`'s
//!   fall-through arms — handled by the uniform kernel.
//! - Float equality uses the epsilon formulas from `compare_eq`, computed in
//!   f32 when both column width and constant are Float4, f64 otherwise.
//!   LtEq/GtEq are `lt || eq` with that epsilon-eq, not plain `<=`.
//! - Text predicates build a per-dictionary-entry lookup table by calling
//!   `pred.evaluate` itself (≤1024 entries per batch), so ANY operator
//!   (LIKE, IN, …) over a dictionary-coded batch is exact by construction.
//!
//! # Autovectorization notes (per kernel, asm-verified at the workspace's
//! # baseline x86-64 target — SSE2; no `target-cpu` override is set)
//!
//! The comparison kernels (`mask_int_cmp`, `mask_float_cmp`), validity/
//! presence expansion, counts and the small-magnitude i64 masked sum are
//! written as branchless select-style loops over equal-length slices, which
//! LLVM autovectorizes (pcmpgt/pand/por masks, pmuludq/paddq sums). Two
//! caveats discovered by inspecting the generated asm:
//!
//! - This workspace builds release with `overflow-checks = true`
//!   (`.cargo/config.toml`); the panic edge of any checked `+=` in a loop
//!   body blocks the vectorizer. Kernels therefore use wrapping arithmetic
//!   exactly where overflow is structurally impossible (0/1 lanes, the
//!   SUM_I64_SAFE_BOUND magnitude precondition) — semantics are identical.
//! - The f64 sums and float MIN/MAX stay scalar BY DESIGN: vectorizing
//!   reassociates FP additions / reorders NaN comparisons, which would
//!   change results vs the engine's scalar paths. LLVM (correctly) refuses
//!   without fast-math, and we do not want fast-math.
//!
//! The dictionary/grouped kernels are gather/scatter loops (data-dependent
//! indices) that no SIMD ISA expresses profitably at this batch size; their
//! win comes from replacing a per-row `Vec<Value>`-key hash-map probe with
//! an array index. The existing AVX2 intrinsics in `simd_filter.rs`
//! (i32-only, index-materializing) were measured against the autovectorized
//! i64 mask loop and lost (they emit a `Vec<usize>` of indices instead of a
//! mask and only cover i32), so no hand-written intrinsics are used here.

use super::columnar::{ColumnBatch, BATCH_SIZE};
use super::simd_filter::{FilterOp, FilterPredicate};
use super::typed_batch::{FloatWidth, TypedValues};
use crate::Value;

/// An all-selected mask (used by unfiltered aggregate paths).
pub(crate) static ALL_SELECTED: [u8; BATCH_SIZE] = [1u8; BATCH_SIZE];

/// Expand a bit-packed presence/validity bitmap into a 0/1 byte mask of
/// length `mask.len()` (bits beyond `bits` read as 0).
pub(crate) fn expand_bitmap_to_mask(bits: &[u8], mask: &mut [u8]) {
    for (i, m) in mask.iter_mut().enumerate() {
        *m = bits.get(i / 8).map_or(0, |byte| (byte >> (i % 8)) & 1);
    }
}

/// Constant representation of a predicate's comparison value, precomputed
/// once per query.
enum ConstRepr<'a> {
    Int(i64),
    Float { v: f64, is_f32: bool },
    Bool(bool),
    Str(&'a str),
    /// NULL / list / unsupported constant: cross-family uniform results for
    /// the six comparison ops, scalar fallback otherwise.
    Other,
}

/// A predicate compiled for per-batch mask evaluation.
pub(crate) struct CompiledPredicate<'a> {
    /// The source predicate (used for scalar fallback and text LUTs).
    pub pred: &'a FilterPredicate,
    null_result: u8,
    konst: ConstRepr<'a>,
}

impl<'a> CompiledPredicate<'a> {
    pub(crate) fn new(pred: &'a FilterPredicate) -> Self {
        let konst = match &pred.value {
            Value::Int2(x) => ConstRepr::Int(*x as i64),
            Value::Int4(x) => ConstRepr::Int(*x as i64),
            Value::Int8(x) => ConstRepr::Int(*x),
            Value::Float4(x) => ConstRepr::Float {
                v: *x as f64,
                is_f32: true,
            },
            Value::Float8(x) => ConstRepr::Float { v: *x, is_f32: false },
            Value::Boolean(b) => ConstRepr::Bool(*b),
            Value::String(s) => ConstRepr::Str(s),
            _ => ConstRepr::Other,
        };
        Self {
            pred,
            null_result: pred.evaluate(&Value::Null) as u8,
            konst,
        }
    }

    /// AND this predicate's result into `mask` for one batch. `batch` is the
    /// predicate column's batch for the current batch id (`None` ⇒ every
    /// lane reads NULL).
    pub(crate) fn apply_to_mask(&self, batch: Option<&ColumnBatch>, mask: &mut [u8]) {
        let Some(batch) = batch else {
            if self.null_result == 0 {
                mask.fill(0);
            }
            return;
        };

        let cmp6 = matches!(
            self.pred.op,
            FilterOp::Eq | FilterOp::NotEq | FilterOp::Lt | FilterOp::LtEq | FilterOp::Gt | FilterOp::GtEq
        );

        if let Some(typed) = &batch.typed {
            match self.pred.op {
                FilterOp::IsNull => {
                    mask_validity(&typed.validity, true, mask);
                    return;
                }
                FilterOp::IsNotNull => {
                    mask_validity(&typed.validity, false, mask);
                    return;
                }
                _ => {}
            }

            match (&typed.data, &self.konst) {
                // Any operator over a dictionary-coded batch goes through the
                // exact per-dict-entry LUT (evaluate() itself).
                (TypedValues::Text { dict, codes }, _) => {
                    let lut: Vec<u8> = dict
                        .iter()
                        .map(|s| self.pred.evaluate(&Value::String(s.clone())) as u8)
                        .collect();
                    mask_text_lut(codes, &typed.validity, &lut, self.null_result, mask);
                    return;
                }
                (TypedValues::Int { data, .. }, ConstRepr::Int(c)) if cmp6 => {
                    mask_int_cmp(data, &typed.validity, self.pred.op, *c, self.null_result, mask);
                    return;
                }
                (TypedValues::Float { width, data }, ConstRepr::Float { v, is_f32 }) if cmp6 => {
                    let f32_math = *is_f32 && *width == FloatWidth::F4;
                    mask_float_cmp(data, &typed.validity, self.pred.op, *v, f32_math, self.null_result, mask);
                    return;
                }
                (TypedValues::Bool { data }, ConstRepr::Bool(c)) if cmp6 => {
                    mask_bool_cmp(data, &typed.validity, self.pred.op, *c, self.null_result, mask);
                    return;
                }
                // Cross-family single-constant comparison: uniform result on
                // every non-NULL lane (mirrors compare_eq/lt/gt fall-through).
                _ if cmp6 => {
                    let nonnull_result = matches!(self.pred.op, FilterOp::NotEq) as u8;
                    mask_uniform(&typed.validity, nonnull_result, self.null_result, mask);
                    return;
                }
                _ => {}
            }
        }

        mask_scalar(self.pred, batch.values(), mask);
    }
}

/// Compile a predicate conjunction once per query.
pub(crate) fn compile_predicates(predicates: &[FilterPredicate]) -> Vec<CompiledPredicate<'_>> {
    predicates.iter().map(CompiledPredicate::new).collect()
}

/// Scalar fallback: evaluate the predicate per still-selected lane against
/// the materialized values (absent slots read as NULL) — bit-exact with the
/// pre-R3.4 row-at-a-time path. Used for v1 batches and operator/constant
/// combinations without a typed kernel.
pub(crate) fn mask_scalar(pred: &FilterPredicate, values: &[Value], mask: &mut [u8]) {
    for (i, m) in mask.iter_mut().enumerate() {
        if *m != 0 {
            *m = pred.evaluate(values.get(i).unwrap_or(&Value::Null)) as u8;
        }
    }
}

/// IS NULL / IS NOT NULL over the validity buffer (lanes beyond the buffer
/// read as NULL). Autovectorizes (pure byte ops).
fn mask_validity(validity: &[u8], is_null: bool, mask: &mut [u8]) {
    let n = validity.len().min(mask.len());
    // SAFETY/bounds: the zip iterators bound every access to n.
    if is_null {
        for (m, &ok) in mask.iter_mut().zip(validity) {
            *m &= ok ^ 1;
        }
        // lanes beyond slot_count are NULL ⇒ IS NULL keeps them (mask unchanged)
    } else {
        for (m, &ok) in mask.iter_mut().zip(validity) {
            *m &= ok;
        }
        for m in mask.iter_mut().skip(n) {
            *m = 0;
        }
    }
}

/// Uniform-result kernel for cross-family comparisons: non-NULL lanes get
/// `nonnull_result`, NULL lanes (and lanes beyond the buffer) `null_result`.
fn mask_uniform(validity: &[u8], nonnull_result: u8, null_result: u8, mask: &mut [u8]) {
    let n = validity.len().min(mask.len());
    for (m, &ok) in mask.iter_mut().zip(validity) {
        *m &= (ok & nonnull_result) | ((ok ^ 1) & null_result);
    }
    if null_result == 0 {
        for m in mask.iter_mut().skip(n) {
            *m = 0;
        }
    }
}

/// Branchless select applied per lane: `mask &= valid ? cmp(v) : null_result`.
#[inline(always)]
fn mask_lanes<T: Copy>(data: &[T], validity: &[u8], null_result: u8, mask: &mut [u8], cmp: impl Fn(T) -> bool) {
    let n = data.len().min(validity.len()).min(mask.len());
    for ((m, &v), &ok) in mask.iter_mut().zip(data).zip(validity) {
        let keep = (ok & cmp(v) as u8) | ((ok ^ 1) & null_result);
        *m &= keep;
    }
    if null_result == 0 {
        for m in mask.iter_mut().skip(n) {
            *m = 0;
        }
    }
}

/// i64 comparison kernel (covers Int2/4/8 batches via width promotion; the
/// constant is widened the same way `compare_eq/lt/gt`'s cross-width arms
/// do, so results are identical). Branchless; autovectorizes.
fn mask_int_cmp(data: &[i64], validity: &[u8], op: FilterOp, c: i64, null_result: u8, mask: &mut [u8]) {
    match op {
        FilterOp::Eq => mask_lanes(data, validity, null_result, mask, |v| v == c),
        FilterOp::NotEq => mask_lanes(data, validity, null_result, mask, |v| v != c),
        FilterOp::Lt => mask_lanes(data, validity, null_result, mask, |v| v < c),
        FilterOp::LtEq => mask_lanes(data, validity, null_result, mask, |v| v <= c),
        FilterOp::Gt => mask_lanes(data, validity, null_result, mask, |v| v > c),
        FilterOp::GtEq => mask_lanes(data, validity, null_result, mask, |v| v >= c),
        _ => mask_lanes(data, validity, null_result, mask, |_| false),
    }
}

/// f64 comparison kernel. `f32_math` selects the f32 epsilon-equality of the
/// (Float4 column, Float4 constant) scalar arm; all promoted values are
/// exactly representable so the downcasts are lossless. LtEq/GtEq are
/// `lt || eps_eq`, mirroring `evaluate`.
fn mask_float_cmp(data: &[f64], validity: &[u8], op: FilterOp, c: f64, f32_math: bool, null_result: u8, mask: &mut [u8]) {
    let eq = move |v: f64| -> bool {
        if f32_math {
            (v as f32 - c as f32).abs() < f32::EPSILON
        } else {
            (v - c).abs() < f64::EPSILON
        }
    };
    match op {
        FilterOp::Eq => mask_lanes(data, validity, null_result, mask, eq),
        FilterOp::NotEq => mask_lanes(data, validity, null_result, mask, move |v| !eq(v)),
        FilterOp::Lt => mask_lanes(data, validity, null_result, mask, move |v| v < c),
        FilterOp::LtEq => mask_lanes(data, validity, null_result, mask, move |v| v < c || eq(v)),
        FilterOp::Gt => mask_lanes(data, validity, null_result, mask, move |v| v > c),
        FilterOp::GtEq => mask_lanes(data, validity, null_result, mask, move |v| v > c || eq(v)),
        _ => mask_lanes(data, validity, null_result, mask, |_| false),
    }
}

/// Boolean comparison kernel (`Lt` is `!a && b` etc., per `compare_lt/gt`).
fn mask_bool_cmp(data: &[u8], validity: &[u8], op: FilterOp, c: bool, null_result: u8, mask: &mut [u8]) {
    let c = c as u8;
    match op {
        FilterOp::Eq => mask_lanes(data, validity, null_result, mask, move |v| v == c),
        FilterOp::NotEq => mask_lanes(data, validity, null_result, mask, move |v| v != c),
        FilterOp::Lt => mask_lanes(data, validity, null_result, mask, move |v| v < c),
        FilterOp::LtEq => mask_lanes(data, validity, null_result, mask, move |v| v <= c),
        FilterOp::Gt => mask_lanes(data, validity, null_result, mask, move |v| v > c),
        FilterOp::GtEq => mask_lanes(data, validity, null_result, mask, move |v| v >= c),
        _ => mask_lanes(data, validity, null_result, mask, |_| false),
    }
}

/// Dictionary LUT kernel: one table lookup per lane. Out-of-range codes
/// (corrupt batch) read as NULL, matching the decoder.
fn mask_text_lut(codes: &[u32], validity: &[u8], lut: &[u8], null_result: u8, mask: &mut [u8]) {
    let n = codes.len().min(validity.len()).min(mask.len());
    for ((m, &code), &ok) in mask.iter_mut().zip(codes).zip(validity) {
        let hit = lut.get(code as usize).copied().unwrap_or(null_result);
        *m &= (ok & hit) | ((ok ^ 1) & null_result);
    }
    if null_result == 0 {
        for m in mask.iter_mut().skip(n) {
            *m = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation kernels over selected lanes
// ---------------------------------------------------------------------------

/// COUNT(*) over the mask (autovectorized byte sum).
///
/// Wrapping arithmetic throughout these kernels is NOT for speed of the adds
/// themselves: this workspace enables `overflow-checks = true` in release
/// (`.cargo/config.toml`), and the panic branch of every checked add blocks
/// LLVM's loop vectorizer (verified: the checked forms compile to scalar
/// loops with panic edges; the wrapping forms vectorize). Overflow is
/// structurally impossible — mask/validity lanes are 0/1, so these counters
/// grow by at most 1 per lane.
pub(crate) fn count_selected(mask: &[u8]) -> u64 {
    // u32 chunk partials (≤256 lanes of 0/1 each) widen once per chunk —
    // this shape vectorizes where a straight u64 fold does not.
    let mut n: u64 = 0;
    for chunk in mask.chunks(256) {
        let mut part: u32 = 0;
        for &m in chunk {
            part = part.wrapping_add(m as u32);
        }
        n = n.wrapping_add(part as u64);
    }
    n
}

/// COUNT(col): selected AND non-NULL lanes.
pub(crate) fn count_selected_valid(validity: &[u8], mask: &[u8]) -> u64 {
    validity
        .iter()
        .zip(mask)
        .fold(0u64, |acc, (&ok, &m)| acc.wrapping_add((ok & m) as u64))
}

/// Magnitude bound under which a whole batch can be summed in plain i64
/// without overflow (BATCH_SIZE terms).
pub(crate) const SUM_I64_SAFE_BOUND: i64 = i64::MAX / (BATCH_SIZE as i64 + 1);

/// SUM over selected non-NULL int lanes. Returns `(sum, contributing)`.
///
/// `small_magnitude` (derived from the batch's embedded min/max) selects a
/// plain-i64 multiply-select loop that autovectorizes; otherwise the sum
/// accumulates in i128 (scalar, but still one tight pass). Like the R3.2
/// parallel partials, batch-level accumulation reorders additions relative
/// to the pure serial fold, which can shift WHERE an i64 overflow error
/// fires in adversarial value orders — the accepted, pre-existing trade.
pub(crate) fn sum_int_selected(data: &[i64], validity: &[u8], mask: &[u8], small_magnitude: bool) -> (i128, u64) {
    let mut n: u64 = 0;
    if small_magnitude {
        // Wrapping ops so the release overflow checks don't block
        // vectorization (see `count_selected`): `sel` is 0/1, and the
        // caller-checked SUM_I64_SAFE_BOUND magnitude bound makes i64
        // overflow impossible over ≤ BATCH_SIZE terms, so wrapping ==
        // checked here. Verified codegen: pmuludq/paddq lanes.
        let mut sum: i64 = 0;
        for ((&v, &ok), &m) in data.iter().zip(validity).zip(mask) {
            let sel = (ok & m) as i64;
            sum = sum.wrapping_add(sel.wrapping_mul(v));
            n = n.wrapping_add(sel as u64);
        }
        (sum as i128, n)
    } else {
        // i128 accumulation is inherently scalar; wrapping on the 0/1
        // counter still removes its panic edge from the loop.
        let mut sum: i128 = 0;
        for ((&v, &ok), &m) in data.iter().zip(validity).zip(mask) {
            let sel = (ok & m) as i128;
            sum += sel * v as i128;
            n = n.wrapping_add((ok & m) as u64);
        }
        (sum, n)
    }
}

/// MIN/MAX over selected non-NULL int lanes (sentinel select; autovectorizes).
pub(crate) fn min_max_int_selected(data: &[i64], validity: &[u8], mask: &[u8]) -> Option<(i64, i64)> {
    let mut mn = i64::MAX;
    let mut mx = i64::MIN;
    let mut any: u8 = 0;
    for ((&v, &ok), &m) in data.iter().zip(validity).zip(mask) {
        let sel = ok & m;
        any |= sel;
        let vmin = if sel != 0 { v } else { i64::MAX };
        let vmax = if sel != 0 { v } else { i64::MIN };
        mn = mn.min(vmin);
        mx = mx.max(vmax);
    }
    (any != 0).then_some((mn, mx))
}

/// AVG partial over selected non-NULL float lanes: `(f64 sum, count)`.
/// Mirrors the scalar `*sum += v` accumulation per lane in slot order;
/// batch-level partials merge like R3.2 chunk partials.
pub(crate) fn sum_f64_selected(data: &[f64], validity: &[u8], mask: &[u8]) -> (f64, u64) {
    // The f64 accumulation stays scalar BY DESIGN: vectorizing would
    // reassociate the additions and change results bit-for-bit vs the
    // scalar path (LLVM refuses without fast-math, correctly). The wrapping
    // counter just removes the overflow-check panic edge from the loop.
    let mut sum = 0.0f64;
    let mut n: u64 = 0;
    for ((&v, &ok), &m) in data.iter().zip(validity).zip(mask) {
        let sel = ok & m;
        sum += if sel != 0 { v } else { 0.0 };
        n = n.wrapping_add(sel as u64);
    }
    (sum, n)
}

/// AVG partial over selected non-NULL int lanes (each int converted to f64,
/// matching the scalar AVG update). Scalar f64 accumulation by design — see
/// `sum_f64_selected`.
pub(crate) fn sum_f64_from_int_selected(data: &[i64], validity: &[u8], mask: &[u8]) -> (f64, u64) {
    let mut sum = 0.0f64;
    let mut n: u64 = 0;
    for ((&v, &ok), &m) in data.iter().zip(validity).zip(mask) {
        let sel = ok & m;
        sum += if sel != 0 { v as f64 } else { 0.0 };
        n = n.wrapping_add(sel as u64);
    }
    (sum, n)
}

/// MIN/MAX over selected non-NULL float lanes, mirroring
/// `compare_values`'s `partial_cmp(..).unwrap_or(Equal)` first-wins
/// semantics in slot order (NaN never displaces an incumbent, and an
/// incumbent NaN is never displaced — deliberately a serial dependent loop).
pub(crate) fn min_max_float_selected(data: &[f64], validity: &[u8], mask: &[u8]) -> (Option<f64>, Option<f64>) {
    let mut mn: Option<f64> = None;
    let mut mx: Option<f64> = None;
    for ((&v, &ok), &m) in data.iter().zip(validity).zip(mask) {
        if ok & m == 0 {
            continue;
        }
        mn = Some(match mn {
            None => v,
            Some(c) => {
                if v.partial_cmp(&c).unwrap_or(std::cmp::Ordering::Equal) == std::cmp::Ordering::Less {
                    v
                } else {
                    c
                }
            }
        });
        mx = Some(match mx {
            None => v,
            Some(c) => {
                if v.partial_cmp(&c).unwrap_or(std::cmp::Ordering::Equal) == std::cmp::Ordering::Greater {
                    v
                } else {
                    c
                }
            }
        });
    }
    (mn, mx)
}

/// Mark dictionary codes used by selected non-NULL lanes (for text MIN/MAX:
/// the caller then min/maxes over ≤ dict.len() strings instead of N lanes).
pub(crate) fn mark_used_codes(codes: &[u32], validity: &[u8], mask: &[u8], used: &mut [u8]) {
    for ((&code, &ok), &m) in codes.iter().zip(validity).zip(mask) {
        if ok & m != 0 {
            if let Some(slot) = used.get_mut(code as usize) {
                *slot = 1;
            }
        }
    }
}

/// Per-batch grouped COUNT(*) + SUM(int) keyed by dictionary code (the text
/// GROUP BY kernel). `counts`/`sums`/`seen` must have `dict_len + 1` slots;
/// the last slot accumulates the NULL group (NULL group-key lanes, including
/// lanes beyond the group batch's slot count). `sum` carries the SUM
/// column's typed buffer for the same batch id, or `None` when every
/// selected lane's sum operand reads NULL (absent batch).
///
/// Gather/scatter loop — not SIMD-vectorizable, but replaces a per-row
/// `Value`-keyed hash probe with an array index.
pub(crate) fn group_count_sum_by_code(
    codes: &[u32],
    gvalid: &[u8],
    mask: &[u8],
    sum: Option<(&[i64], &[u8])>,
    counts: &mut [i64],
    sums: &mut [i128],
    seen: &mut [u8],
) {
    debug_assert!(counts.len() == sums.len() && sums.len() == seen.len());
    let null_slot = counts.len().saturating_sub(1);
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        let idx = match (codes.get(i), gvalid.get(i)) {
            (Some(&code), Some(&ok)) if ok != 0 => (code as usize).min(null_slot),
            _ => null_slot,
        };
        if let Some(c) = counts.get_mut(idx) {
            *c += 1;
        }
        if let Some((data, valid)) = sum {
            if let (Some(&v), Some(&ok)) = (data.get(i), valid.get(i)) {
                if ok != 0 {
                    if let (Some(s), Some(f)) = (sums.get_mut(idx), seen.get_mut(idx)) {
                        *s += v as i128;
                        *f = 1;
                    }
                }
            }
        }
    }
}

/// Per-batch grouped COUNT(*) + SUM(int) keyed by small-range integers:
/// `idx = group_value - base` (caller guarantees `counts.len() >= range + 2`
/// from the batch's embedded min/max; last slot is the NULL group).
pub(crate) fn group_count_sum_by_small_int(
    gdata: &[i64],
    gvalid: &[u8],
    base: i64,
    mask: &[u8],
    sum: Option<(&[i64], &[u8])>,
    counts: &mut [i64],
    sums: &mut [i128],
    seen: &mut [u8],
) {
    debug_assert!(counts.len() == sums.len() && sums.len() == seen.len());
    let null_slot = counts.len().saturating_sub(1);
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        let idx = match (gdata.get(i), gvalid.get(i)) {
            (Some(&v), Some(&ok)) if ok != 0 => usize::try_from(v.wrapping_sub(base)).map_or(null_slot, |d| d.min(null_slot)),
            _ => null_slot,
        };
        if let Some(c) = counts.get_mut(idx) {
            *c += 1;
        }
        if let Some((data, valid)) = sum {
            if let (Some(&v), Some(&ok)) = (data.get(i), valid.get(i)) {
                if ok != 0 {
                    if let (Some(s), Some(f)) = (sums.get_mut(idx), seen.get_mut(idx)) {
                        *s += v as i128;
                        *f = 1;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::typed_batch::{decode_column_batch, encode_column_batch};
    use crate::storage::columnar::BatchStats;

    /// Build a decoded-from-v2 batch (typed populated) from values.
    fn typed_batch(values: Vec<Value>) -> ColumnBatch {
        let batch = ColumnBatch::from_values("v".to_string(), 0, values);
        let stats = BatchStats::from_batch(&batch);
        let encoded = encode_column_batch(&batch, &stats).unwrap();
        let decoded = decode_column_batch(&encoded).unwrap();
        assert!(decoded.typed.is_some(), "test batch must be v2-eligible");
        decoded
    }

    fn scalar_reference(pred: &FilterPredicate, values: &[Value], lanes: usize) -> Vec<u8> {
        (0..lanes)
            .map(|i| pred.evaluate(values.get(i).unwrap_or(&Value::Null)) as u8)
            .collect()
    }

    fn assert_kernel_matches_scalar(pred: &FilterPredicate, values: Vec<Value>, lanes: usize) {
        let batch = typed_batch(values.clone());
        let compiled = CompiledPredicate::new(pred);
        let mut mask = vec![1u8; lanes];
        compiled.apply_to_mask(Some(&batch), &mut mask);
        assert_eq!(
            mask,
            scalar_reference(pred, &values, lanes),
            "kernel diverged from evaluate() for {:?} {:?}",
            pred.op,
            pred.value
        );
    }

    #[test]
    fn int_kernels_match_evaluate_for_all_ops_and_widths() {
        let values: Vec<Value> = vec![
            Value::Int4(5),
            Value::Null,
            Value::Int4(-7),
            Value::Int4(25000),
            Value::Int4(0),
            Value::Int4(i32::MAX),
        ];
        for op in [
            FilterOp::Eq,
            FilterOp::NotEq,
            FilterOp::Lt,
            FilterOp::LtEq,
            FilterOp::Gt,
            FilterOp::GtEq,
        ] {
            for c in [Value::Int8(5), Value::Int4(5), Value::Int2(5), Value::Int8(-7)] {
                let pred = FilterPredicate::compare(0, "v".into(), op, c);
                assert_kernel_matches_scalar(&pred, values.clone(), 8);
            }
            // Cross-family constants → uniform kernel.
            for c in [
                Value::Float8(5.0),
                Value::String("5".into()),
                Value::Boolean(true),
                Value::Null,
            ] {
                let pred = FilterPredicate::compare(0, "v".into(), op, c);
                assert_kernel_matches_scalar(&pred, values.clone(), 8);
            }
        }
    }

    #[test]
    fn float_kernels_match_evaluate_including_epsilon_and_nan() {
        let f8: Vec<Value> = vec![
            Value::Float8(1.5),
            Value::Null,
            Value::Float8(1.5 + f64::EPSILON / 2.0),
            Value::Float8(f64::NAN),
            Value::Float8(-3.25),
        ];
        let f4: Vec<Value> = vec![
            Value::Float4(1.5),
            Value::Null,
            Value::Float4(f32::NAN),
            Value::Float4(-3.25),
        ];
        for op in [
            FilterOp::Eq,
            FilterOp::NotEq,
            FilterOp::Lt,
            FilterOp::LtEq,
            FilterOp::Gt,
            FilterOp::GtEq,
        ] {
            for c in [Value::Float8(1.5), Value::Float4(1.5), Value::Int8(1)] {
                let pred = FilterPredicate::compare(0, "v".into(), op, c.clone());
                assert_kernel_matches_scalar(&pred, f8.clone(), 6);
                assert_kernel_matches_scalar(&pred, f4.clone(), 6);
            }
        }
    }

    #[test]
    fn bool_and_null_kernels_match_evaluate() {
        let values = vec![Value::Boolean(true), Value::Null, Value::Boolean(false)];
        for op in [
            FilterOp::Eq,
            FilterOp::NotEq,
            FilterOp::Lt,
            FilterOp::LtEq,
            FilterOp::Gt,
            FilterOp::GtEq,
        ] {
            for c in [Value::Boolean(true), Value::Boolean(false), Value::Int4(1)] {
                let pred = FilterPredicate::compare(0, "v".into(), op, c.clone());
                assert_kernel_matches_scalar(&pred, values.clone(), 5);
            }
        }
        let is_null = FilterPredicate::is_null(0, "v".into());
        assert_kernel_matches_scalar(&is_null, values.clone(), 5);
        let mut is_not_null = FilterPredicate::is_null(0, "v".into());
        is_not_null.op = FilterOp::IsNotNull;
        assert_kernel_matches_scalar(&is_not_null, values, 5);
    }

    #[test]
    fn text_lut_matches_evaluate_for_every_operator() {
        let values = vec![
            Value::String("apple".into()),
            Value::Null,
            Value::String("banana".into()),
            Value::String("apple".into()),
            Value::String("cherry".into()),
        ];
        let mut preds = vec![
            FilterPredicate::eq(0, "v".into(), Value::String("banana".into())),
            FilterPredicate::compare(0, "v".into(), FilterOp::Lt, Value::String("b".into())),
            FilterPredicate::compare(0, "v".into(), FilterOp::GtEq, Value::String("banana".into())),
            FilterPredicate::like(0, "v".into(), "%an%".into()),
            FilterPredicate::in_list(
                0,
                "v".into(),
                vec![Value::String("apple".into()), Value::String("cherry".into())],
            ),
            FilterPredicate::eq(0, "v".into(), Value::Int8(3)), // cross-family
        ];
        let mut notlike = FilterPredicate::like(0, "v".into(), "%an%".into());
        notlike.op = FilterOp::NotLike;
        preds.push(notlike);
        for pred in &preds {
            assert_kernel_matches_scalar(pred, values.clone(), 7);
        }
    }

    #[test]
    fn between_falls_back_to_scalar_and_matches() {
        let values = vec![Value::Int8(10), Value::Null, Value::Int8(20), Value::Int8(30)];
        let pred = FilterPredicate::between(0, "v".into(), Value::Int8(15), Value::Int8(25));
        assert_kernel_matches_scalar(&pred, values, 5);
    }

    #[test]
    fn absent_batch_lanes_read_null() {
        let pred_ne = FilterPredicate::compare(0, "v".into(), FilterOp::NotEq, Value::Int8(5));
        let compiled = CompiledPredicate::new(&pred_ne);
        let mut mask = vec![1u8; 4];
        compiled.apply_to_mask(None, &mut mask);
        // NotEq accepts NULL under simd-filter semantics.
        assert_eq!(mask, vec![1, 1, 1, 1]);

        let pred_eq = FilterPredicate::eq(0, "v".into(), Value::Int8(5));
        let compiled = CompiledPredicate::new(&pred_eq);
        let mut mask = vec![1u8; 4];
        compiled.apply_to_mask(None, &mut mask);
        assert_eq!(mask, vec![0, 0, 0, 0]);
    }

    #[test]
    fn aggregate_kernels_basics() {
        let data = [3i64, 0, -2, 7];
        let validity = [1u8, 0, 1, 1];
        let mask = [1u8, 1, 1, 0];
        assert_eq!(count_selected(&mask), 3);
        assert_eq!(count_selected_valid(&validity, &mask), 2);
        assert_eq!(sum_int_selected(&data, &validity, &mask, true), (1, 2));
        assert_eq!(sum_int_selected(&data, &validity, &mask, false), (1, 2));
        assert_eq!(min_max_int_selected(&data, &validity, &mask), Some((-2, 3)));
        assert_eq!(min_max_int_selected(&data, &validity, &[0u8; 4]), None);
        let (s, n) = sum_f64_from_int_selected(&data, &validity, &mask);
        assert_eq!((s, n), (1.0, 2));
    }

    #[test]
    fn grouped_kernel_handles_nulls_and_short_buffers() {
        // codes: a a b, NULL group from invalid lane and lane beyond buffers
        let codes = [0u32, 0, 1];
        let gvalid = [1u8, 1, 0];
        let mask = [1u8, 1, 1, 1]; // lane 3 beyond group buffers → NULL group
        let sum_data = [10i64, 20, 30];
        let sum_valid = [1u8, 0, 1];
        let mut counts = vec![0i64; 3]; // dict_len 2 + null slot
        let mut sums = vec![0i128; 3];
        let mut seen = vec![0u8; 3];
        group_count_sum_by_code(
            &codes,
            &gvalid,
            &mask,
            Some((&sum_data, &sum_valid)),
            &mut counts,
            &mut sums,
            &mut seen,
        );
        assert_eq!(counts, vec![2, 0, 2]); // a:2, b:0(invalid→null), null:2
        assert_eq!(sums, vec![10, 0, 30]);
        assert_eq!(seen, vec![1, 0, 1]);
    }
}
