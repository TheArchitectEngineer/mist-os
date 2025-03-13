// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::vfs::{FsStr, FsString};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::mount_flags::MountFlags;
use std::collections::HashMap;
use std::fmt::Display;

/// Parses a comma-separated list of options of the form `key` or `key=value` or `key="value"`.
/// Commas and equals-signs are only permitted in the `key="value"` case. In the case of
/// `key=value1,key=value2` collisions, the last value wins. Returns a hashmap of key/value pairs,
/// or `EINVAL` in the case of malformed input. Note that no escape character sequence is supported,
/// so values may not contain the `"` character.
///
/// # Examples
///
/// `key0=value0,key1,key2=value2,key0=value3` -> `map{"key0":"value3","key1":"","key2":"value2"}`
///
/// `key0=value0,key1="quoted,with=punc:tua-tion."` ->
/// `map{"key0":"value0","key1":"quoted,with=punc:tua-tion."}`
///
/// `key0="mis"quoted,key2=unquoted` -> `EINVAL`
#[derive(Debug, Default, Clone)]
pub struct MountParams {
    options: HashMap<FsString, FsString>,
}

impl MountParams {
    pub fn parse(data: &FsStr) -> Result<Self, Errno> {
        let options = parse_mount_options::parse_mount_options(data).map_err(|_| errno!(EINVAL))?;
        Ok(MountParams { options })
    }

    pub fn get(&self, key: &[u8]) -> Option<&FsString> {
        self.options.get(key)
    }

    pub fn remove(&mut self, key: &[u8]) -> Option<FsString> {
        self.options.remove(key)
    }

    pub fn is_empty(&self) -> bool {
        self.options.is_empty()
    }

    pub fn remove_mount_flags(&mut self) -> MountFlags {
        let mut flags = MountFlags::empty();
        if self.remove(b"ro").is_some() {
            flags |= MountFlags::RDONLY;
        }
        if self.remove(b"nosuid").is_some() {
            flags |= MountFlags::NOSUID;
        }
        if self.remove(b"nodev").is_some() {
            flags |= MountFlags::NODEV;
        }
        if self.remove(b"noexec").is_some() {
            flags |= MountFlags::NOEXEC;
        }
        if self.remove(b"noatime").is_some() {
            flags |= MountFlags::NOATIME;
        }
        if self.remove(b"nodiratime").is_some() {
            flags |= MountFlags::NODIRATIME;
        }
        if self.remove(b"relatime").is_some() {
            flags |= MountFlags::RELATIME;
        }
        if self.remove(b"strictatime").is_some() {
            flags |= MountFlags::STRICTATIME;
        }
        flags
    }
}

impl Display for MountParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", itertools::join(self.options.iter().map(|(k, v)| format!("{k}={v}")), ","))
    }
}

/// Parses `data` slice into another type.
///
/// This relies on str::parse so expects `data` to be utf8.
pub fn parse<F: std::str::FromStr>(data: &FsStr) -> Result<F, Errno>
where
    <F as std::str::FromStr>::Err: std::fmt::Debug,
{
    std::str::from_utf8(data.as_ref())
        .map_err(|e| errno!(EINVAL, e))?
        .parse::<F>()
        .map_err(|e| errno!(EINVAL, format!("{:?}", e)))
}

mod parse_mount_options {
    use crate::vfs::{FsStr, FsString};
    use nom::branch::alt;
    use nom::bytes::complete::{is_not, tag};
    use nom::combinator::opt;
    use nom::multi::separated_list0;
    use nom::sequence::{delimited, separated_pair, terminated};
    use nom::{IResult, Parser};
    use starnix_uapi::errors::{errno, error, Errno};
    use std::collections::HashMap;

    fn unquoted(input: &[u8]) -> IResult<&[u8], &[u8]> {
        is_not(",=").parse(input)
    }

    fn quoted(input: &[u8]) -> IResult<&[u8], &[u8]> {
        delimited(tag("\""), is_not("\""), tag("\"")).parse(input)
    }

    fn value(input: &[u8]) -> IResult<&[u8], &[u8]> {
        alt((quoted, unquoted)).parse(input)
    }

