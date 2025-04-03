// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::builder::build_from_env_like;
use crate::env::EnvLike;
use crate::errors::{BuildError, UsageError};
use crate::logger::Logger;
use crate::name::Name;
use crate::schema::Schema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const HOST_TEST_BINARY_OPTION: &str = "host_test_binary";
const OUTPUT_DIRECTORY_OPTION: &str = "output_directory";
const BINARY_OPTION: &str = "binary";
const TEST_CONFIG_FILE_PATTERN: &str = "test_config_file";
const RESOLVED_HOST_TEST_ARGS_EXPECT: &str =
    "TestConfig is validated prior to calling resolved_host_test_args";

/// Parameters describing a test to be run.
#[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
pub struct TestConfig {
    /// Path for the executable that this program should invoke to run the test.
    pub host_test_binary: PathBuf,

    /// Arguments to pass to `host_test_binary` to run the test. Substrings in curly braces are
    /// replaced with the value of the test parameter named in the braces. For example
    /// "--out={output_directory}" produces a single argument "--out=<foo>" where <foo> is the
    /// value of the `output_directory` test parameter. In addition, the substring
    /// "{test_config_file}" is replaced by the path to the file containing the test parameters
    /// in JSON format.
    ///
    /// If `host_test_args` is not supplied, the host test binary is invoked with two parameters:
    /// the path of the JSON file containing test parameters, and the path of the output directory.
    /// This is equivalent to a `host_test_args` value of
    /// ["{test_config_file}", "{output_directory}"].
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub host_test_args: Vec<String>,

    /// Path for test output.
    pub output_directory: PathBuf,

    // Processors to be applied to the output.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_processors: Vec<OutputProcessor>,

    /// Pass-through test parameters that are not known to this tool.
    #[serde(flatten)]
    pub unknown: HashMap<String, Value>,
}

impl TestConfig {
    /// Creates a new `TestConfig` from an `EnvLike`.
    pub fn from_env_like<E: EnvLike, L: Logger>(
        env_like: &E,
        schema: Schema,
        logger: &mut L,
    ) -> Result<Self, BuildError> {
        let to_return: Self = build_from_env_like(env_like, schema, logger)?;
        to_return.validate()?;
        Ok(to_return)
    }

