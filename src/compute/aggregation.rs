//! Aggregation functions
//!
//! Standard aggregations (no online aggregation - that's proprietary IP).

use crate::{Result, Value, Error};

/// Aggregate function
pub trait AggregateFunction: Send {
    /// Initialize accumulator state
    fn init_state(&self) -> Box<dyn AggregateState>;

    /// Get function name
    fn name(&self) -> &str;
}

/// Aggregate state
pub trait AggregateState: Send {
    /// Accumulate a value
    fn accumulate(&mut self, value: &Value) -> Result<()>;

    /// Finalize and return result
    fn finalize(&self) -> Result<Value>;
}

// =============================================================================
// COUNT aggregate
// =============================================================================

/// COUNT aggregate function
pub struct CountFunction;

impl AggregateFunction for CountFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(CountState { count: 0 })
    }

    fn name(&self) -> &str {
        "COUNT"
    }
}

/// COUNT aggregate state
pub struct CountState {
    count: i64,
}

impl Default for CountState {
    fn default() -> Self {
        Self { count: 0 }
    }
}

impl AggregateState for CountState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        // COUNT(*) counts all rows, COUNT(col) counts non-null values
        if !matches!(value, Value::Null) {
            self.count += 1;
        }
        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        Ok(Value::Int8(self.count))
    }
}

// =============================================================================
// SUM aggregate
// =============================================================================

/// SUM aggregate function
pub struct SumFunction;

impl AggregateFunction for SumFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(SumState { sum: None })
    }

    fn name(&self) -> &str {
        "SUM"
    }
}

/// SUM aggregate state
pub struct SumState {
    sum: Option<f64>,
}

impl AggregateState for SumState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        let num = match value {
            Value::Null => return Ok(()),
            Value::Int2(n) => *n as f64,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Float4(n) => *n as f64,
            Value::Float8(n) => *n,
            _ => return Err(Error::Generic(format!("SUM cannot aggregate non-numeric value: {:?}", value))),
        };

        self.sum = Some(self.sum.unwrap_or(0.0) + num);
        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        Ok(self.sum.map(Value::Float8).unwrap_or(Value::Null))
    }
}

// =============================================================================
// AVG aggregate
// =============================================================================

/// AVG aggregate function
pub struct AvgFunction;

impl AggregateFunction for AvgFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(AvgState { sum: 0.0, count: 0 })
    }

    fn name(&self) -> &str {
        "AVG"
    }
}

/// AVG aggregate state
pub struct AvgState {
    sum: f64,
    count: i64,
}

impl AggregateState for AvgState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        let num = match value {
            Value::Null => return Ok(()),
            Value::Int2(n) => *n as f64,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Float4(n) => *n as f64,
            Value::Float8(n) => *n,
            _ => return Err(Error::Generic(format!("AVG cannot aggregate non-numeric value: {:?}", value))),
        };

        self.sum += num;
        self.count += 1;
        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        if self.count == 0 {
            Ok(Value::Null)
        } else {
            Ok(Value::Float8(self.sum / self.count as f64))
        }
    }
}

// =============================================================================
// MIN aggregate
// =============================================================================

/// MIN aggregate function
pub struct MinFunction;

impl AggregateFunction for MinFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(MinState { min: None })
    }

    fn name(&self) -> &str {
        "MIN"
    }
}

/// MIN aggregate state
pub struct MinState {
    min: Option<Value>,
}

impl AggregateState for MinState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        if matches!(value, Value::Null) {
            return Ok(());
        }

        match &self.min {
            None => {
                self.min = Some(value.clone());
            }
            Some(current_min) => {
                if value_less_than(value, current_min) {
                    self.min = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        Ok(self.min.clone().unwrap_or(Value::Null))
    }
}

// =============================================================================
// MAX aggregate
// =============================================================================

/// MAX aggregate function
pub struct MaxFunction;

impl AggregateFunction for MaxFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(MaxState { max: None })
    }

    fn name(&self) -> &str {
        "MAX"
    }
}

/// MAX aggregate state
pub struct MaxState {
    max: Option<Value>,
}

