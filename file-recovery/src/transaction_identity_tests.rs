use super::{InodeIdentity, Transaction, TransactionIdentity, WindowsIdentity};
use serde_json::json;

fn files() -> InodeIdentity {
    InodeIdentity {
        parent_device: 9,
        parent_inode: 10,
        directory_inode: Some(11),
        original_inode: 12,
        staged_inode: Some(13),
    }
}

#[test]
fn identities_keep_their_platform_on_every_host() {
    for (identity, supported) in [
        (
            TransactionIdentity::Macos {
                volume_uuid: [7; 16],
                files: files(),
            },
            cfg!(target_os = "macos"),
        ),
        (
            TransactionIdentity::Unix { files: files() },
            cfg!(all(unix, not(target_os = "macos"))),
        ),
    ] {
        let tx = Transaction {
            parent: "/fixture".into(),
            directory: ".agent-transaction-fixture".into(),
            identity,
        };
        let reopened: Transaction =
            serde_json::from_value(serde_json::to_value(&tx).unwrap()).unwrap();
        assert_eq!(reopened.identity, tx.identity);
        assert_eq!(tx.identity.validate_host().is_ok(), supported);
    }
}

#[test]
fn windows_roundtrip_preserves_high_64_bits_and_all_object_ids() {
    let mut first = [1u8; 16];
    let mut second = first;
    second[15] = 2;
    first[8] = 3;
    let tx = Transaction {
        parent: r"C:\fixture".into(),
        directory: ".agent-transaction-fixture".into(),
        identity: TransactionIdentity::Windows {
            files: WindowsIdentity {
                volume_serial: u64::MAX,
                parent_file_id: first,
                directory_file_id: Some(second),
                original_file_id: [4; 16],
                staged_file_id: Some([5; 16]),
            },
        },
    };
    let encoded = serde_json::to_vec(&tx).unwrap();
    let reopened: Transaction = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(reopened.identity, tx.identity);
    assert_ne!(first, second);
    assert_eq!(reopened.identity.validate_host().is_ok(), cfg!(windows));
    let mut malformed = serde_json::to_value(&tx).unwrap();
    malformed["identity"]["files"]["parent_file_id"] = serde_json::to_value([1; 8]).unwrap();
    assert!(serde_json::from_value::<Transaction>(malformed).is_err());
}

#[test]
fn flat_or_unknown_identities_are_rejected() {
    let flat = json!({"parent": "/fixture", "directory": ".agent-transaction-fixture",
        "parent_device": 9, "parent_inode": 10, "directory_inode": 11,
        "original_inode": 12, "staged_inode": 13});
    assert!(serde_json::from_value::<Transaction>(flat).is_err());
    let mut unknown = serde_json::to_value(Transaction {
        parent: "/fixture".into(),
        directory: ".agent-transaction-fixture".into(),
        identity: TransactionIdentity::Unix { files: files() },
    })
    .unwrap();
    unknown["identity"]["platform"] = json!("future");
    assert!(serde_json::from_value::<Transaction>(unknown).is_err());
}