    /// Returns the resolved host test arguments. Arguments are resolved by performing the
    /// parameter substitutions of parameter names surrounded with curly braces.
    #[allow(dead_code)]
    pub fn resolved_host_test_args<'a>(
        &'a self,
        test_config_file_path: &'a PathBuf,
    ) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        if self.host_test_args.is_empty() {
            // Default host test arguments are <path to JSON test params> <path to output dir>.
            return Box::new(
                [
                    Cow::from(
                        test_config_file_path.to_str().expect(RESOLVED_HOST_TEST_ARGS_EXPECT),
                    ),
                    Cow::from(
                        self.output_directory.to_str().expect(RESOLVED_HOST_TEST_ARGS_EXPECT),
                    ),
                ]
                .into_iter(),
            );
        }

        Box::new(
            self.host_test_args
                .iter()
                .map(|arg| self.format_arg(arg.as_str(), test_config_file_path)),
        )
    }

    /// Validates `self`.
    fn validate(&self) -> Result<(), UsageError> {
        validate_binary_path(&self.host_test_binary, Name::from_str(HOST_TEST_BINARY_OPTION))?;

        for arg in &self.host_test_args {
            self.validate_arg(arg)?;
        }

        for output_processor in &self.output_processors {
            validate_binary_path(&output_processor.binary, Name::from_str(BINARY_OPTION))?;

            for arg in &output_processor.args {
                self.validate_arg(arg)?;
            }
        }

        Ok(())
    }

    /// Validates a host test or output processor argument by ensuring that any substitutions
    /// refer to valid test parameters.
    fn validate_arg(&self, arg: &str) -> Result<(), UsageError> {
        let mut remaining_arg = arg;

        while let Some(open) = remaining_arg.find('{') {
            if let Some(close) = remaining_arg[open..].find('}') {
                self.validate_pattern(&remaining_arg[open + 1..open + close], arg)?;
                remaining_arg = &remaining_arg[open + close + 1..];
            } else {
                return Err(UsageError::UnterminatedArgPattern(String::from(arg)));
            }
        }

        Ok(())
    }

    /// Validates that `pattern` refers to a valid test or output processor parameter. `arg` is
    /// the argument containing the pattern and is used to construct an informative error.
    fn validate_pattern(&self, pattern: &str, arg: &str) -> Result<(), UsageError> {
        match pattern {
            HOST_TEST_BINARY_OPTION | OUTPUT_DIRECTORY_OPTION | TEST_CONFIG_FILE_PATTERN => Ok(()),
            _ if self.unknown.contains_key(pattern) => Ok(()),
            _ => Err(UsageError::UnknownArgPattern {
                unknown_parameter: String::from(pattern),
                pattern: String::from(arg),
            }),
        }
    }

    /// Formats a host test or output processor argument by performing substitutions. This
    /// function assumes that `self` has been validated.
    fn format_arg<'a>(&'a self, arg: &'a str, test_config_file_path: &PathBuf) -> Cow<'a, str> {
        if !arg.contains('{') {
            return Cow::from(arg);
        }

        let mut remaining_arg = arg;
        let mut result = String::new();

        while let Some(open) = remaining_arg.find('{') {
            let close =
                open + remaining_arg[open..].find('}').expect(RESOLVED_HOST_TEST_ARGS_EXPECT);
            result.push_str(&remaining_arg[..open]);
            result.push_str(
                self.value_for_pattern(&remaining_arg[open + 1..close], test_config_file_path),
            );
            remaining_arg = &remaining_arg[close + 1..];
        }

        result.push_str(&remaining_arg);

        result.into()
    }

    /// Returns the string value for a substitution pattern. `arg` is the argument containing
    /// the pattern. This function assumes that `self` has been validated.
    fn value_for_pattern<'a>(
        &'a self,
        pattern: &str,
        test_config_file_path: &'a PathBuf,
    ) -> &'a str {
        match pattern {
            HOST_TEST_BINARY_OPTION => {
                self.host_test_binary.to_str().expect(RESOLVED_HOST_TEST_ARGS_EXPECT)
            }
            OUTPUT_DIRECTORY_OPTION => {
                self.output_directory.to_str().expect(RESOLVED_HOST_TEST_ARGS_EXPECT)
            }
            TEST_CONFIG_FILE_PATTERN => {
                test_config_file_path.to_str().expect(RESOLVED_HOST_TEST_ARGS_EXPECT)
            }
            _ => self
                .unknown
                .get(pattern)
                .map(|value| match value {
                    // This special case avoids the quotes produced by `value.to_string()`.
                    Value::String(s) => s.as_str(),
                    _ => (value.as_str()).expect(RESOLVED_HOST_TEST_ARGS_EXPECT),
                })
                .expect(RESOLVED_HOST_TEST_ARGS_EXPECT),
        }
    }
}

/// A processor to be applied to the output.
#[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
pub struct OutputProcessor {
    /// Path for the executable that implements the output processor.
    pub binary: PathBuf,

    /// Arguments to pass to `binary` to run the processor. Substrings in curly braces are
    /// replaced with the value of the test parameter named in the braces. For example
    /// "--out={output_directory}" produces a single argument "--out=<foo>" where <foo> is the
    /// value of the `output_directory` test parameter. In addition, the substring
    /// "{test_config_file}" is replaced by the path to the file containing the test parameters
    /// in JSON format.
    ///
    /// If `host_test_args` is not supplied, the host test binary is invoked with two parameters:
    /// the path of the JSON file containing test parameters, and the path of the output directory.
    /// This is equivalent to a `host_test_args` value of
    /// ["{test_config_file}", "{output_directory}"].
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Whether the output processor should be invoked when the test run succeeds.
    #[serde(default)]
    pub use_on_success: bool,

    /// Whether the output processor should be invoked when the test run fails.
    #[serde(default)]
    pub use_on_failure: bool,
}