impl AggregateState for MaxState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        if matches!(value, Value::Null) {
            return Ok(());
        }

        match &self.max {
            None => {
                self.max = Some(value.clone());
            }
            Some(current_max) => {
                if value_greater_than(value, current_max) {
                    self.max = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        Ok(self.max.clone().unwrap_or(Value::Null))
    }
}

// =============================================================================
// STDDEV (Standard Deviation) aggregate
// =============================================================================

/// STDDEV aggregate function (sample standard deviation)
pub struct StddevFunction;

impl AggregateFunction for StddevFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(StddevState {
            count: 0,
            mean: 0.0,
            m2: 0.0  // Sum of squares of differences from mean (Welford's algorithm)
        })
    }

    fn name(&self) -> &str {
        "STDDEV"
    }
}

/// STDDEV aggregate state using Welford's online algorithm
pub struct StddevState {
    count: i64,
    mean: f64,
    m2: f64,
}

impl AggregateState for StddevState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        let num = match value {
            Value::Null => return Ok(()),
            Value::Int2(n) => *n as f64,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Float4(n) => *n as f64,
            Value::Float8(n) => *n,
            _ => return Err(Error::Generic(format!("STDDEV cannot aggregate non-numeric value: {:?}", value))),
        };

        // Welford's online algorithm for computing variance
        self.count += 1;
        let delta = num - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = num - self.mean;
        self.m2 += delta * delta2;

        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        if self.count < 2 {
            Ok(Value::Null) // Need at least 2 values for sample stddev
        } else {
            // Sample standard deviation (N-1 denominator)
            let variance = self.m2 / (self.count - 1) as f64;
            Ok(Value::Float8(variance.sqrt()))
        }
    }
}

// =============================================================================
// VARIANCE aggregate
// =============================================================================

/// VARIANCE aggregate function (sample variance)
pub struct VarianceFunction;

impl AggregateFunction for VarianceFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(VarianceState {
            count: 0,
            mean: 0.0,
            m2: 0.0
        })
    }

    fn name(&self) -> &str {
        "VARIANCE"
    }
}

/// VARIANCE aggregate state using Welford's online algorithm
pub struct VarianceState {
    count: i64,
    mean: f64,
    m2: f64,
}

impl AggregateState for VarianceState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        let num = match value {
            Value::Null => return Ok(()),
            Value::Int2(n) => *n as f64,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Float4(n) => *n as f64,
            Value::Float8(n) => *n,
            _ => return Err(Error::Generic(format!("VARIANCE cannot aggregate non-numeric value: {:?}", value))),
        };

        // Welford's online algorithm for computing variance
        self.count += 1;
        let delta = num - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = num - self.mean;
        self.m2 += delta * delta2;

        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        if self.count < 2 {
            Ok(Value::Null) // Need at least 2 values for sample variance
        } else {
            // Sample variance (N-1 denominator)
            let variance = self.m2 / (self.count - 1) as f64;
            Ok(Value::Float8(variance))
        }
    }
}

// =============================================================================
// Helper functions for value comparison
// =============================================================================

/// Compare two values for less-than ordering
fn value_less_than(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int2(x), Value::Int2(y)) => x < y,
        (Value::Int4(x), Value::Int4(y)) => x < y,
        (Value::Int8(x), Value::Int8(y)) => x < y,
        (Value::Float4(x), Value::Float4(y)) => x < y,
        (Value::Float8(x), Value::Float8(y)) => x < y,
        (Value::String(x), Value::String(y)) => x < y,
        (Value::Boolean(x), Value::Boolean(y)) => !x && *y, // false < true
        // Cross-type numeric comparisons (convert to f64)
        (a, b) if is_numeric(a) && is_numeric(b) => {
            to_f64(a).unwrap_or(f64::NAN) < to_f64(b).unwrap_or(f64::NAN)
        }
        _ => false, // Can't compare incompatible types
    }
}

