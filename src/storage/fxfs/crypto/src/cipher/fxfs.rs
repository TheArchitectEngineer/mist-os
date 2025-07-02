// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use super::{
    CbcDecryptProcessor, CbcEncryptProcessor, Cipher, Tweak, UnwrappedKey, XtsProcessor,
    FSCRYPT_PADDING, SECTOR_SIZE,
};
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes256;
use anyhow::Error;
use std::hash::{Hash, Hasher};
use zerocopy::IntoBytes;

#[derive(Debug)]
pub struct FxfsCipher {
    key: Aes256,
}
impl FxfsCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        Self { key: Aes256::new(GenericArray::from_slice(key)) }
    }
}
impl Cipher for FxfsCipher {
    fn encrypt(
        &self,
        _ino: u64,
        _device_offset: u64,
        file_offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        fxfs_trace::duration!(c"encrypt", "len" => buffer.len());
        assert_eq!(file_offset % SECTOR_SIZE, 0);
        let mut sector_offset = file_offset / SECTOR_SIZE;
        for sector in buffer.chunks_exact_mut(SECTOR_SIZE as usize) {
            let mut tweak = Tweak(sector_offset as u128);
            // The same key is used for encrypting the data and computing the tweak.
            self.key.encrypt_block(GenericArray::from_mut_slice(tweak.as_mut_bytes()));
            self.key.encrypt_with_backend(XtsProcessor::new(tweak, sector));
            sector_offset += 1;
        }
        Ok(())
    }

    fn decrypt(
        &self,
        _ino: u64,
        _device_offset: u64,
        file_offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        fxfs_trace::duration!(c"decrypt", "len" => buffer.len());
        assert_eq!(file_offset % SECTOR_SIZE, 0);
        let mut sector_offset = file_offset / SECTOR_SIZE;
        for sector in buffer.chunks_exact_mut(SECTOR_SIZE as usize) {
            let mut tweak = Tweak(sector_offset as u128);
            // The same key is used for encrypting the data and computing the tweak.
            self.key.encrypt_block(GenericArray::from_mut_slice(tweak.as_mut_bytes()));
            self.key.decrypt_with_backend(XtsProcessor::new(tweak, sector));
            sector_offset += 1;
        }
        Ok(())
    }

    fn encrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        // Pad the buffer such that its length is a multiple of FSCRYPT_PADDING.
        buffer.resize(buffer.len().next_multiple_of(FSCRYPT_PADDING), 0);
        self.key.encrypt_with_backend(CbcEncryptProcessor::new(Tweak(object_id as u128), buffer));
        Ok(())
    }

    fn decrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.key.decrypt_with_backend(CbcDecryptProcessor::new(Tweak(object_id as u128), buffer));
        // Remove the padding
        if let Some(i) = buffer.iter().rposition(|x| *x != 0) {
            let new_len = i + 1;
            buffer.truncate(new_len);
        }
        Ok(())
    }

    fn hash_code(&self, filename: &[u8], casefold: bool) -> u32 {
        if filename.is_empty() {
            return 0;
        }
        let mut hasher = rustc_hash::FxHasher::default();
        if casefold {
            for ch in fxfs_unicode::casefold(filename.iter().map(|x| *x as char)) {
                ch.hash(&mut hasher);
            }
        } else {
            filename.hash(&mut hasher);
        }
        let mut hash = hasher.finish() as u32;
        self.encrypt(0, 0, 0, hash.as_mut_bytes()).unwrap();
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::{FxfsCipher, UnwrappedKey};
    use crate::Cipher;
    use std::sync::Arc;

    /// Output produced via:
    /// echo -n filename > in.txt ; truncate -s 16 in.txt
    /// openssl aes-256-cbc -e -iv 02000000000000000000000000000000 -nosalt -K 1fcdf30b7d191bd95d3161fe08513b864aa15f27f910f1c66eec8cfa93e9893b -in in.txt -out out.txt -nopad
    /// hexdump out.txt -e "16/1 \"%02x\" \"\n\"" -v
    #[test]
    fn test_encrypt_filename() {
        let raw_key_hex = "1fcdf30b7d191bd95d3161fe08513b864aa15f27f910f1c66eec8cfa93e9893b";
        let raw_key_bytes: [u8; 32] =
            hex::decode(raw_key_hex).expect("decode failed").try_into().unwrap();
        let unwrapped_key = UnwrappedKey::new(raw_key_bytes.to_vec());
        let cipher: Arc<dyn Cipher> = Arc::new(FxfsCipher::new(&unwrapped_key));
        let object_id = 2;
        let mut text = "filename".to_string().as_bytes().to_vec();
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, hex::decode("52d56369103a39b3ea1e09c85dd51546").expect("decode failed"));
    }

    /// Output produced via:
    /// openssl aes-256-cbc -d -iv 02000000000000000000000000000000 -nosalt -K 1fcdf30b7d191bd95d3161fe08513b864aa15f27f910f1c66eec8cfa93e9893b -in out.txt -out in.txt
    /// cat in.txt
    #[test]
    fn test_decrypt_filename() {
        let raw_key_hex = "1fcdf30b7d191bd95d3161fe08513b864aa15f27f910f1c66eec8cfa93e9893b";
        let raw_key_bytes: [u8; 32] =
            hex::decode(raw_key_hex).expect("decode failed").try_into().unwrap();
        let unwrapped_key = UnwrappedKey::new(raw_key_bytes.to_vec());
        let cipher: Arc<dyn Cipher> = Arc::new(FxfsCipher::new(&unwrapped_key));
        let object_id = 2;
        let mut text = hex::decode("52d56369103a39b3ea1e09c85dd51546").expect("decode failed");
        cipher.decrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, "filename".to_string().as_bytes().to_vec());
    }
}