impl OutputProcessor {
    /// Returns the output processors arguments. Arguments are resolved by performing the
    /// parameter substitutions of parameter names surrounded with curly braces.
    #[allow(dead_code)]
    pub fn resolved_args<'a>(
        &'a self,
        test_config: &'a TestConfig,
        test_config_file_path: &'a PathBuf,
    ) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        if self.args.is_empty() {
            // Default host test arguments are <path to JSON test params> <path to output dir>.
            return Box::new(
                [
                    Cow::from(
                        test_config_file_path.to_str().expect(RESOLVED_HOST_TEST_ARGS_EXPECT),
                    ),
                    Cow::from(
                        test_config
                            .output_directory
                            .to_str()
                            .expect(RESOLVED_HOST_TEST_ARGS_EXPECT),
                    ),
                ]
                .into_iter(),
            );
        }

        Box::new(
            self.args.iter().map(|arg| test_config.format_arg(arg.as_str(), test_config_file_path)),
        )
    }
}

/// Validate a binary file path, checking that the file exists and is an executable file.
fn validate_binary_path(path: &PathBuf, option: Name) -> Result<(), UsageError> {
    if !path.exists() {
        return Err(UsageError::BinaryDoesNotExist { option, path: path.clone() });
    }

    if let Ok(metadata) = fs::metadata(&path) {
        if !metadata.is_file() {
            return Err(UsageError::BinaryIsNotAFile { option, path: path.clone() });
        }
    } else {
        return Err(UsageError::BinaryUnreadable { option, path: path.clone() });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::testutils::FakeEnv;
    use crate::logger::NullLogger;
    use assert_matches::assert_matches;
    use serde_json::json;
    use tempfile::NamedTempFile;

    pub fn test_schema() -> Schema {
        Schema::from_value(
            json!({
                "type": "object",
                "properties": {
                    "foo": { "type": "string" },
                    "bar": { "type": "string" },
                    "baz": { "type": "string" },
                    "output_directory": { "type": "string" },
                    "host_test_binary": { "type": "string" },
                    "host_test_args": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "output_processors": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "binary": {
                                    "type": "string",
                                },
                                "args": {
                                    "type": "array",
                                    "items": {
                                        "type": "string",
                                    },
                                },
                                "use_on_success": {
                                    "type": "boolean",
                                },
                                "use_on_failure": {
                                    "type": "boolean",
                                },
                            },
                            "required": [
                                "binary",
                            ],
                            "additionalProperties": false,
                        },
                    },
                },
                "required": [ "output_directory", "host_test_binary" ],
                "additionalProperties": false,
            }
            ),
            PathBuf::new(),
        )
        .expect("test schema is valid")
    }

    /// Asserts that a `Result<TestConfig, BuilderError>` wraps `usage_error`
    #[track_caller]
    fn assert_usage_error(result: Result<TestConfig, BuildError>, usage_error: UsageError) {
        assert_matches!(result, Err(BuildError::IncorrectUsage(e)) if e == usage_error)
    }

    #[test]
    // Tests construction of a `TestConfig` from an environment.
    fn test_config_validate() {
        // Missing required parameter host-test-binary.
        let fake_env = FakeEnv::new("", "");
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        match result {
            Err(BuildError::ValidationMultiple(errors)) => {
                assert_matches!(
                    &errors[0],
                    BuildError::IncorrectUsage(e)
                        if e == &UsageError::MissingParameterRequiredBySchema(
                            String::from(OUTPUT_DIRECTORY_OPTION)
                        )
                );
                assert_matches!(
                    &errors[1],
                    BuildError::IncorrectUsage(e)
                        if e == &UsageError::MissingParameterRequiredBySchema(
                            String::from(HOST_TEST_BINARY_OPTION)
                        )
                );
            }
            other => {
                panic!("expected Err(BuildError::ValidationMultiple(...)), got: {other:?}");
            }
        }

        let temp_file = NamedTempFile::new().expect("Failed to create temporary file");
        let temp_file_path = temp_file.path().display();

        // Missing required parameter output-directory.
        let fake_env = FakeEnv::new(format!("--host-test-binary={}", temp_file_path).as_str(), "");
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_usage_error(
            result,
            UsageError::MissingParameterRequiredBySchema(String::from(OUTPUT_DIRECTORY_OPTION)),
        );

        // Valid.
        let fake_env = FakeEnv::new(
            format!("--host-test-binary={} --output-directory=/nonexistent", temp_file_path)
                .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));

        // Missing explicitly required parameter foo.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --output-directory=/nonexistent --require=foo",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_usage_error(result, UsageError::MissingRequiredParameter(Name::from_str("foo")));

        // Defining explicitly prohibited parameter foo.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --output-directory=/nonexistent --prohibit=foo --foo=bar",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_usage_error(result, UsageError::DefinedProhibitedParameter(Name::from_str("foo")));

        // Using host-test-args pattern that references an unknown value.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{notathing}} --output-directory=/nonexistent",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_usage_error(
            result,
            UsageError::UnknownArgPattern {
                unknown_parameter: String::from("notathing"),
                pattern: String::from("{notathing}"),
            },
        );

        // Using an unterminated host-test-args pattern.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo --output-directory=/nonexistent",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_usage_error(result, UsageError::UnterminatedArgPattern(String::from("{foo")));

        // Valid, using a valid host-test-args pattern.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}} --output-directory=/nonexistent \
                     --foo=bar",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));

        temp_file.close().expect("Failed to close temporary file");
    }

    #[test]
    // Tests `resolved_host_test_args`.
    fn test_test_params_resolved_host_test_args() {
        let temp_file = NamedTempFile::new().expect("Failed to create temporary file");
        let temp_file_path = temp_file.path().display();

        // Default args.
        let fake_env = FakeEnv::new(
            format!("--host-test-binary={} --output-directory=/nonexistent", temp_file_path)
                .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["test/params/path", "/nonexistent"].into_iter()));

        // Multiple args, no patterns.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=1,true,thing \
                     --output-directory=/nonexistent",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1", "true", "thing"].into_iter()));

        // One arg, no patterns.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=just_one --output-directory=/nonexistent",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["just_one"].into_iter()));

        // Three args, one pattern in first position.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}},true,thing \
                     --output-directory=/nonexistent --foo=1",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1", "true", "thing"].into_iter()));

        // Three args, one pattern in second position.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=1,{{foo}},thing \
                     --output-directory=/nonexistent --foo=true",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1", "true", "thing"].into_iter()));

        // Three args, one pattern in third position.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=1,true,{{foo}} \
                     --output-directory=/nonexistent --foo=thing",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1", "true", "thing"].into_iter()));

        // Three args, three patterns.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}},{{bar}},{{baz}} \
                     --output-directory=/nonexistent --foo=1 --bar=true --baz=thing",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1", "true", "thing"].into_iter()));

        // One arg with initial pattern.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}}truething \
                     --output-directory=/nonexistent --foo=1",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        // One arg with embedded pattern.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=1{{foo}}thing \
                     --output-directory=/nonexistent --foo=true",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        // One arg with final pattern.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=1true{{foo}} \
                     --output-directory=/nonexistent --foo=thing",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        // One arg with two initial patterns.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}}{{bar}}thing \
                     --output-directory=/nonexistent --foo=1 --bar=true",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        // One arg with one initial and one final pattern.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}}true{{bar}} \
                     --output-directory=/nonexistent --foo=1 --bar=thing",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        // One arg with two final patterns.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args=1{{foo}}{{bar}} \
                     --output-directory=/nonexistent --foo=true --bar=thing",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        // One arg with three patterns.
        let fake_env = FakeEnv::new(
            format!(
                "--host-test-binary={} --host-test-args={{foo}}{{bar}}{{baz}} \
                     --output-directory=/nonexistent --foo=1 --bar=true --baz=thing",
                temp_file_path
            )
            .as_str(),
            "",
        );
        let result = TestConfig::from_env_like(&fake_env, test_schema(), &mut NullLogger);
        assert_matches!(result, Ok(_));
        assert!(result
            .unwrap()
            .resolved_host_test_args(&PathBuf::from("test/params/path"))
            .eq(vec!["1truething"].into_iter()));

        temp_file.close().expect("Failed to close temporary file");
    }
}