/// Compare two values for greater-than ordering
fn value_greater_than(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int2(x), Value::Int2(y)) => x > y,
        (Value::Int4(x), Value::Int4(y)) => x > y,
        (Value::Int8(x), Value::Int8(y)) => x > y,
        (Value::Float4(x), Value::Float4(y)) => x > y,
        (Value::Float8(x), Value::Float8(y)) => x > y,
        (Value::String(x), Value::String(y)) => x > y,
        (Value::Boolean(x), Value::Boolean(y)) => *x && !y, // true > false
        // Cross-type numeric comparisons (convert to f64)
        (a, b) if is_numeric(a) && is_numeric(b) => {
            to_f64(a).unwrap_or(f64::NAN) > to_f64(b).unwrap_or(f64::NAN)
        }
        _ => false, // Can't compare incompatible types
    }
}

/// Check if a value is numeric
fn is_numeric(v: &Value) -> bool {
    matches!(v,
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) |
        Value::Float4(_) | Value::Float8(_)
    )
}

/// Convert a numeric value to f64
fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int2(n) => Some(*n as f64),
        Value::Int4(n) => Some(*n as f64),
        Value::Int8(n) => Some(*n as f64),
        Value::Float4(n) => Some(*n as f64),
        Value::Float8(n) => Some(*n),
        _ => None,
    }
}

// =============================================================================
// JSON_AGG aggregate
// =============================================================================

/// JSON_AGG aggregate function - aggregates values into a JSON array
pub struct JsonAggFunction;

impl AggregateFunction for JsonAggFunction {
    fn init_state(&self) -> Box<dyn AggregateState> {
        Box::new(JsonAggState {
            values: Vec::new()
        })
    }

    fn name(&self) -> &str {
        "JSON_AGG"
    }
}

/// JSON_AGG aggregate state - collects values into a JSON array
pub struct JsonAggState {
    values: Vec<serde_json::Value>,
}

impl AggregateState for JsonAggState {
    fn accumulate(&mut self, value: &Value) -> Result<()> {
        // Convert Value to JSON representation
        let json_val = match value {
            Value::Null => serde_json::Value::Null,
            Value::Boolean(b) => serde_json::Value::Bool(*b),
            Value::Int2(n) => serde_json::json!(*n),
            Value::Int4(n) => serde_json::json!(*n),
            Value::Int8(n) => serde_json::json!(*n),
            Value::Float4(f) => serde_json::json!(*f as f64),
            Value::Float8(f) => serde_json::json!(*f),
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::Bytes(b) => {
                // Encode bytes as hex string
                serde_json::Value::String(hex::encode(b))
            }
            Value::Uuid(u) => serde_json::Value::String(u.to_string()),
            Value::Timestamp(ts) => serde_json::Value::String(ts.to_rfc3339()),
            Value::Json(j) => {
                // Parse JSON string into serde_json::Value
                serde_json::from_str(j).unwrap_or_else(|_| serde_json::Value::String(j.clone()))
            }
            Value::Array(arr) => {
                // Recursively convert array elements
                let json_arr: Vec<serde_json::Value> = arr.iter().map(|v| {
                    match v {
                        Value::Null => serde_json::Value::Null,
                        Value::Boolean(b) => serde_json::Value::Bool(*b),
                        Value::Int2(n) => serde_json::json!(*n),
                        Value::Int4(n) => serde_json::json!(*n),
                        Value::Int8(n) => serde_json::json!(*n),
                        Value::Float4(f) => serde_json::json!(*f as f64),
                        Value::Float8(f) => serde_json::json!(*f),
                        Value::String(s) => serde_json::Value::String(s.clone()),
                        Value::Bytes(b) => serde_json::Value::String(hex::encode(b)),
                        Value::Uuid(u) => serde_json::Value::String(u.to_string()),
                        Value::Timestamp(ts) => serde_json::Value::String(ts.to_rfc3339()),
                        Value::Json(j) => {
                            serde_json::from_str(j).unwrap_or_else(|_| serde_json::Value::String(j.clone()))
                        }
                        _ => serde_json::Value::Null,
                    }
                }).collect();
                serde_json::Value::Array(json_arr)
            }
            _ => serde_json::Value::Null,
        };

        self.values.push(json_val);
        Ok(())
    }

    fn finalize(&self) -> Result<Value> {
        // Return the collected values as a JSON array
        let json_array = serde_json::Value::Array(self.values.clone());
        Ok(Value::Json(json_array.to_string()))
    }
}

