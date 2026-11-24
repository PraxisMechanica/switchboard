//! Generic conformance suite for [`DirectoryPort`] implementations and directory tests.

use std::collections::HashMap;

use crate::identity::{self, LocalKeypair};
use crate::primitives::Timestamp;

use super::card::{verify_peer_card, AdvertisedExtension, PeerCard};
use super::DirectoryPort;

/// Verifies that any [`DirectoryPort`] supports empty fetch, publish/fetch roundtrip, and overwrite.
pub async fn verify_directory_port<D: DirectoryPort>(dir: &D) {
    let initial = dir
        .fetch_all()
        .await
        .expect("fetch_all on empty directory must succeed");
    assert!(
        initial.is_empty(),
        "expected empty directory before publishing"
    );

    let alice_kp = LocalKeypair::generate();
    let bob_kp = LocalKeypair::generate();

    let alice_v1 = PeerCard {
        handle: "alice".to_string(),
        peer_id: alice_kp.peer_id(),
        direct_addrs: vec!["10.0.0.10:4433".to_string()],
        extensions: vec![AdvertisedExtension {
            name: "review".to_string(),
            description: "Code review agent".to_string(),
        }],
        updated_at: Timestamp::from_millis(1_700_000_000_000),
    };
    let bob_card = PeerCard {
        handle: "bob".to_string(),
        peer_id: bob_kp.peer_id(),
        direct_addrs: vec!["10.0.0.11:4433".to_string()],
        extensions: vec![],
        updated_at: Timestamp::from_millis(1_700_000_001_000),
    };

    dir.publish(identity::sign(&alice_kp, alice_v1.clone()))
        .await
        .expect("publish alice v1 must succeed");
    dir.publish(identity::sign(&bob_kp, bob_card.clone()))
        .await
        .expect("publish bob must succeed");

    let fetched = dir
        .fetch_all()
        .await
        .expect("fetch_all after publishing must succeed");
    assert_eq!(fetched.len(), 2);

    let by_handle: HashMap<String, PeerCard> = fetched
        .into_iter()
        .map(|signed| {
            let verified = verify_peer_card(signed).expect("published card must verify");
            let (_, card) = verified.into_parts();
            (card.handle.clone(), card)
        })
        .collect();
    assert_eq!(by_handle.get("alice"), Some(&alice_v1));
    assert_eq!(by_handle.get("bob"), Some(&bob_card));

    let alice_v2 = PeerCard {
        handle: "alice".to_string(),
        peer_id: alice_kp.peer_id(),
        direct_addrs: vec!["10.0.0.20:4433".to_string(), "10.0.0.21:4433".to_string()],
        extensions: vec![
            AdvertisedExtension {
                name: "review".to_string(),
                description: "Code review agent v2".to_string(),
            },
            AdvertisedExtension {
                name: "deploy".to_string(),
                description: "Deployment helper".to_string(),
            },
        ],
        updated_at: Timestamp::from_millis(1_700_000_005_000),
    };

    dir.publish(identity::sign(&alice_kp, alice_v2.clone()))
        .await
        .expect("update overwrite for alice must succeed");
    dir.publish(identity::sign(&alice_kp, alice_v2.clone()))
        .await
        .expect("idempotent republish for alice must succeed");

    let updated = dir
        .fetch_all()
        .await
        .expect("fetch_all after update must succeed");
    assert_eq!(updated.len(), 2);

    let updated_by_handle: HashMap<String, PeerCard> = updated
        .into_iter()
        .map(|signed| {
            let verified = verify_peer_card(signed).expect("updated card must verify");
            let (_, card) = verified.into_parts();
            (card.handle.clone(), card)
        })
        .collect();
    assert_eq!(updated_by_handle.get("alice"), Some(&alice_v2));
    assert_eq!(updated_by_handle.get("bob"), Some(&bob_card));
}

#[cfg(test)]
mod tests {
    use tokio::process::Command;

    use crate::db::Database;

