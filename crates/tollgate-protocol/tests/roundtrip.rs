//! Every message type survives an encode/decode round trip, and the framing
//! reassembles messages regardless of how the byte stream was chopped up.

use tollgate_protocol::*;

fn pubkey(seed: u8) -> PubKey {
    let mut b = [seed; 33];
    b[0] = 0x02;
    PubKey(b)
}

fn channel(seed: u8) -> ChannelId {
    ChannelId([seed; 32])
}

fn signature(seed: u8) -> Signature {
    Signature([seed; 64])
}

/// One of each, so the round-trip test covers the whole message set.
fn all_messages() -> Vec<Message> {
    vec![
        Message::Announce(Announce {
            version: PROTOCOL_VERSION,
            pubkey: pubkey(1),
            unit: "byte".into(),
            capabilities: 0,
        }),
        Message::Offer(Offer {
            accepted_mints: vec![
                "https://upstream.example/mint".into(),
                "https://hub.example/mint".into(),
            ],
            unit: "byte".into(),
            min_window_ms: 200,
            max_window_ms: 30_000,
            received_multiplier: 2,
        }),
        Message::Accept(Accept {
            funding: vec![0xAA; 64],
        }),
        Message::ChannelReady(ChannelReady {
            channel_id: channel(3),
        }),
        Message::TopUp(TopUp {
            updates: vec![ChannelUpdate {
                channel_id: channel(3),
                cumulative: 5_242_880,
                signature: signature(4),
            }],
            window_ms: 5_000,
        }),
        // A purchase spanning a channel that is filling up and its replacement.
        Message::TopUp(TopUp {
            updates: vec![
                ChannelUpdate {
                    channel_id: channel(3),
                    cumulative: 8_000_000,
                    signature: signature(4),
                },
                ChannelUpdate {
                    channel_id: channel(5),
                    cumulative: 1_500_000,
                    signature: signature(7),
                },
            ],
            window_ms: 5_000,
        }),
        Message::TopUpReject(TopUpReject {
            refused: vec![RefusedUpdate {
                channel_id: channel(3),
                cumulative: 9_000_000,
            }],
            max_rate_available: 1_000_000,
            reason: ReasonCode::RateExceedsCapacity,
        }),
        Message::RolloverInit(RolloverInit {
            old_channel_id: channel(3),
            funding: vec![0xBB; 96],
        }),
        Message::RolloverReady(RolloverReady {
            old_channel_id: channel(3),
            new_channel_id: channel(5),
        }),
        Message::ChannelClose(ChannelClose {
            channel_id: channel(3),
            final_balance: 4_096,
            final_signature: signature(6),
            reason: CloseReason::Normal,
        }),
        Message::CloseAck(CloseAck {
            channel_id: channel(3),
            accepted_balance: 4_096,
        }),
        Message::Reject(Reject {
            rejected_type: MsgType::TopUp as u8,
            reason: ReasonCode::GrantInvalid,
            text: Some("cumulative did not increase".into()),
        }),
        Message::Disconnect(Disconnect {
            reason: ReasonCode::Other,
        }),
    ]
}

#[test]
fn every_message_round_trips() {
    for msg in all_messages() {
        let mut buf = Vec::new();
        encode(&msg, &mut buf).expect("encode");
        let back = decode(&buf).unwrap_or_else(|e| panic!("decode {:?}: {e}", msg.msg_type()));
        assert_eq!(msg, back, "round trip changed {:?}", msg.msg_type());
    }
}

#[test]
fn reject_with_no_text_round_trips() {
    // The null branch of field 3 is a separate path from the string branch.
    let msg = Message::Reject(Reject {
        rejected_type: MsgType::Offer as u8,
        reason: ReasonCode::MintNotAccepted,
        text: None,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf).expect("decode"), msg);
}

#[test]
fn messages_stay_within_their_size_estimates() {
    // The design doc quotes per-message sizes; a large regression here means
    // the encoding drifted from what constrained devices were promised.
    let announce = Message::Announce(Announce {
        version: PROTOCOL_VERSION,
        pubkey: pubkey(1),
        unit: "byte".into(),
        capabilities: 0,
    });
    let mut buf = Vec::new();
    encode(&announce, &mut buf).expect("encode");
    assert!(buf.len() <= 60, "announce grew to {} bytes", buf.len());

    let topup = Message::TopUp(TopUp {
        updates: vec![ChannelUpdate {
            channel_id: channel(3),
            cumulative: u64::MAX,
            signature: signature(4),
        }],
        window_ms: 5_000,
    });
    buf.clear();
    encode(&topup, &mut buf).expect("encode");
    assert!(buf.len() <= 140, "topup grew to {} bytes", buf.len());
}

#[test]
fn frame_reader_reassembles_across_arbitrary_chunk_boundaries() {
    let msgs = all_messages();
    let mut stream = Vec::new();
    for m in &msgs {
        encode_frame(m, &mut stream).expect("encode_frame");
    }

    // One byte at a time is the worst case a TCP reader can hand us.
    let mut reader = FrameReader::new();
    let mut out = Vec::new();
    for byte in &stream {
        reader.push(&[*byte]);
        while let Some(m) = reader.next_message() {
            out.push(m.expect("decode"));
        }
    }

    assert_eq!(out, msgs);
    assert_eq!(
        reader.pending(),
        0,
        "reader held bytes after the last frame"
    );
}

