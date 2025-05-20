// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::marker::PhantomData;
use core::mem::MaybeUninit;

use fidl_next_codec::{
    munge, Chunk, Decode, DecodeError, Decoder, Encodable, Encode, EncodeError, EncodeRef, Encoder,
    FromWire, FromWireRef, RawWireUnion, Slot, Wire,
};

use crate::{FrameworkError, WireFrameworkError};

/// A flexible FIDL response.
#[derive(Debug)]
pub enum Flexible<T> {
    /// The value of the flexible call when successful.
    Ok(T),
    /// The error indicating that the flexible call failed.
    FrameworkErr(FrameworkError),
}

impl<T> Flexible<T> {
    /// Converts from `&Flexible<T>` to `Flexible<&T>`.
    pub fn as_ref(&self) -> Flexible<&T> {
        match self {
            Self::Ok(value) => Flexible::Ok(value),
            Self::FrameworkErr(framework_error) => Flexible::FrameworkErr(*framework_error),
        }
    }
}

/// A flexible FIDL response.
#[repr(transparent)]
pub struct WireFlexible<'de, T> {
    raw: RawWireUnion,
    _phantom: PhantomData<(&'de mut [Chunk], T)>,
}

unsafe impl<T: Wire> Wire for WireFlexible<'static, T> {
    type Decoded<'de> = WireFlexible<'de, T::Decoded<'de>>;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { raw, _phantom: _ } = out);
        RawWireUnion::zero_padding(raw);
    }
}

const ORD_OK: u64 = 1;
const ORD_FRAMEWORK_ERR: u64 = 3;

impl<T> WireFlexible<'_, T> {
    /// Returns whether the flexible response is `Ok`.
    pub fn is_ok(&self) -> bool {
        self.raw.ordinal() == ORD_OK
    }

    /// Returns whether the flexible response is `FrameworkErr`.
    pub fn is_framework_err(&self) -> bool {
        self.raw.ordinal() == ORD_FRAMEWORK_ERR
    }

    /// Returns the `Ok` value of the response, if any.
    pub fn ok(&self) -> Option<&T> {
        self.is_ok().then(|| unsafe { self.raw.get().deref_unchecked() })
    }

    /// Returns the `FrameworkErr` value of the response, if any.
    pub fn framework_err(&self) -> Option<FrameworkError> {
        self.is_framework_err()
            .then(|| unsafe { (*self.raw.get().deref_unchecked::<WireFrameworkError>()).into() })
    }

    /// Returns the contained `Ok` value.
    ///
    /// Panics if the response was not `Ok`.
    pub fn unwrap(&self) -> &T {
        self.ok().unwrap()
    }

    /// Returns the contained `FrameworkErr` value.
    ///
    /// Panics if the response was not `FrameworkErr`.
    pub fn unwrap_framework_err(&self) -> FrameworkError {
        self.framework_err().unwrap()
    }

    /// Returns a `Flexible` of a reference to the value or framework error.
    pub fn as_ref(&self) -> Flexible<&T> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { Flexible::Ok(self.raw.get().deref_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                Flexible::FrameworkErr(
                    (*self.raw.get().deref_unchecked::<WireFrameworkError>()).into(),
                )
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a `Result` of the `Ok` value and a potential `FrameworkError`.
    pub fn as_result(&self) -> Result<&T, FrameworkError> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { Ok(self.raw.get().deref_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                Err((*self.raw.get().deref_unchecked::<WireFrameworkError>()).into())
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a `Flexible` of an `Owned` value or framework error.
    pub fn to_flexible(self) -> Flexible<T> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { Flexible::Ok(self.raw.get().read_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                Flexible::FrameworkErr(self.raw.get().read_unchecked::<WireFrameworkError>().into())
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }
}

impl<T> fmt::Debug for WireFlexible<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

unsafe impl<D, T> Decode<D> for WireFlexible<'static, T>
where
    D: Decoder + ?Sized,
    T: Decode<D>,
{
    fn decode(slot: Slot<'_, Self>, decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { mut raw, _phantom: _ } = slot);

        match RawWireUnion::encoded_ordinal(raw.as_mut()) {
            ORD_OK => RawWireUnion::decode_as::<D, T>(raw, decoder)?,
            ORD_FRAMEWORK_ERR => RawWireUnion::decode_as::<D, WireFrameworkError>(raw, decoder)?,
            ord => return Err(DecodeError::InvalidUnionOrdinal(ord as usize)),
        }

        Ok(())
    }
}

impl<T> Encodable for Flexible<T>
where
    T: Encodable,
{
    type Encoded = WireFlexible<'static, T::Encoded>;
}

unsafe impl<E, T> Encode<E> for Flexible<T>
where
    E: Encoder + ?Sized,
    T: Encode<E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        munge!(let WireFlexible { raw, _phantom: _ } = out);

        match self {
            Self::Ok(value) => RawWireUnion::encode_as::<E, T>(value, ORD_OK, encoder, raw)?,
            Self::FrameworkErr(error) => RawWireUnion::encode_as::<E, FrameworkError>(
                error,
                ORD_FRAMEWORK_ERR,
                encoder,
                raw,
            )?,
        }

        Ok(())
    }
}

unsafe impl<E, T> EncodeRef<E> for Flexible<T>
where
    E: Encoder + ?Sized,
    T: EncodeRef<E>,
{
    fn encode_ref(
        &self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        self.as_ref().encode(encoder, out)
    }
}

impl<T, WT> FromWire<WireFlexible<'_, WT>> for Flexible<T>
where
    T: FromWire<WT>,
{
    fn from_wire(wire: WireFlexible<'_, WT>) -> Self {
        match wire.to_flexible() {
            Flexible::Ok(value) => Self::Ok(T::from_wire(value)),
            Flexible::FrameworkErr(framework_error) => Self::FrameworkErr(framework_error),
        }
    }
}

impl<T, WT> FromWireRef<WireFlexible<'_, WT>> for Flexible<T>
where
    T: FromWireRef<WT>,
{
    fn from_wire_ref(wire: &WireFlexible<'_, WT>) -> Self {
        match wire.as_ref() {
            Flexible::Ok(value) => Self::Ok(T::from_wire_ref(value)),
            Flexible::FrameworkErr(framework_error) => Self::FrameworkErr(framework_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use fidl_next_codec::chunks;

    use super::{Flexible, WireFlexible};
    use crate::testing::{assert_decoded, assert_encoded};
    use crate::FrameworkError;

    #[test]
    fn encode_flexible_result() {
        assert_encoded(
            Flexible::<()>::Ok(()),
            &chunks![
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
        assert_encoded(
            Flexible::<()>::FrameworkErr(FrameworkError::UnknownMethod),
            &chunks![
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
    }

    #[test]
    fn decode_flexible_result() {
        assert_decoded::<WireFlexible<'_, ()>>(
            &mut chunks![
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x01, 0x00,
            ],
            |x| assert!(matches!(x.as_ref(), Flexible::Ok(()))),
        );
        assert_decoded::<WireFlexible<'_, ()>>(
            &mut chunks![
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
                0x01, 0x00,
            ],
            |x| {
                assert!(matches!(x.as_ref(), Flexible::FrameworkErr(FrameworkError::UnknownMethod)))
            },
        );
    }
}