// =============================================================================
// Factory function
// =============================================================================

/// Create an aggregate function by name
pub fn create_aggregate(name: &str) -> Option<Box<dyn AggregateFunction>> {
    match name.to_uppercase().as_str() {
        "COUNT" => Some(Box::new(CountFunction)),
        "SUM" => Some(Box::new(SumFunction)),
        "AVG" => Some(Box::new(AvgFunction)),
        "MIN" => Some(Box::new(MinFunction)),
        "MAX" => Some(Box::new(MaxFunction)),
        "STDDEV" | "STDDEV_SAMP" => Some(Box::new(StddevFunction)),
        "VARIANCE" | "VAR_SAMP" => Some(Box::new(VarianceFunction)),
        "JSON_AGG" => Some(Box::new(JsonAggFunction)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count() {
        let func = CountFunction;
        let mut state = func.init_state();

        state.accumulate(&Value::Int4(1)).unwrap();
        state.accumulate(&Value::Int4(2)).unwrap();
        state.accumulate(&Value::Null).unwrap();
        state.accumulate(&Value::Int4(3)).unwrap();

        assert_eq!(state.finalize().unwrap(), Value::Int8(3)); // NULL not counted
    }

    #[test]
    fn test_sum() {
        let func = SumFunction;
        let mut state = func.init_state();

        state.accumulate(&Value::Int4(10)).unwrap();
        state.accumulate(&Value::Int4(20)).unwrap();
        state.accumulate(&Value::Null).unwrap();
        state.accumulate(&Value::Int4(30)).unwrap();

        assert_eq!(state.finalize().unwrap(), Value::Float8(60.0));
    }

    #[test]
    fn test_avg() {
        let func = AvgFunction;
        let mut state = func.init_state();

        state.accumulate(&Value::Int4(10)).unwrap();
        state.accumulate(&Value::Int4(20)).unwrap();
        state.accumulate(&Value::Int4(30)).unwrap();

        assert_eq!(state.finalize().unwrap(), Value::Float8(20.0));
    }

    #[test]
    fn test_min_max() {
        let min_func = MinFunction;
        let max_func = MaxFunction;
        let mut min_state = min_func.init_state();
        let mut max_state = max_func.init_state();

        for v in [Value::Int4(5), Value::Int4(2), Value::Int4(8), Value::Int4(1)] {
            min_state.accumulate(&v).unwrap();
            max_state.accumulate(&v).unwrap();
        }

        assert_eq!(min_state.finalize().unwrap(), Value::Int4(1));
        assert_eq!(max_state.finalize().unwrap(), Value::Int4(8));
    }

    #[test]
    fn test_stddev_variance() {
        let stddev_func = StddevFunction;
        let variance_func = VarianceFunction;
        let mut stddev_state = stddev_func.init_state();
        let mut var_state = variance_func.init_state();

        // Sample: 2, 4, 4, 4, 5, 5, 7, 9
        // Mean = 5, Sum of squared deviations = 32
        // Sample Variance = 32/(n-1) = 32/7 ≈ 4.5714
        // Sample StdDev = sqrt(32/7) ≈ 2.1381
        for v in [2, 4, 4, 4, 5, 5, 7, 9] {
            stddev_state.accumulate(&Value::Int4(v)).unwrap();
            var_state.accumulate(&Value::Int4(v)).unwrap();
        }

        let variance = match var_state.finalize().unwrap() {
            Value::Float8(v) => v,
            _ => panic!("Expected Float8"),
        };
        let stddev = match stddev_state.finalize().unwrap() {
            Value::Float8(v) => v,
            _ => panic!("Expected Float8"),
        };

        // Sample variance = 32/7 ≈ 4.5714
        let expected_variance: f64 = 32.0 / 7.0;
        let expected_stddev: f64 = expected_variance.sqrt();
        assert!((variance - expected_variance).abs() < 0.001, "variance {} != {}", variance, expected_variance);
        assert!((stddev - expected_stddev).abs() < 0.001, "stddev {} != {}", stddev, expected_stddev);
    }
}
