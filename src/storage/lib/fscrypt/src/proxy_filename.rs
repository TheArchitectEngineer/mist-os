// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{anyhow, ensure, Error};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::Digest;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// A proxy filename is used when we don't have the keys to decrypt the actual filename.
///
/// When working with locked directories, we encode both Dentry and user provided
/// filenames using this struct before comparing them.
///
/// The hash code is encoded directly, allowing an index in some cases, even when entries
/// are encrypted.
///
/// As a work around to keep uniqueness, for filenames longer than 149 bytes, we use the filename
/// prefix and calculate the sha256 of the full encrypted name.  This produces a filename that is
/// under the limit and very likely to remain unique.
///
/// The filename prefix size is chosen here for compatibility.
#[repr(C, packed)]
#[derive(
    Copy, Clone, Debug, Eq, PartialEq, FromBytes, Immutable, KnownLayout, IntoBytes, Unaligned,
)]
pub struct ProxyFilename {
    pub hash_code: u64,
    pub filename: [u8; 149],
    pub sha256: [u8; 32],
    // 'len' holds the length in bytes of the material that gets base64 encoded.
    //
    // The fields above are treated as an array of bytes and directly base64 encoded to give
    // a proxy filename. The length of this encoding varies. It is always at least 8 bytes
    // (covering hash_code) and covers the filename if 149 bytes or shorter, but if the filename
    // exceeds 149 characters then the length will always be the full size of hash_code, filename
    // prefix and sha256.
    // This length is not stored anywhere and cannot be implied from the encrypted filename as
    // it may contain NULL characters so we tack it onto the end here. It is not serialized.
    len: usize,
}

/// The maximum length of ProxyFilename before being base64 encoded.
const PROXY_FILENAME_MAX_SIZE: usize = 8 + 149 + 32;

impl ProxyFilename {
    pub fn new(hash_code: u64, raw_filename: &[u8]) -> Self {
        let mut filename = [0u8; 149];
        let mut sha256 = [0u8; 32];
        let len = if raw_filename.len() <= filename.len() {
            filename[..raw_filename.len()].copy_from_slice(raw_filename);
            std::mem::size_of::<u64>() + raw_filename.len()
        } else {
            let len = filename.len();
            filename.copy_from_slice(&raw_filename[..len]);
            sha256 = sha2::Sha256::digest(&raw_filename[len..]).into();
            PROXY_FILENAME_MAX_SIZE
        };
        Self { hash_code, filename, sha256, len }
    }
}

impl Into<String> for ProxyFilename {
    fn into(self) -> String {
        URL_SAFE_NO_PAD.encode(&self.as_bytes()[..self.len])
    }
}

impl TryFrom<&str> for ProxyFilename {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let mut bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| anyhow!("Invalid proxy filename"))?;
        ensure!(
            (bytes.len() >= 8 && bytes.len() <= 8 + 149) || bytes.len() == PROXY_FILENAME_MAX_SIZE,
            "Invalid proxy filename length {}",
            bytes.len()
        );
        let len = bytes.len();
        bytes.resize(std::mem::size_of::<ProxyFilename>(), 0);
        let mut instance = Self::read_from_bytes(&bytes).unwrap();
        instance.len = len;
        Ok(instance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The maximum length of ProxyFilename encoded with base64.
    const PROXY_FILENAME_MAX_ENCODED_SIZE: usize = (PROXY_FILENAME_MAX_SIZE * 4).div_ceil(3);

    #[test]
    fn test_proxy_filename() {
        // Invalid filename works when encoded.
        let a = ProxyFilename::new(1, b"fo!$obar");
        let encoded: String = a.into();
        let b: ProxyFilename = encoded.as_str().try_into().unwrap();
        assert_eq!(a, b);

        // Short filename.
        let a = ProxyFilename::new(1, b"foobar");
        let encoded: String = a.into();
        let b: ProxyFilename = encoded.as_str().try_into().unwrap();
        assert_eq!(a, b);

        // 149 length filename.
        let a = ProxyFilename::new(1, &[0xff; 149]);
        let encoded: String = a.into();
        let b: ProxyFilename = encoded.as_str().try_into().unwrap();
        assert_eq!(a, b);
        // Account for base64 bloat (Length growth by 4/3rd, rounded up)
        assert_eq!(encoded.len(), ((8 + 149) * 4usize).div_ceil(3));

        // 150 length filename -- now has sha256 suffix.
        // Note the filename is all zeros. This should not affect the output.
        let a = ProxyFilename::new(1, &[0; 150]);
        let encoded: String = a.into();
        let b: ProxyFilename = encoded.as_str().try_into().unwrap();
        assert_eq!(a, b);
        assert_eq!(encoded.len(), PROXY_FILENAME_MAX_ENCODED_SIZE);

        // 255 length filename
        let a = ProxyFilename::new(1, &[b'a'; 255]);
        let encoded: String = a.into();
        let b: ProxyFilename = encoded.as_str().try_into().unwrap();
        assert_eq!(a, b);
        assert_eq!(encoded.len(), PROXY_FILENAME_MAX_ENCODED_SIZE);

        // Decoding of bad base64 strings.
        assert!(ProxyFilename::try_from("$$dda123=").is_err());

        assert_eq!(
            URL_SAFE_NO_PAD.encode(&[0; PROXY_FILENAME_MAX_SIZE]).len(),
            PROXY_FILENAME_MAX_ENCODED_SIZE
        );

        // Decoding of bad lengths.
        // Valid lengths include the 8 byte hash_code and a filename up to 149 characters.
        // If the filename exceeds that, the length should be exactly PROXY_FILENAME_MAX_SIZE.
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 0]).as_str()).is_err());
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 7]).as_str()).is_err());
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 8]).as_str()).is_ok());
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 9]).as_str()).is_ok());
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 8 + 148]).as_str()).is_ok());
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 8 + 149]).as_str()).is_ok());
        assert!(ProxyFilename::try_from(URL_SAFE_NO_PAD.encode(&[b'a'; 8 + 150]).as_str()).is_err());
        assert!(ProxyFilename::try_from(
            URL_SAFE_NO_PAD.encode(&[b'a'; PROXY_FILENAME_MAX_SIZE - 1]).as_str()
        )
        .is_err());
        assert!(ProxyFilename::try_from(
            URL_SAFE_NO_PAD.encode(&[b'a'; PROXY_FILENAME_MAX_SIZE]).as_str()
        )
        .is_ok());
        assert!(ProxyFilename::try_from(
            URL_SAFE_NO_PAD.encode(&[b'a'; PROXY_FILENAME_MAX_SIZE + 1]).as_str()
        )
        .is_err());
    }
}