    fn key_value(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8])> {
        separated_pair(unquoted, tag("="), value).parse(input)
    }

    fn key_only(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8])> {
        let (input, key) = unquoted(input)?;
        Ok((input, (key, b"")))
    }

    fn option(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8])> {
        alt((key_value, key_only)).parse(input)
    }

    pub(super) fn parse_mount_options(input: &FsStr) -> Result<HashMap<FsString, FsString>, Errno> {
        let (input, options) = terminated(separated_list0(tag(","), option), opt(tag(",")))
            .parse(input.into())
            .map_err(|_| errno!(EINVAL))?;

        // `[...],last_key="mis"quoted` not allowed.
        if input.len() > 0 {
            return error!(EINVAL);
        }

        // Insert in-order so that last `key=value` containing `key` "wins".
        let mut options_map: HashMap<FsString, FsString> = HashMap::with_capacity(options.len());
        for (key, value) in options.into_iter() {
            options_map.insert(key.into(), value.into());
        }

        Ok(options_map)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, MountParams};
    use crate::vfs::FsString;
    use maplit::hashmap;
    use starnix_uapi::mount_flags::MountFlags;

    #[::fuchsia::test]
    fn empty_data() {
        assert!(MountParams::parse(Default::default()).unwrap().is_empty());
    }

    #[::fuchsia::test]
    fn parse_options_with_trailing_comma() {
        let data = b"key0=value0,";
        let parsed_data =
            MountParams::parse(data.into()).expect("mount options parse:  key0=value0,");
        assert_eq!(
            parsed_data.options,
            hashmap! {
                b"key0".into() => b"value0".into(),
            }
        );
    }

    #[::fuchsia::test]
    fn parse_options_last_value_wins() {
        // Repeat key `key0`.
        let data = b"key0=value0,key1,key2=value2,key0=value3";
        let parsed_data = MountParams::parse(data.into())
            .expect("mount options parse:  key0=value0,key1,key2=value2,key0=value3");
        assert_eq!(
            parsed_data.options,
            hashmap! {
                b"key1".into() => b"".into(),
                b"key2".into() => b"value2".into(),
                // Last `key0` value in list "wins":
                b"key0".into() => b"value3".into(),
            }
        );
    }

    #[::fuchsia::test]
    fn parse_options_quoted() {
        let data = b"key0=unqouted,key1=\"quoted,with=punc:tua-tion.\"";
        let parsed_data = MountParams::parse(data.into())
            .expect("mount options parse:  key0=value0,key1,key2=value2,key0=value3");
        assert_eq!(
            parsed_data.options,
            hashmap! {
                b"key0".into() => b"unqouted".into(),
                b"key1".into() => b"quoted,with=punc:tua-tion.".into(),
            }
        );
    }

    #[::fuchsia::test]
    fn parse_options_misquoted() {
        let data = b"key0=\"mis\"quoted,key1=\"quoted\"";
        let parse_result = MountParams::parse(data.into());
        assert!(
            parse_result.is_err(),
            "expected parse failure:  key0=\"mis\"quoted,key1=\"quoted\""
        );
    }

    #[::fuchsia::test]
    fn parse_options_misquoted_tail() {
        let data = b"key0=\"quoted\",key1=\"mis\"quoted";
        let parse_result = MountParams::parse(data.into());
        assert!(
            parse_result.is_err(),
            "expected parse failure:  key0=\"quoted\",key1=\"mis\"quoted"
        );
    }

    #[::fuchsia::test]
    fn parse_normal_mount_flags() {
        let data = b"nosuid,nodev,noexec,relatime";
        let parsed_data = MountParams::parse(data.into())
            .expect("mount options parse:  nosuid,nodev,noexec,relatime");
        assert_eq!(
            parsed_data.options,
            hashmap! {
                b"nosuid".into() => FsString::default(),
                b"nodev".into() => FsString::default(),
                b"noexec".into() => FsString::default(),
                b"relatime".into() => FsString::default(),
            }
        );
    }

    #[::fuchsia::test]
    fn parse_and_remove_normal_mount_flags() {
        let data = b"nosuid,nodev,noexec,relatime";
        let mut parsed_data = MountParams::parse(data.into())
            .expect("mount options parse:  nosuid,nodev,noexec,relatime");
        let flags = parsed_data.remove_mount_flags();
        assert_eq!(
            flags,
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::RELATIME
        );
    }

    #[::fuchsia::test]
    fn parse_data() {
        assert_eq!(parse::<usize>("42".into()), Ok(42));
    }
}