    use super::*;
    use crate::directory::{
        migrate, DirectoryError, DirectoryService, GitPhonebook, MemoryDirectory, SyncReport,
    };

    async fn init_bare_repo(path: &std::path::Path) {
        let output = Command::new("git")
            .args(["init", "--bare"])
            .current_dir(path)
            .output()
            .await
            .expect("git init --bare must spawn");
        assert!(
            output.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn memory_directory_passes_conformance() {
        let dir = MemoryDirectory::new();
        verify_directory_port(&dir).await;
    }

    #[tokio::test]
    async fn git_phonebook_passes_conformance() {
        let bare_dir = tempfile::tempdir().expect("create bare tempdir");
        let work_dir = tempfile::tempdir().expect("create checkout tempdir");
        init_bare_repo(bare_dir.path()).await;

        let remote_url = bare_dir.path().to_string_lossy().to_string();
        let phonebook = GitPhonebook::new(remote_url.clone(), work_dir.path().join("checkout"));
        verify_directory_port(&phonebook).await;

        let peer_clone_dir = tempfile::tempdir().expect("create peer checkout tempdir");
        let peer_phonebook = GitPhonebook::new(remote_url, peer_clone_dir.path().join("checkout"));
        let cards = peer_phonebook
            .fetch_all()
            .await
            .expect("second peer must fetch published cards");
        assert_eq!(cards.len(), 2);
    }

    #[tokio::test]
    async fn git_phonebook_handles_empty_clone_followed_by_remote_publish() {
        let bare_dir = tempfile::tempdir().expect("create bare tempdir");
        let peer_a_dir = tempfile::tempdir().expect("create peer A tempdir");
        let peer_b_dir = tempfile::tempdir().expect("create peer B tempdir");
        init_bare_repo(bare_dir.path()).await;

        let remote_url = bare_dir.path().to_string_lossy().to_string();
        let peer_a = GitPhonebook::new(remote_url.clone(), peer_a_dir.path().join("repo"));
        let peer_b = GitPhonebook::new(remote_url, peer_b_dir.path().join("repo"));

        assert!(peer_b.fetch_all().await.expect("empty fetch").is_empty());

        let alice_kp = LocalKeypair::generate();
        let alice_card = PeerCard {
            handle: "alice".to_string(),
            peer_id: alice_kp.peer_id(),
            direct_addrs: vec!["10.0.0.5:4433".to_string()],
            extensions: vec![],
            updated_at: Timestamp::from_millis(1_700_000_010_000),
        };
        peer_a
            .publish(identity::sign(&alice_kp, alice_card.clone()))
            .await
            .expect("peer A publish");

        let synced_cards = peer_b
            .fetch_all()
            .await
            .expect("peer B fetch after first commit");
        assert_eq!(synced_cards.len(), 1);
        let verified = verify_peer_card(synced_cards.into_iter().next().expect("one card"))
            .expect("verified card");
        assert_eq!(verified.payload(), &alice_card);
    }

    #[tokio::test]
    async fn sync_rejects_tampered_and_signer_mismatch_cards_without_caching() {
        let db = Database::open_in_memory().expect("open in-memory db");
        db.with_conn_sync(|conn| migrate(conn).expect("run directory migration"));

        let port = MemoryDirectory::new();
        let alice_kp = LocalKeypair::generate();
        let bob_kp = LocalKeypair::generate();
        let mallory_kp = LocalKeypair::generate();
        let bad_handle_kp = LocalKeypair::generate();

        let alice_card = PeerCard {
            handle: "alice".to_string(),
            peer_id: alice_kp.peer_id(),
            direct_addrs: vec!["10.0.0.1:4433".to_string()],
            extensions: vec![],
            updated_at: Timestamp::from_millis(1_700_000_000_000),
        };
        port.publish(identity::sign(&alice_kp, alice_card.clone()))
            .await
            .expect("publish alice");

        let mut tampered_bob = identity::sign(
            &bob_kp,
            PeerCard {
                handle: "bob".to_string(),
                peer_id: bob_kp.peer_id(),
                direct_addrs: vec!["10.0.0.2:4433".to_string()],
                extensions: vec![],
                updated_at: Timestamp::from_millis(1_700_000_000_000),
            },
        );
        tampered_bob
            .payload
            .direct_addrs
            .push("198.51.100.77:9999".to_string());
        port.publish(tampered_bob)
            .await
            .expect("publish tampered bob");

        let forged_mallory = identity::sign(
            &mallory_kp,
            PeerCard {
                handle: "mallory".to_string(),
                peer_id: alice_kp.peer_id(),
                direct_addrs: vec!["198.51.100.88:9999".to_string()],
                extensions: vec![],
                updated_at: Timestamp::from_millis(1_700_000_000_000),
            },
        );
        port.publish(forged_mallory)
            .await
            .expect("publish signer-mismatch card");

        let bad_handle_card = identity::sign(
            &bad_handle_kp,
            PeerCard {
                handle: "Bad Handle!".to_string(),
                peer_id: bad_handle_kp.peer_id(),
                direct_addrs: vec![],
                extensions: vec![],
                updated_at: Timestamp::from_millis(1_700_000_000_000),
            },
        );
        port.publish(bad_handle_card)
            .await
            .expect("publish invalid handle card");

        let service = DirectoryService::new(db, port);
        let report = service.sync().await.expect("sync must succeed");
        assert_eq!(
            report,
            SyncReport {
                synced: 1,
                rejected: 3,
            }
        );

        let cached = service.list_cached().await.expect("list cached");
        assert_eq!(cached, vec![alice_card.clone()]);
        assert_eq!(
            service
                .resolve_handle("alice")
                .await
                .expect("resolve alice"),
            Some(alice_card.clone())
        );
        assert_eq!(
            service
                .resolve_peer_id(alice_kp.peer_id())
                .await
                .expect("resolve alice peer_id"),
            Some(alice_card)
        );
        assert_eq!(
            service.resolve_handle("bob").await.expect("resolve bob"),
            None
        );
        assert_eq!(
            service
                .resolve_handle("mallory")
                .await
                .expect("resolve mallory"),
            None
        );
        assert_eq!(
            service
                .resolve_peer_id(bob_kp.peer_id())
                .await
                .expect("resolve bob peer_id"),
            None
        );
    }

    #[tokio::test]
    async fn publish_local_validates_keypair_caches_and_publishes() {
        let db = Database::open_in_memory().expect("open in-memory db");
        db.with_conn_sync(|conn| migrate(conn).expect("run directory migration"));

        let port = MemoryDirectory::new();
        let service = DirectoryService::new(db, port.clone());

        let alice_kp = LocalKeypair::generate();
        let other_kp = LocalKeypair::generate();

        let card = PeerCard {
            handle: "alice".to_string(),
            peer_id: alice_kp.peer_id(),
            direct_addrs: vec!["10.0.0.1:4433".to_string()],
            extensions: vec![],
            updated_at: Timestamp::from_millis(1_700_000_000_000),
        };

        let mismatch_err = service
            .publish_local(&other_kp, card.clone())
            .await
            .expect_err("wrong keypair must fail");
        assert!(matches!(
            mismatch_err,
            DirectoryError::SignerMismatch { .. }
        ));
        assert!(service.list_cached().await.expect("list cached").is_empty());
        assert!(port.fetch_all().await.expect("fetch port").is_empty());

        service
            .publish_local(&alice_kp, card.clone())
            .await
            .expect("publish_local with matching keypair must succeed");

        assert_eq!(
            service
                .resolve_handle("alice")
                .await
                .expect("resolve handle"),
            Some(card.clone())
        );
        assert_eq!(
            service
                .resolve_peer_id(alice_kp.peer_id())
                .await
                .expect("resolve peer_id"),
            Some(card)
        );
        assert_eq!(port.fetch_all().await.expect("fetch port").len(), 1);
    }
}
