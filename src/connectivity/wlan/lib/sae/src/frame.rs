// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{AuthFrameRx, AuthFrameTx};
use anyhow::{anyhow, bail, Error};
use fidl_fuchsia_wlan_ieee80211::StatusCode;
use wlan_common::append::Append;
use wlan_common::buffer_reader::BufferReader;

/// IEEE Std 802.11-2016, 12.4.6
/// An anticlogging token sent to a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntiCloggingTokenMsg<'a> {
    pub group_id: u16,
    pub anti_clogging_token: &'a [u8],
}

/// IEEE Std 802.11-2016, 12.4.7.4
/// An SAE Commit message received or sent to a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMsg<'a> {
    pub group_id: u16,
    pub anti_clogging_token: Option<&'a [u8]>,
    pub scalar: &'a [u8],
    pub element: &'a [u8],
}

/// IEEE Std 802.11-2016, 12.4.7.5
/// An SAE Confirm message received or sent to a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmMsg<'a> {
    pub send_confirm: u16,
    pub confirm: &'a [u8],
}

#[derive(Debug)]
pub enum ParseSuccess<'a> {
    Commit(CommitMsg<'a>),
    Confirm(ConfirmMsg<'a>),
    AntiCloggingToken(AntiCloggingTokenMsg<'a>),
}

fn get_scalar_and_element_len_bytes(group_id: u16) -> Result<(usize, usize), Error> {
    match group_id {
        19 => Ok((32, 64)),
        _ => bail!("Unsupported SAE group ID: {}", group_id),
    }
}

pub fn parse<'a>(frame: &'a AuthFrameRx<'_>) -> Result<ParseSuccess<'a>, Error> {
    // IEEE 802.11 9.3.3.12 Table 9-36 specifies all SAE auth frame formats.
    match (frame.seq, frame.status_code) {
        (1, StatusCode::Success) => parse_commit(frame.body).map(ParseSuccess::Commit),
        (1, StatusCode::AntiCloggingTokenRequired) => {
            parse_anti_clogging_token(frame.body).map(ParseSuccess::AntiCloggingToken)
        }
        (2, StatusCode::Success) => parse_confirm(frame.body).map(ParseSuccess::Confirm),
        _ => bail!("Could not parse received SAE frame"),
    }
}

fn parse_anti_clogging_token(body: &[u8]) -> Result<AntiCloggingTokenMsg<'_>, Error> {
    let mut reader = BufferReader::new(body);
    let group_id = reader.read_value::<u16>().ok_or_else(|| anyhow!("Failed to read group ID"))?;
    if reader.bytes_remaining() == 0 {
        bail!("Commit indicated AntiCloggingTokenRequired, but no token provided");
    }
    let anti_clogging_token = reader.into_remaining();
    Ok(AntiCloggingTokenMsg { group_id, anti_clogging_token })
}

fn parse_commit(body: &[u8]) -> Result<CommitMsg<'_>, Error> {
    let mut reader = BufferReader::new(body);
    let group_id = reader.read_value::<u16>().ok_or_else(|| anyhow!("Failed to read group ID"))?;

    let (scalar_size, element_size) = get_scalar_and_element_len_bytes(group_id)?;
    let bytes_remaining = reader.bytes_remaining();
    let anti_clogging_token = match bytes_remaining.cmp(&(scalar_size + element_size)) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(
            reader
                .read_bytes(bytes_remaining - scalar_size - element_size)
                .ok_or_else(|| anyhow!("Unexpected buffer end"))?,
        ),
        std::cmp::Ordering::Less => bail!("Buffer truncated"),
    };

    let scalar = reader.read_bytes(scalar_size).ok_or_else(|| anyhow!("Unexpected buffer end"))?;
    let element =
        reader.read_bytes(element_size).ok_or_else(|| anyhow!("Unexpected buffer end"))?;

    Ok(CommitMsg { group_id, scalar, element, anti_clogging_token })
}

const CONFIRM_BYTES: usize = 32;

fn parse_confirm(body: &[u8]) -> Result<ConfirmMsg<'_>, Error> {
    let mut reader = BufferReader::new(body);
    let send_confirm =
        reader.read_value::<u16>().ok_or_else(|| anyhow!("Failed to read send confirm"))?;
    let confirm = reader.read_bytes(CONFIRM_BYTES).ok_or_else(|| anyhow!("Buffer truncated"))?;
    match reader.bytes_remaining() {
        0 => Ok(ConfirmMsg { send_confirm, confirm }),
        _ => bail!("Buffer too long"),
    }
}

