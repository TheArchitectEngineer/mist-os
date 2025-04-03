// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Library for generating structured configuration accessors. Each generated
//! language-specific library depends on the output of [`create_fidl_source`].

pub mod cpp;
pub mod fidl;
pub mod rust;

use syn::Error as SynError;
use thiserror::Error;

// TODO(https://fxbug.dev/42173254): This list should be kept in sync with fidlgen_rust.
const RESERVED_SUFFIXES: [&str; 7] =
    ["Impl", "Marker", "Proxy", "ProxyProtocol", "ControlHandle", "Responder", "Server"];

// TODO(https://fxbug.dev/42173254): This list should be kept in sync with fidlgen_rust.
const RESERVED_WORDS: [&str; 73] = [
    "as",
    "box",
    "break",
    "const",
    "continue",
    "crate",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    // Keywords reserved for future use (future-proofing...)
    "abstract",
    "alignof",
    "await",
    "become",
    "do",
    "final",
    "macro",
    "offsetof",
    "override",
    "priv",
    "proc",
    "pure",
    "sizeof",
    "typeof",
    "unsized",
    "virtual",
    "yield",
    // Weak keywords (special meaning in specific contexts)
    // These are ok in all contexts of fidl names.
    //"default",
    //"union",

    // Things that are not keywords, but for which collisions would be very unpleasant
    "Result",
    "Ok",
    "Err",
    "Vec",
    "Option",
    "Some",
    "None",
    "Box",
    "Future",
    "Stream",
    "Never",
    "Send",
    "fidl",
    "futures",
    "zx",
    "async",
    "on_open",
    "OnOpen",
    // TODO(https://fxbug.dev/42145610): Remove "WaitForEvent".
    "wait_for_event",
    "WaitForEvent",
];

/// Error from generating a source file
#[derive(Debug, Error)]
pub enum SourceGenError {
    #[error("The given string `{input}` is not a valid Rust identifier")]
    InvalidIdentifier { input: String, source: SynError },
}

// TODO(https://fxbug.dev/42173254): This logic should be kept in sync with fidlgen_rust.
fn normalize_field_key(key: &str) -> String {
    let mut identifier = String::new();
    let mut saw_lowercase_or_digit = false;
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            if saw_lowercase_or_digit {
                // A lowercase letter or digit preceded this uppercase letter.
                // Break this into two words.
                identifier.push('_');
                saw_lowercase_or_digit = false;
            }
            identifier.push(c.to_ascii_lowercase());
        } else if c == '_' {
            identifier.push('_');
            saw_lowercase_or_digit = false;
        } else {
            identifier.push(c);
            saw_lowercase_or_digit = true;
        }
    }

    if RESERVED_WORDS.contains(&key) || RESERVED_SUFFIXES.iter().any(|s| key.starts_with(s)) {
        identifier.push('_')
    }

    identifier
}

#[cfg(test)]
mod tests {
    use super::*;
    use cm_rust::ConfigChecksum;
    use fidl_fuchsia_component_config_ext::config_decl;
    use pretty_assertions::assert_eq;
    use quote::quote;

    fn test_checksum() -> ConfigChecksum {
        // sha256("Back to the Fuchsia")
        ConfigChecksum::Sha256([
            0xb5, 0xf9, 0x33, 0xe8, 0x94, 0x56, 0x3a, 0xf9, 0x61, 0x39, 0xe5, 0x05, 0x79, 0x4b,
            0x88, 0xa5, 0x3e, 0xd4, 0xd1, 0x5c, 0x32, 0xe2, 0xb4, 0x49, 0x9e, 0x42, 0xeb, 0xa3,
            0x32, 0xb1, 0xf5, 0xbb,
        ])
    }