#[test]
fn frame_reader_yields_several_messages_from_one_push() {
    let msgs = all_messages();
    let mut stream = Vec::new();
    for m in &msgs {
        encode_frame(m, &mut stream).expect("encode_frame");
    }

    let mut reader = FrameReader::new();
    reader.push(&stream);
    let mut out = Vec::new();
    while let Some(m) = reader.next_message() {
        out.push(m.expect("decode"));
    }
    assert_eq!(out, msgs);
}

#[test]
fn a_corrupt_frame_does_not_desync_the_ones_behind_it() {
    let mut stream = Vec::new();
    // A frame whose body is not valid CBOR at all.
    stream.extend_from_slice(&3u16.to_le_bytes());
    stream.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    let good = Message::Disconnect(Disconnect {
        reason: ReasonCode::Other,
    });
    encode_frame(&good, &mut stream).expect("encode_frame");

    let mut reader = FrameReader::new();
    reader.push(&stream);
    assert!(reader.next_message().expect("a frame").is_err());
    assert_eq!(
        reader.next_message().expect("a frame").expect("decode"),
        good
    );
}

#[test]
fn an_offer_with_no_mints_is_malformed() {
    // "A node that will take no payment has nothing to offer" — the encoder
    // will happily write it, so the decoder is where this has to be caught.
    let msg = Message::Offer(Offer {
        accepted_mints: vec![],
        unit: "byte".into(),
        min_window_ms: 200,
        max_window_ms: 30_000,
        received_multiplier: 0,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::EmptyMintList));
}

#[test]
fn unknown_fields_are_skipped_rather_than_rejected() {
    // A later minor version adding key 9 must not break a v1 peer. Built by
    // hand because our own encoder never emits an unknown key.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(3).unwrap();
    e.u8(0).unwrap().u8(MsgType::Disconnect as u8).unwrap();
    e.u8(1)
        .unwrap()
        .u8(ReasonCode::MintNotAccepted as u8)
        .unwrap();
    e.u8(9).unwrap().str("from the future").unwrap();

    assert_eq!(
        decode(&buf).expect("decode"),
        Message::Disconnect(Disconnect {
            reason: ReasonCode::MintNotAccepted
        })
    );
}

#[test]
fn the_type_field_may_arrive_last() {
    // Field order is not guaranteed across implementations, so the decoder
    // scans for key 0 rather than assuming it comes first.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(2).unwrap();
    e.u8(1)
        .unwrap()
        .u8(ReasonCode::VersionUnsupported as u8)
        .unwrap();
    e.u8(0).unwrap().u8(MsgType::Disconnect as u8).unwrap();

    assert_eq!(
        decode(&buf).expect("decode"),
        Message::Disconnect(Disconnect {
            reason: ReasonCode::VersionUnsupported
        })
    );
}

#[test]
fn a_wrong_length_channel_id_is_rejected() {
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(2).unwrap();
    e.u8(0).unwrap().u8(MsgType::ChannelReady as u8).unwrap();
    e.u8(1).unwrap().bytes(&[0u8; 16]).unwrap();

    assert_eq!(
        decode(&buf),
        Err(Error::BadLength {
            key: 1,
            expected: 32,
            got: 16
        })
    );
}

#[test]
fn only_a_rate_refusal_is_unavoidable_from_the_offer() {
    // The Offer advertises mints, unit and window bounds, so a peer that read it
    // has no excuse for tripping any of those. It carries no rate ceiling —
    // there is no honest static number, since what is available depends on what
    // is committed to every other peer — so being refused on rate is how a payer
    // discovers the limit rather than a fault.
    assert!(!ReasonCode::RateExceedsCapacity.avoidable_from_offer());

    for reason in [
        ReasonCode::MultiplierUnacceptable,
        ReasonCode::MintNotAccepted,
        ReasonCode::UnitNotAccepted,
        ReasonCode::WindowOutOfRange,
        ReasonCode::FundingInvalid,
        ReasonCode::GrantInvalid,
        ReasonCode::GrantExceedsChannel,
        ReasonCode::VersionUnsupported,
        ReasonCode::Other,
    ] {
        assert!(
            reason.avoidable_from_offer(),
            "{reason:?} should be worth an operator's attention"
        );
    }
}

#[test]
fn a_topup_with_no_updates_is_malformed() {
    // A purchase that ratchets nothing buys nothing.
    let msg = Message::TopUp(TopUp {
        updates: vec![],
        window_ms: 1_000,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::NoChannelUpdates));
}

#[test]
fn a_topup_with_too_many_updates_is_refused_before_allocating() {
    // Each update is a signature verification and `min_window_ms` only bounds
    // how often a TopUp arrives, so an unbounded array would multiply straight
    // through that budget.
    let over = MAX_CHANNEL_UPDATES + 1;
    let msg = Message::TopUp(TopUp {
        updates: (0..over)
            .map(|i| ChannelUpdate {
                channel_id: channel(i as u8),
                cumulative: 1_000 + i as u64,
                signature: signature(1),
            })
            .collect(),
        window_ms: 1_000,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::TooManyChannelUpdates(over)));
}

#[test]
fn a_full_length_topup_still_fits_a_frame_comfortably() {
    // The cap has to be affordable on the wire as well as in verification time.
    let msg = Message::TopUp(TopUp {
        updates: (0..MAX_CHANNEL_UPDATES)
            .map(|i| ChannelUpdate {
                channel_id: channel(i as u8),
                cumulative: u64::MAX,
                signature: signature(1),
            })
            .collect(),
        window_ms: 30_000,
    });
    let mut buf = Vec::new();
    encode_frame(&msg, &mut buf).expect("encode");
    assert!(
        buf.len() < 1_024,
        "a full top-up reached {} bytes",
        buf.len()
    );
}
