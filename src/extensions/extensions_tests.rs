use std::str::FromStr;

use crate::daemon::parse_call_target;
use crate::primitives::FixedClock;

use super::*;

#[test]
fn target_address_validation_and_dotted_parsing() {
    let dotted = ExtensionName::new("codex.01a1f360-26fc-7273-af47-5bf87a88c890").unwrap();
    assert_eq!(
        dotted.parse_harness_target().unwrap(),
        Some((HarnessKind::Codex, "01a1f360-26fc-7273-af47-5bf87a88c890"))
    );

    let uri =
        ExtensionName::new("codex.codex://threads/01a1f360-26fc-7273-af47-5bf87a88c890").unwrap();
    assert_eq!(
        uri.parse_harness_target().unwrap(),
        Some((
            HarnessKind::Codex,
            "codex://threads/01a1f360-26fc-7273-af47-5bf87a88c890"
        ))
    );

    assert_eq!(
        parse_call_target("andrin.codex.codex://threads/01a1f360-26fc-7273-af47-5bf87a88c890"),
        Some((
            "andrin",
            "codex.codex://threads/01a1f360-26fc-7273-af47-5bf87a88c890"
        ))
    );
    assert_eq!(
        parse_call_target("andrin.codex.01a1f360-26fc-7273-af47-5bf87a88c890"),
        Some(("andrin", "codex.01a1f360-26fc-7273-af47-5bf87a88c890"))
    );
    assert_eq!(
        parse_call_target("dante.codex.Test Euclid 2.0 with Petraco DB"),
        Some(("dante", "codex.Test Euclid 2.0 with Petraco DB"))
    );

    assert!(matches!(
        ExtensionName::new(""),
        Err(ExtensionError::InvalidName(_))
    ));
    assert!(matches!(
        ExtensionName::new(" leading-space"),
        Err(ExtensionError::InvalidName(_))
    ));
    assert!(matches!(
        ExtensionName::new("bad\nnewline"),
        Err(ExtensionError::InvalidName(_))
    ));
    assert!(matches!(
        ExtensionName::new(&"a".repeat(161)),
        Err(ExtensionError::InvalidName(_))
    ));
}

#[test]
fn harness_kind_roundtrips() {
    assert_eq!(HarnessKind::Codex.to_string(), "codex");
    assert_eq!(HarnessKind::Agy.to_string(), "agy");
    assert_eq!(HarnessKind::from_str("codex").unwrap(), HarnessKind::Codex);
    assert_eq!(HarnessKind::from_str("agy").unwrap(), HarnessKind::Agy);
    assert_eq!(
        HarnessKind::from_str("antigravity").unwrap(),
        HarnessKind::Agy
    );
    assert!(matches!(
        HarnessKind::from_str("unknown"),
        Err(ExtensionError::UnsupportedHarness(_))
    ));
}

#[tokio::test]
async fn delivers_to_dotted_uuid_uri_and_named_sessions_without_registration() {
    let db = Database::open_in_memory().unwrap();
    db.with_conn_sync(|c| migrate(c).unwrap());

    let harness = StubHarness::new().with_sessions(vec![CodexSessionInfo {
        id: "01a20d93-57ce-7150-8daf-77cf1172377e".to_string(),
        harness: HarnessKind::Codex,
        cwd: "/tmp/euclid".to_string(),
        title: "Verify Euclid cluster access".to_string(),
        updated_at: None,
    }]);
    let svc = ExtensionsService::new(db, harness, FixedClock::new(1_700_000_000_000));

    // 1. `<harness>.<uuid>`
    let by_uuid = ExtensionName::new("codex.01a1f360-26fc-7273-af47-5bf87a88c890").unwrap();
    let r1 = svc
        .deliver(
            &by_uuid,
            Uuid::now_v7(),
            "dante".into(),
            "ping".into(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        r1,
        DeliveryOutcome::Replied { session_id: Some(ref s), .. }
            if s == "01a1f360-26fc-7273-af47-5bf87a88c890"
    ));

    // 2. `<harness>.codex://threads/<uuid>`
    let by_uri =
        ExtensionName::new("codex.codex://threads/01a1f360-26fc-7273-af47-5bf87a88c890").unwrap();
    let r2 = svc
        .deliver(
            &by_uri,
            Uuid::now_v7(),
            "dante".into(),
            "ping".into(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        r2,
        DeliveryOutcome::Replied { session_id: Some(ref s), .. }
            if s == "01a1f360-26fc-7273-af47-5bf87a88c890"
    ));

    // 3. `<harness>.<conversation-slug>` resolves existing session by title
    let by_slug = ExtensionName::new("codex.verify-euclid-cluster-access").unwrap();
    let r3 = svc
        .deliver(
            &by_slug,
            Uuid::now_v7(),
            "dante".into(),
            "summary?".into(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        r3,
        DeliveryOutcome::Replied { session_id: Some(ref s), .. }
            if s == "01a20d93-57ce-7150-8daf-77cf1172377e"
    ));

    // 4. Unknown conversation name fails cleanly without starting a new session
    let missing = ExtensionName::new("codex.nonexistent-session").unwrap();
    assert!(matches!(
        svc.deliver(
            &missing,
            Uuid::now_v7(),
            "dante".into(),
            "hi".into(),
            None,
            CancellationToken::new(),
        )
        .await,
        Err(ExtensionError::SessionNotFound(_))
    ));
}
