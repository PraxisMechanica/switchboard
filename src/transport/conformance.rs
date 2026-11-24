//! Generic transport conformance suite verifying `MemoryTransport`, `IrohTransport`, and framing.

use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;

use super::frame::{FrameKind, WireFrame};
use super::{PeerDialer, PeerEndpoint, PeerListener, TransportError, PROTOCOL_VERSION};

/// Verifies core dialer/listener semantics across any [`PeerDialer`] + [`PeerListener`] pair.
pub async fn verify_transport_pair<D, L>(
    alice_dialer: &D,
    bob_listener: &L,
    bob_ep: PeerEndpoint,
    unreachable_ep: PeerEndpoint,
) where
    D: PeerDialer,
    L: PeerListener,
{
    let request_timeout = Duration::from_secs(5);

    // 1. Authenticated CallRequest -> CallResponse round-trip.
    let call_req = WireFrame::new(
        FrameKind::CallRequest,
        json!({ "op": "ping", "seq": 1, "nested": { "flag": true } }),
    );
    let expected_call_req = call_req.clone();
    let expected_call_resp =
        WireFrame::new(FrameKind::CallResponse, json!({ "ok": true, "seq": 1 }));
    let bob_reply = expected_call_resp.clone();
    let alice_id = alice_dialer.local_peer_id();

    let alice_fut = alice_dialer.request(&bob_ep, call_req, request_timeout);
    let bob_fut = async {
        let exchange = bob_listener
            .accept()
            .await
            .expect("bob should accept call request");
        assert_eq!(exchange.remote_peer, alice_id);
        assert_eq!(exchange.request.version, PROTOCOL_VERSION);
        assert_eq!(exchange.request, expected_call_req);
        exchange
            .respond(bob_reply)
            .expect("bob should send call response");
    };

    let (alice_res, ()) = tokio::join!(alice_fut, bob_fut);
    let actual_resp = alice_res.expect("alice should receive call response");
    assert_eq!(actual_resp, expected_call_resp);

    // 2. Second frame kind (Knock -> KnockDecision) round-trip.
    let knock_req = WireFrame::new(FrameKind::Knock, json!({ "handle": "alice" }));
    let expected_knock_req = knock_req.clone();
    let expected_knock_resp =
        WireFrame::new(FrameKind::KnockDecision, json!({ "decision": "accepted" }));
    let bob_knock_reply = expected_knock_resp.clone();

    let alice_knock_fut = alice_dialer.request(&bob_ep, knock_req, request_timeout);
    let bob_knock_fut = async {
        let exchange = bob_listener
            .accept()
            .await
            .expect("bob should accept knock");
        assert_eq!(exchange.remote_peer, alice_id);
        assert_eq!(exchange.request, expected_knock_req);
        exchange
            .respond(bob_knock_reply)
            .expect("bob should send knock decision");
    };

    let (knock_res, ()) = tokio::join!(alice_knock_fut, bob_knock_fut);
    assert_eq!(
        knock_res.expect("alice should receive knock decision"),
        expected_knock_resp
    );

    // 3. Timeout when listener accepts an exchange without replying within the deadline.
    let done_notify = Notify::new();
    let hold_req = WireFrame::new(FrameKind::CallRequest, json!({ "op": "slow" }));

    let alice_timeout_fut = async {
        let res = alice_dialer
            .request(&bob_ep, hold_req, Duration::from_millis(200))
            .await;
        done_notify.notify_one();
        res
    };

    let bob_hold_fut = async {
        if let Ok(Ok(exchange)) =
            tokio::time::timeout(Duration::from_secs(2), bob_listener.accept()).await
        {
            assert_eq!(exchange.remote_peer, alice_id);
            done_notify.notified().await;
            drop(exchange);
        }
    };

    let (timeout_res, ()) = tokio::join!(alice_timeout_fut, bob_hold_fut);
    assert!(
        matches!(timeout_res, Err(TransportError::TimedOut)),
        "expected TimedOut, got {timeout_res:?}"
    );

    // 4. Dialing an unreachable peer fails cleanly within the requested timeout.
    let unreachable_req = WireFrame::new(FrameKind::Knock, json!({ "ping": "unreachable" }));
    let unreachable_res = alice_dialer
        .request(&unreachable_ep, unreachable_req, Duration::from_millis(300))
        .await;
    assert!(
        matches!(
            unreachable_res,
            Err(TransportError::Unavailable(_)
                | TransportError::Connect(_)
                | TransportError::TimedOut)
        ),
        "expected Unavailable/Connect/TimedOut for unreachable peer, got {unreachable_res:?}"
    );
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use proptest::prelude::*;
    use serde_json::json;

    use crate::identity::LocalKeypair;

    use super::super::frame::{decode_frame, encode_frame, read_frame, write_frame};
    use super::super::{InboundExchange, IrohTransport, MemoryHub, NetworkMode, MAX_FRAME_BYTES};
    use super::*;

    fn arb_frame_kind() -> impl Strategy<Value = FrameKind> {
        prop_oneof![
            Just(FrameKind::Knock),
            Just(FrameKind::KnockDecision),
            Just(FrameKind::CallRequest),
            Just(FrameKind::CallResponse),
            Just(FrameKind::Error),
        ]
    }

    #[tokio::test]
    async fn memory_transport_passes_conformance() {
        let hub = MemoryHub::new();
        let alice_kp = LocalKeypair::generate();
        let bob_kp = LocalKeypair::generate();
        let unreachable_kp = LocalKeypair::generate();

        let alice = hub.register(alice_kp.peer_id());
        let bob = hub.register(bob_kp.peer_id());

        verify_transport_pair(
            &alice,
            &bob,
            bob.local_endpoint(),
            PeerEndpoint::from_peer_id(unreachable_kp.peer_id()),
        )
        .await;
    }

    #[tokio::test]
    async fn iroh_vpn_only_loopback_passes_conformance() {
        let alice_kp = LocalKeypair::generate();
        let bob_kp = LocalKeypair::generate();
        let unreachable_kp = LocalKeypair::generate();

        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("valid loopback addr");

        let alice = IrohTransport::bind(
            &alice_kp,
            NetworkMode::VpnOnly {
                bind_addr: Some(loopback),
                custom_relays: Vec::new(),
            },
        )
        .await
        .expect("alice iroh bind");

        let bob = IrohTransport::bind(
            &bob_kp,
            NetworkMode::VpnOnly {
                bind_addr: Some(loopback),
                custom_relays: Vec::new(),
            },
        )
        .await
        .expect("bob iroh bind");

        assert_eq!(alice.local_peer_id(), alice_kp.peer_id());
        assert_eq!(bob.local_peer_id(), bob_kp.peer_id());

        let bob_ep = bob.local_endpoint();
        assert_eq!(bob_ep.peer_id, bob_kp.peer_id());
        assert!(bob_ep.relay_url.is_none());
        assert!(!bob_ep.direct_addrs.is_empty());

        verify_transport_pair(
            &alice,
            &bob,
            bob_ep,
            PeerEndpoint::from_peer_id(unreachable_kp.peer_id()),
        )
        .await;

        alice.shutdown().await;
        bob.shutdown().await;
    }

    proptest! {
        #[test]
        fn frame_codec_roundtrips_arbitrary_payloads(
            kind in arb_frame_kind(),
            text in ".*",
            count in any::<i64>(),
            flag in any::<bool>(),
        ) {
            let frame = WireFrame::new(
                kind,
                json!({
                    "text": text,
                    "count": count,
                    "flag": flag,
                }),
            );
            let encoded = encode_frame(&frame).expect("encode valid frame");
            let decoded = decode_frame(&encoded).expect("decode valid frame");
            prop_assert_eq!(decoded, frame);
        }
    }

    #[tokio::test]
    async fn async_stream_read_write_roundtrips_all_kinds() {
        let kinds = [
            FrameKind::Knock,
            FrameKind::KnockDecision,
            FrameKind::CallRequest,
            FrameKind::CallResponse,
            FrameKind::Error,
        ];

        for (idx, kind) in kinds.into_iter().enumerate() {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let frame = WireFrame::new(kind, json!({ "idx": idx }));
            let expected = frame.clone();

            let writer = async move {
                write_frame(&mut client, &frame).await.expect("write frame");
            };
            let reader = async move { read_frame(&mut server).await.expect("read frame") };

            let ((), actual) = tokio::join!(writer, reader);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn frame_codec_rejects_unsupported_version() {
        let bad_frame = WireFrame {
            version: PROTOCOL_VERSION + 1,
            kind: FrameKind::Knock,
            payload: json!({}),
        };

        assert!(matches!(
            encode_frame(&bad_frame),
            Err(TransportError::UnsupportedVersion(v)) if v == PROTOCOL_VERSION + 1
        ));

        let raw_body = serde_json::to_vec(&bad_frame).expect("serialize bad version frame");
        let mut raw = Vec::with_capacity(4 + raw_body.len());
        raw.extend_from_slice(&(raw_body.len() as u32).to_be_bytes());
        raw.extend_from_slice(&raw_body);

        assert!(matches!(
            decode_frame(&raw),
            Err(TransportError::UnsupportedVersion(v)) if v == PROTOCOL_VERSION + 1
        ));
    }

    #[tokio::test]
    async fn frame_codec_rejects_oversized_and_malformed_frames() {
        let oversized_len = (MAX_FRAME_BYTES + 1) as u32;
        let header_only = oversized_len.to_be_bytes();

        assert!(matches!(
            decode_frame(&header_only),
            Err(TransportError::FrameTooLarge(n)) if n == MAX_FRAME_BYTES + 1
        ));

        let mut cursor = std::io::Cursor::new(header_only);
        assert!(matches!(
            read_frame(&mut cursor).await,
            Err(TransportError::FrameTooLarge(n)) if n == MAX_FRAME_BYTES + 1
        ));

        let huge_frame = WireFrame::new(
            FrameKind::CallRequest,
            json!({ "blob": "a".repeat(MAX_FRAME_BYTES + 64) }),
        );
        assert!(matches!(
            encode_frame(&huge_frame),
            Err(TransportError::FrameTooLarge(_))
        ));

        assert!(matches!(
            decode_frame(&[0, 1]),
            Err(TransportError::InvalidFrame(_))
        ));

        let mut mismatched = Vec::new();
        mismatched.extend_from_slice(&10u32.to_be_bytes());
        mismatched.extend_from_slice(b"{}");
        assert!(matches!(
            decode_frame(&mismatched),
            Err(TransportError::InvalidFrame(_))
        ));

        let mut bad_json = Vec::new();
        bad_json.extend_from_slice(&3u32.to_be_bytes());
        bad_json.extend_from_slice(b"{{{");
        assert!(matches!(
            decode_frame(&bad_json),
            Err(TransportError::InvalidFrame(_))
        ));
    }

    #[tokio::test]
    async fn inbound_exchange_test_helper_delivers_or_reports_closed() {
        let kp = LocalKeypair::generate();
        let req = WireFrame::new(FrameKind::Knock, json!({ "hi": 1 }));
        let resp = WireFrame::new(FrameKind::KnockDecision, json!({ "ok": true }));

        let (exchange, rx) = InboundExchange::new_for_test(kp.peer_id(), req);
        exchange.respond(resp.clone()).expect("respond succeeds");
        assert_eq!(rx.await.expect("receiver gets reply"), resp);

        let (exchange2, rx2) = InboundExchange::new_for_test(
            kp.peer_id(),
            WireFrame::new(FrameKind::Error, json!({})),
        );
        drop(rx2);
        assert!(matches!(
            exchange2.respond(WireFrame::new(FrameKind::Error, json!({}))),
            Err(TransportError::Closed)
        ));
    }
}