// Allow skipping checks on append_bytes() and append_value()
#[allow(unused_must_use)]
pub fn write_commit(
    group_id: u16,
    scalar: &[u8],
    element: &[u8],
    anti_clogging_token: &[u8],
) -> AuthFrameTx {
    let mut body = Vec::with_capacity(2 + scalar.len() + element.len() + anti_clogging_token.len());
    body.append_value(&group_id);
    body.append_bytes(anti_clogging_token);
    body.append_bytes(scalar);
    body.append_bytes(element);
    AuthFrameTx { seq: 1, status_code: StatusCode::Success, body }
}

// Allow skipping checks on append_bytes() and append_value()
#[allow(unused_must_use)]
// This function is currently unused, but planned for future use
#[allow(dead_code)]
pub fn write_token(group_id: u16, token: &[u8]) -> AuthFrameTx {
    let mut body = Vec::with_capacity(2 + token.len());
    body.append_value(&group_id);
    body.append_bytes(token);
    AuthFrameTx { seq: 1, status_code: StatusCode::AntiCloggingTokenRequired, body }
}

// Allow skipping checks on append_bytes() and append_value()
#[allow(unused_must_use)]
pub fn write_confirm(send_confirm: u16, confirm: &[u8]) -> AuthFrameTx {
    let mut body = Vec::with_capacity(2 + confirm.len());
    body.append_value(&send_confirm);
    body.append_bytes(confirm);
    AuthFrameTx { seq: 2, status_code: StatusCode::Success, body }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wlan_common::assert_variant;

    #[rustfmt::skip]
    const ECC_COMMIT_BODY: &[u8] = &[
        // group id
        19, 00,
        // scalar [0x1; 32]
        1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
        // element [0x2; 64]
        2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
        2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    ];

    #[rustfmt::skip]
    const ECC_COMMIT_BODY_WITH_ANTI_CLOGGING_TOKEN: &[u8] = &[
        // group id
        19, 00,
        // anti-clogging token
        4, 4, 4, 4, 4, 4, 4, 4,
        // scalar [0x1; 32]
        1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
        // element [0x2; 64]
        2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
        2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    ];

    #[rustfmt::skip]
    const ECC_CONFIRM_BODY: &[u8] = &[
        // send-confirm
        0x01, 0x00,
        // confirm [0x3; 32]
        3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,
    ];

    #[rustfmt::skip]
    const ECC_ACT_REQUIRED_BODY: &[u8] = &[
        // group id
        19, 00,
        // anti-clogging token [0x4; 8]
        4,4,4,4,4,4,4,4
    ];

    #[test]
    fn test_parse_commit() {
        let commit_msg =
            AuthFrameRx { seq: 1, status_code: StatusCode::Success, body: ECC_COMMIT_BODY };
        let parse_result = parse(&commit_msg);
        let commit = assert_variant!(parse_result, Ok(ParseSuccess::Commit(commit)) => commit);
        assert_eq!(commit.group_id, 19);
        assert_eq!(commit.scalar, &[1u8; 32][..]);
        assert_eq!(commit.element, &[2u8; 64][..]);
        assert!(commit.anti_clogging_token.is_none());
    }

    #[test]
    fn commit_with_anti_clogging_token() {
        let commit_msg = AuthFrameRx {
            seq: 1,
            status_code: StatusCode::Success,
            body: ECC_COMMIT_BODY_WITH_ANTI_CLOGGING_TOKEN,
        };
        let parse_result = parse(&commit_msg);
        let commit = assert_variant!(parse_result, Ok(ParseSuccess::Commit(commit)) => commit);
        assert_eq!(commit.group_id, 19);
        let anti_clogging_token = assert_variant!(commit.anti_clogging_token, Some(token) => token);
        assert_eq!(anti_clogging_token, &[0x4; 8]);
        assert_eq!(commit.scalar, &[1u8; 32][..]);
        assert_eq!(commit.element, &[2u8; 64][..]);
    }

    #[test]
    fn unknown_group_id_commit() {
        let mut body = ECC_COMMIT_BODY.to_vec();
        body[0] = 0xff; // not a real group
        let commit_msg = AuthFrameRx { seq: 1, status_code: StatusCode::Success, body: &body[..] };
        assert_variant!(parse(&commit_msg), Err(e) => {
            assert!(format!("{:?}", e).contains("Unsupported SAE group ID: 255"))
        });
    }

    #[test]
    fn truncated_commit() {
        let commit_msg =
            AuthFrameRx { seq: 1, status_code: StatusCode::Success, body: &ECC_COMMIT_BODY[..20] };
        assert_variant!(parse(&commit_msg), Err(e) => {
            assert!(format!("{:?}", e).contains("Buffer truncated"))
        });

        let commit_msg = AuthFrameRx { seq: 1, status_code: StatusCode::Success, body: &[] };
        assert_variant!(parse(&commit_msg), Err(e) => {
            assert!(format!("{:?}", e).contains("Failed to read group ID"))
        });
    }

    #[test]
    fn test_parse_confirm() {
        let confirm_msg =
            AuthFrameRx { seq: 2, status_code: StatusCode::Success, body: ECC_CONFIRM_BODY };
        let parse_result = parse(&confirm_msg);
        let confirm = assert_variant!(parse_result, Ok(ParseSuccess::Confirm(confirm)) => confirm);
        assert_eq!(confirm.send_confirm, 1);
        assert_eq!(confirm.confirm, &[3u8; 32][..]);
    }

    #[test]
    fn truncated_confirm() {
        let confirm_msg =
            AuthFrameRx { seq: 2, status_code: StatusCode::Success, body: &ECC_CONFIRM_BODY[..20] };
        assert_variant!(parse(&confirm_msg), Err(e) => {
            assert!(format!("{:?}", e).contains("Buffer truncated"))
        });

        let confirm_msg = AuthFrameRx { seq: 2, status_code: StatusCode::Success, body: &[] };
        assert_variant!(parse(&confirm_msg), Err(e) => {
            assert!(format!("{:?}", e).contains("Failed to read send confirm"))
        });
    }

    #[test]
    fn padded_confirm() {
        let mut body = ECC_CONFIRM_BODY.to_vec();
        body.push(0xff);
        let confirm_msg = AuthFrameRx { seq: 2, status_code: StatusCode::Success, body: &body[..] };
        assert_variant!(parse(&confirm_msg), Err(e) => {
            assert!(format!("{:?}", e).contains("Buffer too long"))
        });
    }

    #[test]
    fn test_parse_anti_clogging_token_required() {
        let act_required = AuthFrameRx {
            seq: 1,
            status_code: StatusCode::AntiCloggingTokenRequired,
            body: ECC_ACT_REQUIRED_BODY,
        };
        let parse_result = parse(&act_required);
        let act = assert_variant!(parse_result, Ok(ParseSuccess::AntiCloggingToken(act)) => act);
        assert_eq!(act.group_id, 19);
        assert_eq!(act.anti_clogging_token, &[0x4; 8][..]);
    }

    #[test]
    fn truncated_anti_clogging_token() {
        let act_required = AuthFrameRx {
            seq: 1,
            status_code: StatusCode::AntiCloggingTokenRequired,
            body: &[19, 00],
        };
        assert_variant!(parse(&act_required), Err(e) => {
            assert!(format!("{:?}", e).contains("no token provided"))
        });

        let act_required =
            AuthFrameRx { seq: 1, status_code: StatusCode::AntiCloggingTokenRequired, body: &[19] };
        assert_variant!(parse(&act_required), Err(e) => {
            assert!(format!("{:?}", e).contains("Failed to read group ID"))
        });

        let act_required =
            AuthFrameRx { seq: 1, status_code: StatusCode::AntiCloggingTokenRequired, body: &[] };
        assert_variant!(parse(&act_required), Err(e) => {
            assert!(format!("{:?}", e).contains("Failed to read group ID"))
        });
    }

    #[test]
    fn test_write_commit() {
        let auth_frame = write_commit(19, &[1u8; 32], &[2u8; 64], &[]);
        assert_eq!(auth_frame.seq, 1);
        assert_eq!(auth_frame.status_code, StatusCode::Success);
        assert_eq!(&auth_frame.body[..], ECC_COMMIT_BODY);
    }

    #[test]
    fn test_write_commit_with_anti_clogging_token() {
        let auth_frame = write_commit(19, &[1u8; 32], &[2u8; 64], &[4u8; 8]);
        assert_eq!(auth_frame.seq, 1);
        assert_eq!(auth_frame.status_code, StatusCode::Success);
        let mut expected_body = ECC_COMMIT_BODY.to_vec();
        expected_body.append(&mut vec![4u8; 8]);
        assert_eq!(&auth_frame.body[..], ECC_COMMIT_BODY_WITH_ANTI_CLOGGING_TOKEN);
    }

    #[test]
    fn test_write_confirm() {
        let auth_frame = write_confirm(1, &[3u8; 32]);
        assert_eq!(auth_frame.seq, 2);
        assert_eq!(auth_frame.status_code, StatusCode::Success);
        assert_eq!(&auth_frame.body[..], ECC_CONFIRM_BODY);
    }

    #[test]
    fn test_write_anticlogging_token() {
        let auth_frame = write_token(19, &[4u8; 8]);
        assert_eq!(auth_frame.seq, 1);
        assert_eq!(auth_frame.status_code, StatusCode::AntiCloggingTokenRequired);
        assert_eq!(&auth_frame.body[..], ECC_ACT_REQUIRED_BODY);
    }
}
