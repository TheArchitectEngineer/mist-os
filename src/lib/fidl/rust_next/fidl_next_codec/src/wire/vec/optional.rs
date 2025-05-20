// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::{needs_drop, MaybeUninit};
use core::{fmt, slice};

use munge::munge;

use super::raw::RawWireVector;
use crate::{
    Decode, DecodeError, Decoder, DecoderExt as _, Encodable, EncodableOption, Encode, EncodeError,
    EncodeOption, EncodeOptionRef, EncodeRef, Encoder, EncoderExt as _, FromWire, FromWireOption,
    FromWireOptionRef, FromWireRef, Slot, Wire, WirePointer, WireVector,
};

/// An optional FIDL vector
#[repr(transparent)]
pub struct WireOptionalVector<'de, T> {
    raw: RawWireVector<'de, T>,
}

unsafe impl<T: Wire> Wire for WireOptionalVector<'static, T> {
    type Decoded<'de> = WireOptionalVector<'de, T::Decoded<'de>>;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { raw } = out);
        RawWireVector::<T>::zero_padding(raw);
    }
}

impl<T> Drop for WireOptionalVector<'_, T> {
    fn drop(&mut self) {
        if needs_drop::<T>() && self.is_some() {
            unsafe {
                self.raw.as_slice_ptr().drop_in_place();
            }
        }
    }
}

impl<'de, T> WireOptionalVector<'de, T> {
    /// Encodes that a vector is present in a slot.
    pub fn encode_present(out: &mut MaybeUninit<Self>, len: u64) {
        munge!(let Self { raw } = out);
        RawWireVector::encode_present(raw, len);
    }

    /// Encodes that a vector is absent in a slot.
    pub fn encode_absent(out: &mut MaybeUninit<Self>) {
        munge!(let Self { raw } = out);
        RawWireVector::encode_absent(raw);
    }

    /// Returns whether the vector is present.
    pub fn is_some(&self) -> bool {
        !self.raw.as_ptr().is_null()
    }

    /// Returns whether the vector is absent.
    pub fn is_none(&self) -> bool {
        !self.is_some()
    }

    /// Gets a reference to the vector, if any.
    pub fn as_ref(&self) -> Option<&WireVector<'_, T>> {
        if self.is_some() {
            Some(unsafe { &*(self as *const Self).cast() })
        } else {
            None
        }
    }

    /// Converts the optional wire vector to an `Option<WireVector>`.
    pub fn to_option(self) -> Option<WireVector<'de, T>> {
        if self.is_some() {
            Some(unsafe { core::mem::transmute::<Self, WireVector<'de, T>>(self) })
        } else {
            None
        }
    }

    /// Decodes a wire vector which contains raw data.
    ///
    /// # Safety
    ///
    /// The elements of the wire vector must not need to be individually decoded, and must always be
    /// valid.
    pub unsafe fn decode_raw<D>(
        mut slot: Slot<'_, Self>,
        mut decoder: &mut D,
    ) -> Result<(), DecodeError>
    where
        D: Decoder + ?Sized,
        T: Decode<D>,
    {
        munge!(let Self { raw: RawWireVector { len, mut ptr } } = slot.as_mut());

        if WirePointer::is_encoded_present(ptr.as_mut())? {
            let mut slice = decoder.take_slice_slot::<T>(**len as usize)?;
            WirePointer::set_decoded(ptr, slice.as_mut_ptr().cast());
        } else if *len != 0 {
            return Err(DecodeError::InvalidOptionalSize(**len));
        }

        Ok(())
    }
}

impl<T: fmt::Debug> fmt::Debug for WireOptionalVector<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

unsafe impl<D: Decoder + ?Sized, T: Decode<D>> Decode<D> for WireOptionalVector<'static, T> {
    fn decode(mut slot: Slot<'_, Self>, mut decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { raw: RawWireVector { len, mut ptr } } = slot.as_mut());

        if WirePointer::is_encoded_present(ptr.as_mut())? {
            let mut slice = decoder.take_slice_slot::<T>(**len as usize)?;
            for i in 0..**len as usize {
                T::decode(slice.index(i), decoder)?;
            }
            WirePointer::set_decoded(ptr, slice.as_mut_ptr().cast());
        } else if *len != 0 {
            return Err(DecodeError::InvalidOptionalSize(**len));
        }

        Ok(())
    }
}

#[inline]
fn encode_to_optional_vector<V, E, T>(
    value: Option<V>,
    encoder: &mut E,
    out: &mut MaybeUninit<WireOptionalVector<'_, T::Encoded>>,
) -> Result<(), EncodeError>
where
    V: AsRef<[T]> + IntoIterator,
    V::IntoIter: ExactSizeIterator,
    V::Item: Encode<E, Encoded = T::Encoded>,
    E: Encoder + ?Sized,
    T: Encode<E>,
{
    if let Some(value) = value {
        let len = value.as_ref().len();
        if T::COPY_OPTIMIZATION.is_enabled() {
            let slice = value.as_ref();
            // SAFETY: `T` has copy optimization enabled, which guarantees that it has no uninit
            // bytes and can be copied directly to the output instead of calling `encode`. This
            // means that we may cast `&[T]` to `&[u8]` and write those bytes.
            let bytes = unsafe { slice::from_raw_parts(slice.as_ptr().cast(), size_of_val(slice)) };
            encoder.write(bytes);
        } else {
            encoder.encode_next_iter(value.into_iter())?;
        }
        WireOptionalVector::encode_present(out, len as u64);
    } else {
        WireOptionalVector::encode_absent(out);
    }
    Ok(())
}

impl<T: Encodable> EncodableOption for Vec<T> {
    type EncodedOption = WireOptionalVector<'static, T::Encoded>;
}

unsafe impl<E, T> EncodeOption<E> for Vec<T>
where
    E: Encoder + ?Sized,
    T: Encode<E>,
{
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::EncodedOption>,
    ) -> Result<(), EncodeError> {
        encode_to_optional_vector(this, encoder, out)
    }
}

unsafe impl<E, T> EncodeOptionRef<E> for Vec<T>
where
    E: Encoder + ?Sized,
    T: EncodeRef<E>,
{
    fn encode_option_ref(
        this: Option<&Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::EncodedOption>,
    ) -> Result<(), EncodeError> {
        encode_to_optional_vector(this, encoder, out)
    }
}

impl<T: Encodable> EncodableOption for &[T] {
    type EncodedOption = WireOptionalVector<'static, T::Encoded>;
}

unsafe impl<E, T> EncodeOption<E> for &[T]
where
    E: Encoder + ?Sized,
    T: EncodeRef<E>,
{
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::EncodedOption>,
    ) -> Result<(), EncodeError> {
        encode_to_optional_vector(this, encoder, out)
    }
}

impl<T: FromWire<W>, W> FromWireOption<WireOptionalVector<'_, W>> for Vec<T> {
    fn from_wire_option(wire: WireOptionalVector<'_, W>) -> Option<Self> {
        wire.to_option().map(Vec::from_wire)
    }
}

impl<T: FromWireRef<W>, W> FromWireOptionRef<WireOptionalVector<'_, W>> for Vec<T> {
    fn from_wire_option_ref(wire: &WireOptionalVector<'_, W>) -> Option<Self> {
        wire.as_ref().map(Vec::from_wire_ref)
    }
}