    /// This test makes sure we sensibly handle config schemas that would normally be banned by cmc.
    /// It's like a golden test but the golden infrastructure relies on being able to use real
    /// component manifests so this operates directly on TokenStreams.
    #[test]
    fn bad_field_names() {
        let decl = config_decl! {
            ck@ test_checksum(),
            snake_case_string: { bool },
            lowerCamelCaseString: { bool },
            UpperCamelCaseString: { bool },
            CONST_CASE: { bool },
            stringThatHas02Digits: { bool },
            mixedLowerCamel_snakeCaseString: { bool },
            MixedUpperCamel_SnakeCaseString: { bool },
            multiple__underscores: { bool },
            unsafe: { bool },
            ServerMode: { bool },
        };

        let observed_fidl_src = fidl::create_fidl_source(&decl, "cf.sc.internal".to_string());
        let expected_fidl_src = "
library cf.sc.internal;

type Config = struct {
  snake_case_string bool;
  lower_camel_case_string bool;
  upper_camel_case_string bool;
  const_case bool;
  string_that_has02_digits bool;
  mixed_lower_camel_snake_case_string bool;
  mixed_upper_camel_snake_case_string bool;
  multiple__underscores bool;
  unsafe_ bool;
  server_mode_ bool;
};
";
        assert_eq!(observed_fidl_src, expected_fidl_src);

        let actual_rust_src =
            rust::create_rust_wrapper(&decl, "cf.sc.internal".to_string()).unwrap();

        let expected_rust_src = quote! {
            use fidl_cf_sc_internal::Config as FidlConfig;
            use fidl::unpersist;
            use fuchsia_inspect::{Node};
            use fuchsia_runtime::{take_startup_handle, HandleInfo, HandleType};
            use std::convert::TryInto;

            // This is generated from the config schema for the component. Component Manager also
            // computes this in parallel to allow config libraries that its config VMO's ABI matches
            // expectations.
            const EXPECTED_CHECKSUM: &[u8] = &[
                0xb5, 0xf9, 0x33, 0xe8, 0x94, 0x56, 0x3a, 0xf9, 0x61, 0x39, 0xe5, 0x05, 0x79, 0x4b,
                0x88, 0xa5, 0x3e, 0xd4, 0xd1, 0x5c, 0x32, 0xe2, 0xb4, 0x49, 0x9e, 0x42, 0xeb, 0xa3,
                0x32, 0xb1, 0xf5, 0xbb
            ];

            #[derive(Debug)]
            pub struct Config {
                pub snake_case_string: bool,
                pub lower_camel_case_string: bool,
                pub upper_camel_case_string: bool,
                pub const_case: bool,
                pub string_that_has02_digits: bool,
                pub mixed_lower_camel_snake_case_string: bool,
                pub mixed_upper_camel_snake_case_string: bool,
                pub multiple__underscores: bool,
                pub unsafe_: bool,
                pub server_mode_: bool
            }

            impl Config {
                /// Take the config startup handle and parse its contents.
                ///
                /// # Panics
                ///
                /// If the config startup handle was already taken or if it is not valid.
                pub fn take_from_startup_handle() -> Self {
                    let handle_info = HandleInfo::new(HandleType::ComponentConfigVmo, 0);
                    let config_vmo: zx::Vmo = take_startup_handle(handle_info)
                        .expect("Config VMO handle must be present.")
                        .into();
                    Self::from_vmo(&config_vmo).expect("Config VMO handle must be valid.")
                }

                /// Parse `Self` from `vmo`.
                pub fn from_vmo(vmo: &zx::Vmo) -> Result<Self, Error> {
                    let config_size = vmo.get_content_size().map_err(Error::GettingContentSize)?;
                    let config_bytes =
                        vmo.read_to_vec(0, config_size).map_err(Error::ReadingConfigBytes)?;
                    Self::from_bytes(&config_bytes)
                }

                /// Parse `Self` from `bytes`.
                pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
                    let (checksum_len_bytes, bytes) =
                        bytes.split_at_checked(2).ok_or(Error::TooFewBytes)?;
                    let checksum_len_bytes: [u8; 2] = checksum_len_bytes
                        .try_into()
                        .expect("previous call guaranteed 2 element slice");
                    let checksum_length = u16::from_le_bytes(checksum_len_bytes) as usize;
                    let (observed_checksum, bytes) =
                        bytes.split_at_checked(checksum_length).ok_or(Error::TooFewBytes)?;
                    if observed_checksum != EXPECTED_CHECKSUM {
                        return Err(Error::ChecksumMismatch {
                            observed_checksum: observed_checksum.to_vec(),
                        });
                    }
                    let fidl_config: FidlConfig = unpersist(bytes).map_err(Error::Unpersist)?;
                    Ok(Self {
                        snake_case_string: fidl_config.snake_case_string,
                        lower_camel_case_string: fidl_config.lower_camel_case_string,
                        upper_camel_case_string: fidl_config.upper_camel_case_string,
                        const_case: fidl_config.const_case,
                        string_that_has02_digits: fidl_config.string_that_has02_digits,
                        mixed_lower_camel_snake_case_string: fidl_config
                            .mixed_lower_camel_snake_case_string,
                        mixed_upper_camel_snake_case_string: fidl_config
                            .mixed_upper_camel_snake_case_string,
                        multiple__underscores: fidl_config.multiple__underscores,
                        unsafe_: fidl_config.unsafe_,
                        server_mode_: fidl_config.server_mode_
                    })
                }
                pub fn record_inspect(&self, inspector_node: &Node) {
                    inspector_node.record_bool("snake_case_string", self.snake_case_string);
                    inspector_node
                        .record_bool("lowerCamelCaseString", self.lower_camel_case_string);
                    inspector_node
                        .record_bool("UpperCamelCaseString", self.upper_camel_case_string);
                    inspector_node.record_bool("CONST_CASE", self.const_case);
                    inspector_node
                        .record_bool("stringThatHas02Digits", self.string_that_has02_digits);
                    inspector_node.record_bool(
                        "mixedLowerCamel_snakeCaseString",
                        self.mixed_lower_camel_snake_case_string
                    );
                    inspector_node.record_bool(
                        "MixedUpperCamel_SnakeCaseString",
                        self.mixed_upper_camel_snake_case_string
                    );
                    inspector_node.record_bool("multiple__underscores", self.multiple__underscores);
                    inspector_node.record_bool("unsafe", self.unsafe_);
                    inspector_node.record_bool("ServerMode", self.server_mode_);
                }
            }

            #[derive(Debug)]
            pub enum Error {
                /// Failed to read the content size of the VMO.
                GettingContentSize(zx::Status),
                /// Failed to read the content of the VMO.
                ReadingConfigBytes(zx::Status),
                /// The VMO was too small for this config library.
                TooFewBytes,
                /// The VMO's config ABI checksum did not match this library's.
                ChecksumMismatch { observed_checksum: Vec<u8> },
                /// Failed to parse the non-checksum bytes of the VMO as this library's FIDL type.
                Unpersist(fidl::Error),
            }

            impl std::fmt::Display for Error {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    match self {
                        Self::GettingContentSize(status) => {
                            write!(f, "Failed to get content size: {status}")
                        }
                        Self::ReadingConfigBytes(status) => {
                            write!(f, "Failed to read VMO content: {status}")
                        }
                        Self::TooFewBytes => {
                            write!(f, "VMO content is not large enough for this config library.")
                        }
                        Self::ChecksumMismatch { observed_checksum } => {
                            write!(
                                f,
                                "ABI checksum mismatch, expected {:?}, got {:?}",
                                EXPECTED_CHECKSUM, observed_checksum,
                            )
                        }
                        Self::Unpersist(fidl_error) => {
                            write!(f, "Failed to parse contents of config VMO: {fidl_error}")
                        }
                    }
                }
            }

            impl std::error::Error for Error {
                #[allow(unused_parens, reason = "rustfmt errors without parens here")]
                fn source(&self) -> Option<(&'_ (dyn std::error::Error + 'static))> {
                    match self {
                        Self::GettingContentSize(ref status)
                        | Self::ReadingConfigBytes(ref status) => Some(status),
                        Self::TooFewBytes => None,
                        Self::ChecksumMismatch { .. } => None,
                        Self::Unpersist(ref fidl_error) => Some(fidl_error),
                    }
                }
                fn description(&self) -> &str {
                    match self {
                        Self::GettingContentSize(_) => "getting content size",
                        Self::ReadingConfigBytes(_) => "reading VMO contents",
                        Self::TooFewBytes => "VMO contents too small",
                        Self::ChecksumMismatch { .. } => "ABI checksum mismatch",
                        Self::Unpersist(_) => "FIDL parsing error",
                    }
                }
            }
        }
        .to_string();

        assert_eq!(actual_rust_src, expected_rust_src);
    }
}
