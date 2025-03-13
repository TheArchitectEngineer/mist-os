// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::name::Name;
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use thiserror::Error;
use valico::json_schema::validators::ValidationState;

/// Error encountered validating config
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Schema file {path} could not be opened for reading")]
    FailedToOpenSchema {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("Failure attempting to read schema {path}")]
    FailedToReadSchema {
        path: PathBuf,
        #[source]
        source: serde_json5::Error,
    },

    #[error("Failure attempting to parse schema {path}")]
    FailedToParseSchema {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("Schema not well-formed: {0}")]
    InvalidSchema(String),

    #[error("Failed to parse test configuration as aggregated JSON value")]
    FailedToParse(#[from] serde_json::Error),

    #[error("Include file {path} could not be opened for reading")]
    FailedToOpenInclude {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("Failure attempting to parse include {path}")]
    FailedToParseInclude {
        path: PathBuf,
        #[source]
        source: serde_json5::Error,
    },

    #[error("Incorrect usage")]
    IncorrectUsage(#[from] UsageError),

    #[error("Multiple validation errors: {0:?}")]
    ValidationMultiple(Vec<BuildError>),

    // The valico error here is deliberately formatted instead of being included explicitly.
    // Neither valico errors nor validation_state are cloneable, creating problems when we
    // want to capture them in errors. Also, valico errors are not std::error::Error.
    #[error("Unclassified schema error, please file a bug to classify {0}")]
    UnclassifiedSchemaError(String),

    #[error("Unclassified schema state, please file a bug to classify {0:?}")]
    UnclassifiedSchemaState(Box<ValidationState>),
}

/// Error encountered processing command line arguments, environment variables, or
/// included JSON files.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum UsageError {
    #[error("Unexpected positional argument '{0}'")]
    UnexpectedPositionalArgument(String),

    #[error("Argument '{0}' missing required value")]
    MissingValue(Name),

    #[error("Parameter name '{0}' does not appear in the test config schema")]
    UnrecognizedParameter(Name),

    #[error("Option '{option}' has unexpected value {got}")]
    UnexpectedOptionValue { option: Name, got: Value },

    #[error("Invalid 'strict' value: {0:?}. 'strict' can only be set to true")]
    InvalidStrictValue(String),

    #[error("Included path {0} does not exist")]
    IncludedPathDoesNotExist(PathBuf),

    #[error("Included path {0} cannot be read (user lacks permission?)")]
    IncludedPathUnreadable(PathBuf),

    #[error("Included path {0} is not a file")]
    IncludedPathIsNotAFile(PathBuf),

    #[error("Parameter '{0}' is specified in 'env' option, but was not in the schema")]
    UnknownEnvParameter(Name),

    #[error("Parameter '{0}' is required, but was not in the schema")]
    UnknownRequiredParameter(Name),

    #[error("Parameter '{0}' is required, but was not defined")]
    MissingRequiredParameter(Name),

    #[error("Parameter '{0}' is prohibited, but was not in the schema")]
    UnknownProhibitedParameter(Name),

    #[error("Parameter '{0}' is prohibited, but was defined")]
    DefinedProhibitedParameter(Name),

    #[error("Expected viable parameter names for option '{option}', got {got}")]
    InvalidParameterName { option: Name, got: Name },

    #[error("Unterminated pattern in argument {0}")]
    UnterminatedArgPattern(String),

    #[error("Unknown parameter {unknown_parameter} referenced in argument pattern {pattern}")]
    UnknownArgPattern { unknown_parameter: String, pattern: String },

    #[error("Parameter {0} was assigned more than one value in strict mode")]
    ParamAlreadyStrictlyAssigned(Name),

    #[error("Wrong type: expected type {expected}, got \"{got}\" for parameter {parameter}")]
    TypeMismatch { expected: String, got: String, parameter: Name },

    #[error("Multiple values supplied for non-array parameter {parameter}: \"{got}\"")]
    CommasNotAllowed { parameter: Name, got: String },

    #[error("Parameters of type object (such as {0}) are not allowed in command lines or environment variables")]
    ObjectNotAllowed(Name),

    #[error("Parameter '{0}' is required by the schema, but was not defined")]
    MissingParameterRequiredBySchema(String),

    #[error("Type of parameter {parameter} disagrees with schema, {detail}")]
    SchemaTypeMismatch { parameter: String, detail: String },

    #[error(
        "Parameters of type array must have simple items in command lines or environment variables"
    )]
    ArrayOfComplexTypeNotAllowed(Name),
}
