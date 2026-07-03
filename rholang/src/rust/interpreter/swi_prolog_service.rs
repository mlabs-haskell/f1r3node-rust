use std::env;
use std::fs::{remove_file, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use models::rhoapi::Par;
use serde_json::Value;
use tempfile::NamedTempFile;
use tokio::process::Command;

use super::errors::InterpreterError;
use crate::rust::interpreter::rho_type::{
    RhoBoolean, RhoList, RhoMap, RhoNil, RhoNumber, RhoString,
};

/// Executes MeTTa code through the PeTTa (SWI-Prolog) interpreter and returns the result as a Rholang Par.
///
/// # Overview
///
/// This function provides low-level access to the PeTTa interpreter, which
/// provides MeTTa execution with SWI-Prolog. The execution is sandboxed using bubblewrap
/// and libsandbox.so (from Cloudflare) to apply syscall whitelisting. The execution has
/// a 10-second timeout to prevent runaway computations.
///
/// # Arguments
///
/// * `metta_code` - A string containing valid MeTTa code to execute
///
/// # Returns
///
/// Returns `Ok(Par)` containing the execution result as a Rholang Par structure, or an
/// `InterpreterError::SwiplError` if execution fails.
///
/// # JSON Output Schema
///
/// PeTTa returns results as JSON in the following envelope:
/// ```json
/// {"results": [...]}
/// ```
///
/// The `results` field contains a JSON array of MeTTa execution results. This entire JSON
/// structure is converted to a Rholang Par using the following mapping:
///
/// - `null` → `RhoNil`
/// - `true`/`false` → `RhoBoolean`
/// - Numbers → `RhoNumber` (must fit in i64, otherwise error)
/// - Strings → `RhoString`
/// - Arrays → `RhoList` (recursive conversion of elements)
/// - Objects → `RhoMap` (keys converted to RhoString, values recursively converted)
///
/// # Error Conditions
///
/// - `InterpreterError::SwiplError("Can't find petta.sh script.")` - petta.sh script not found at `$PETTA_SCRIPT_PATH`
/// - `InterpreterError::SwiplError("PETTA_DIR environment variable not set")` - PETTA_DIR not set
/// - `InterpreterError::SwiplError("CACHE_DIR environment variable not set")` - CACHE_DIR not set
/// - `InterpreterError::SwiplError("SANDBOX_LIB_PATH environment variable not set")` - SANDBOX_LIB_PATH not set
/// - `InterpreterError::SwiplError("MeTTa execution timed out...")` - Execution exceeded 10 seconds
/// - `InterpreterError::SwiplError("PeTTa execution failed...")` - petta.sh returned error
/// - `InterpreterError::SwiplError("Can't parse JSON output...")` - Invalid JSON from PeTTa
/// - `InterpreterError::SwiplError("Could not parse number as i64")` - Number exceeds i64 range
///
/// # Environment Variables
///
/// - `PETTA_SCRIPT_PATH` - Path to petta.sh script (default: `./petta.sh`)
/// - `PETTA_DIR` - Path to PeTTa installation directory (required)
/// - `CACHE_DIR` - Path to cache directory for patched libraries (required)
/// - `SANDBOX_LIB_PATH` - Path to libsandbox.so library (required)
///
/// # Timeout
///
/// Execution is limited to 10 seconds. Long-running computations will be terminated and return
/// a timeout error. This prevents malicious or buggy MeTTa code from blocking the node.
///
/// # Examples
///
/// ```ignore
/// // Simple arithmetic
/// let result = petta_execute("!(+ 1 2)").await?;
///
/// // Pattern matching
/// let result = petta_execute(
///     "(= (swap (Pair $x $y)) (Pair $y $x)) !(swap (Pair 1 3))"
/// ).await?;
/// ```
///
/// # See Also
///
/// - [`system_processes::petta_execute`] - System process wrapper for Rholang contracts
/// - [`value_to_par`] - JSON to Par conversion logic
pub async fn petta_execute(metta_code: &str) -> Result<Par, InterpreterError> {
    // Write the MeTTa code to a temp file
    let mut metta_file = NamedTempFile::new()
        .map_err(|_| InterpreterError::SwiplError("Can't open temp file".into()))?;
    metta_file
        .write_all(metta_code.as_bytes())
        .map_err(|_| InterpreterError::SwiplError("Can't write MeTTa code to temp file".into()))?;
    metta_file
        .flush()
        .map_err(|_| InterpreterError::SwiplError("Can't flush MeTTa temp file".into()))?;

    let metta_file_path = metta_file
        .path()
        .to_str()
        .ok_or(InterpreterError::SwiplError(
            "Can't convert metta_file path to string".into(),
        ))?;

    if metta_file_path.contains('\'') {
        return Err(InterpreterError::SwiplError(
            "Temp file path contains unsafe character (single quote)".into(),
        ));
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(metta_file_path)?;
    file.write_all(metta_code.as_bytes())?;
    drop(file);

    let result = async {
        let petta_script_path =
            PathBuf::from(env::var("PETTA_SCRIPT_PATH").unwrap_or("./petta.sh".into()));

        if !petta_script_path.exists() {
            return Err(InterpreterError::SwiplError(
                "Can't find petta.sh script.".into(),
            ));
        }

        let petta_dir = env::var("PETTA_DIR").map_err(|_| {
            InterpreterError::SwiplError("PETTA_DIR environment variable not set".into())
        })?;

        let cache_dir = env::var("CACHE_DIR").map_err(|_| {
            InterpreterError::SwiplError("CACHE_DIR environment variable not set".into())
        })?;

        let sandbox_lib_path = env::var("SANDBOX_LIB_PATH").map_err(|_| {
            InterpreterError::SwiplError("SANDBOX_LIB_PATH environment variable not set".into())
        })?;

        let timeout_secs: u64 = 10;
        let proc_handle = tokio::spawn(tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            Command::new(&petta_script_path)
                .arg(metta_file_path)
                .env("PETTA_DIR", petta_dir)
                .env("CACHE_DIR", cache_dir)
                .env("SANDBOX_LIB_PATH", sandbox_lib_path)
                .kill_on_drop(true)
                .output(),
        ));

        let output = proc_handle
            .await
            .map_err(|join_error| {
                InterpreterError::SwiplError(
                    format!("Error while joining with the PeTTa task: {}", join_error).into(),
                )
            })?
            .map_err(|elapsed| {
                InterpreterError::SwiplError(
                    format!("MeTTa execution timed out after {}", elapsed).into(),
                )
            })?
            .map_err(|e| {
                InterpreterError::SwiplError(format!("MeTTa execution failed: {}", e).into())
            })?;

        if !output.status.success() {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            return Err(InterpreterError::SwiplError(
                format!("PeTTa execution failed. stderr: {}", stderr_str).into(),
            ));
        }

        let str_output = String::from_utf8(output.stdout)
            .map_err(|_| InterpreterError::SwiplError("Can't interpret PeTTa output".into()))?;

        let value_output = serde_json::from_str::<Value>(str_output.as_str()).map_err(|e| {
            InterpreterError::SwiplError(
                format!(
                    "Can't parse JSON output from PeTTa execution: {}. Output was: {}",
                    e, str_output
                )
                .into(),
            )
        })?;

        let par_output = value_to_par(value_output)?;
        Ok(par_output)
    }
    .await;

    remove_file(metta_file_path).ok();

    result
}

/// Converts a JSON Value to a Rholang Par structure.
///
/// This function recursively transforms JSON data returned by PeTTa into Rholang's internal
/// representation (Par). It is used internally by [`petta_execute`] to convert PeTTa results.
///
/// # Type Mapping
///
/// | JSON Type | Rholang Type | Notes |
/// |-----------|--------------|-------|
/// | `null` | `RhoNil` | Represents absence of value |
/// | `boolean` | `RhoBoolean` | Direct mapping |
/// | `number` | `RhoNumber` | **Must be integer in i64 range**, floats rejected |
/// | `string` | `RhoString` | Direct mapping, supports Unicode |
/// | `array` | `RhoList` | Elements recursively converted |
/// | `object` | `RhoMap` | Keys stringified, values recursively converted |
///
/// # Important Constraints
///
/// - **Only integers are supported**: JSON numbers must be integers in the range `i64::MIN` to
///   `i64::MAX`. Floating-point numbers and integers outside this range are explicitly rejected
///   with descriptive errors to prevent silent truncation and maintain consensus safety.
/// - **Object keys become strings**: All JSON object keys are converted to `RhoString` in the
///   resulting `RhoMap`.
/// - **Recursive conversion**: Nested structures (arrays in arrays, objects in objects, etc.)
///   are fully supported and recursively converted.
///
/// # Arguments
///
/// * `v` - A `serde_json::Value` to convert
///
/// # Returns
///
/// Returns `Ok(Par)` with the converted structure, or `InterpreterError::SwiplError` if
/// conversion fails (e.g., number doesn't fit in i64).
///
/// # Examples
///
/// ```ignore
/// use serde_json::json;
///
/// // Simple values
/// let nil = value_to_par(json!(null))?;
/// let bool = value_to_par(json!(true))?;
/// let num = value_to_par(json!(42))?;
/// let str = value_to_par(json!("hello"))?;
///
/// // Collections
/// let list = value_to_par(json!([1, 2, 3]))?;
/// let map = value_to_par(json!({"key": "value"}))?;
///
/// // Nested structures
/// let nested = value_to_par(json!({
///     "list": [1, 2, 3],
///     "map": {"inner": "value"}
/// }))?;
/// ```
fn value_to_par(v: Value) -> Result<Par, InterpreterError> {
    match v {
        Value::Null => Ok(RhoNil::create_par()),
        Value::Bool(b) => Ok(RhoBoolean::create_par(b)),
        Value::Number(n) => {
            if !n.is_i64() {
                if n.is_f64() {
                    return Err(InterpreterError::SwiplError(
                        format!(
                            "Floating-point numbers are not supported. Got: {}. \
                             Only integers in the range {} to {} are allowed.",
                            n,
                            i64::MIN,
                            i64::MAX
                        )
                        .into(),
                    ));
                } else {
                    return Err(InterpreterError::SwiplError(
                        format!(
                            "Number exceeds i64 range. Got: {}. \
                             Only integers in the range {} to {} are allowed.",
                            n,
                            i64::MIN,
                            i64::MAX
                        )
                        .into(),
                    ));
                }
            }
            let n64 = n.as_i64().unwrap();
            Ok(RhoNumber::create_par(n64))
        }
        Value::String(s) => Ok(RhoString::create_par(s)),
        Value::Array(values) => {
            let ps = values
                .into_iter()
                .map(value_to_par)
                .collect::<Result<_, _>>()?;
            Ok(RhoList::create_par(ps))
        }
        Value::Object(map) => {
            let hashmap = map
                .into_iter()
                .map(|(k, v)| {
                    let p = value_to_par(v)?;
                    Ok((RhoString::create_par(k), p))
                })
                .collect::<Result<_, InterpreterError>>()?;
            Ok(RhoMap::create_par(hashmap))
        }
    }
}

///// TESTS /////

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    #[test]
    fn test_value_to_par_null() {
        let result = value_to_par(json!(null)).unwrap();
        assert_eq!(result, RhoNil::create_par());
    }
    #[test]
    fn test_value_to_par_boolean() {
        let result = value_to_par(json!(true)).unwrap();
        assert_eq!(result, RhoBoolean::create_par(true));
    }
    #[test]
    fn test_value_to_par_small_number() {
        let result = value_to_par(json!(42)).unwrap();
        assert_eq!(result, RhoNumber::create_par(42));
    }
    #[test]
    fn test_value_to_par_max_i64() {
        let result = value_to_par(json!(i64::MAX)).unwrap();
        assert_eq!(result, RhoNumber::create_par(i64::MAX));
    }
    #[test]
    fn test_value_to_par_string() {
        let result = value_to_par(json!("hello")).unwrap();
        assert_eq!(result, RhoString::create_par("hello".into()));
    }
    #[test]
    fn test_value_to_par_array() {
        let result = value_to_par(json!([1, "two", true])).unwrap();
        assert_eq!(
            result,
            RhoList::create_par(vec![
                RhoNumber::create_par(1),
                RhoString::create_par("two".into()),
                RhoBoolean::create_par(true)
            ])
        );
    }
    #[test]
    fn test_value_to_par_nested_array() {
        let result = value_to_par(json!([[1, 2], [3, 4]])).unwrap();
        assert_eq!(
            result,
            RhoList::create_par(vec![
                RhoList::create_par(vec![RhoNumber::create_par(1), RhoNumber::create_par(2)]),
                RhoList::create_par(vec![RhoNumber::create_par(3), RhoNumber::create_par(4)])
            ])
        );
    }
    #[test]
    fn test_value_to_par_object() {
        let result = value_to_par(json!({"key": "value", "num": 123})).unwrap();
        assert_eq!(
            result,
            RhoMap::create_par(
                vec![
                    (
                        RhoString::create_par("key".into()),
                        RhoString::create_par("value".into())
                    ),
                    (
                        RhoString::create_par("num".into()),
                        RhoNumber::create_par(123)
                    )
                ]
                .into_iter()
                .collect()
            )
        );
    }
    #[test]
    fn test_value_to_par_nested_object() {
        let result = value_to_par(json!({
            "outer": {
                "inner": "value"
            }
        }))
        .unwrap();
        assert_eq!(
            result,
            RhoMap::create_par(
                vec![(
                    RhoString::create_par("outer".into()),
                    RhoMap::create_par(
                        vec![(
                            RhoString::create_par("inner".into()),
                            RhoString::create_par("value".into())
                        )]
                        .into_iter()
                        .collect()
                    )
                )]
                .into_iter()
                .collect()
            )
        );
    }

    #[test]
    fn test_value_to_par_complex_nested_structure() {
        let result = value_to_par(json!({
            "list": [1, 2, 3],
            "nested": {
                "bool": true,
                "null": null
            }
        }))
        .unwrap();

        assert_eq!(
            result,
            RhoMap::create_par(
                vec![
                    (
                        RhoString::create_par("list".into()),
                        RhoList::create_par(vec![
                            RhoNumber::create_par(1),
                            RhoNumber::create_par(2),
                            RhoNumber::create_par(3)
                        ])
                    ),
                    (
                        RhoString::create_par("nested".into()),
                        RhoMap::create_par(
                            vec![
                                (
                                    RhoString::create_par("bool".into()),
                                    RhoBoolean::create_par(true)
                                ),
                                (RhoString::create_par("null".into()), RhoNil::create_par())
                            ]
                            .into_iter()
                            .collect()
                        )
                    )
                ]
                .into_iter()
                .collect()
            )
        );
    }

    #[test]
    fn test_value_to_par_empty_array() {
        let result = value_to_par(json!([])).unwrap();
        assert_eq!(result, RhoList::create_par(vec![]));
    }

    #[test]
    fn test_value_to_par_empty_object() {
        let result = value_to_par(json!({})).unwrap();
        assert_eq!(result, RhoMap::create_par(std::collections::HashMap::new()));
    }

    #[test]
    fn test_value_to_par_negative_number() {
        let result = value_to_par(json!(-42)).unwrap();
        assert_eq!(result, RhoNumber::create_par(-42));
    }

    #[test]
    fn test_value_to_par_zero() {
        let result = value_to_par(json!(0)).unwrap();
        assert_eq!(result, RhoNumber::create_par(0));
    }

    #[test]
    fn test_value_to_par_false_boolean() {
        let result = value_to_par(json!(false)).unwrap();
        assert_eq!(result, RhoBoolean::create_par(false));
    }

    #[test]
    fn test_value_to_par_empty_string() {
        let result = value_to_par(json!("")).unwrap();
        assert_eq!(result, RhoString::create_par("".into()));
    }

    #[test]
    fn test_value_to_par_unicode_string() {
        let result = value_to_par(json!("Hello, 世界! 🌍")).unwrap();
        assert_eq!(result, RhoString::create_par("Hello, 世界! 🌍".into()));
    }

    #[test]
    fn test_value_to_par_float_rejected() {
        let result = value_to_par(json!(3.14));
        assert!(result.is_err(), "Floating-point numbers should be rejected");
        let err = result.unwrap_err();
        let err_msg = format!("{:?}", err);
        assert!(
            err_msg.contains("Floating-point") && err_msg.contains("not supported"),
            "Error should mention floating-point not supported, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_value_to_par_large_uint_rejected() {
        let large_uint = u64::MAX;
        let result = value_to_par(json!(large_uint));
        assert!(
            result.is_err(),
            "Numbers exceeding i64::MAX should be rejected"
        );
        let err = result.unwrap_err();
        let err_msg = format!("{:?}", err);
        assert!(
            err_msg.contains("exceeds i64 range") || err_msg.contains("not supported"),
            "Error should mention range exceeded, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_value_to_par_min_i64() {
        let result = value_to_par(json!(i64::MIN)).unwrap();
        assert_eq!(result, RhoNumber::create_par(i64::MIN));
    }

    /// Test that petta_execute times out after 10 seconds for long-running computations.
    /// This uses a large fibonacci number that should exceed the timeout.
    #[tokio::test]
    async fn test_petta_execute_timeout() {
        use crate::rust::interpreter::test_utils::utils::should_skip_petta_test;

        if should_skip_petta_test() {
            return;
        }

        // Fibonacci of a very large number should timeout (10 second limit)
        // fib(10000000) will take much longer than 10 seconds
        let metta_code = r#"
            (= (fib-tr $n $a $b) (if (== $n 0) $a (fib-tr (- $n 1) $b (+ $a $b))))
            (= (fib $n) (fib-tr $n 0 1))
            !(fib 10000000)
        "#;

        let result = petta_execute(metta_code).await;

        // Should fail with a timeout error
        assert!(
            result.is_err(),
            "Large fibonacci computation should timeout"
        );

        let err = result.unwrap_err();
        let err_msg = format!("{:?}", err);

        // Error should mention timeout
        assert!(
            err_msg.contains("timed out") || err_msg.contains("timeout"),
            "Error should be a timeout error, got: {}",
            err_msg
        );
    }
}
