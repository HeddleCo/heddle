// SPDX-License-Identifier: Apache-2.0

#![cfg(target_os = "linux")]

use std::{
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use heddle_client::owner_authorization::{
    AuthorizationError, AuthorizationKey, CloneKeyringStore, GuardianSigner, OfflineAuthorizer,
    OfflineRequest, RecoverySetup, VerificationLimits, create_clone_keyring,
    create_direct_capability, create_human_owner_root, mint_subject_biscuit, verify_owner_root,
    wire::{
        CapabilityPrincipal, CapabilityPrincipalKind, CloneOwnerPinKind, SpoolCapabilityAction,
        SpoolCapabilityGrant, SpoolSelector,
    },
};
use sley::Repository as SleyRepository;
use tempfile::TempDir;

const NOW: i64 = 1_000;

fn limits() -> VerificationLimits {
    VerificationLimits::new(300, 3_600, 1024 * 1024).expect("limits")
}

fn heddle(args: &[&str], cwd: Option<&std::path::Path>, test_home: &std::path::Path) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .env_clear()
        .args(args)
        .env("HOME", test_home)
        .env("HEDDLE_CONFIG", test_home.join("config.toml"))
        .env("RUST_BACKTRACE", "1")
        .env("HEDDLE_PRINCIPAL_NAME", "Offline Clone Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "offline-clone@heddle.test");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().expect("run heddle");
    assert!(
        output.status.success(),
        "heddle {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn disconnect_network_for_current_thread() {
    const LOAD_SYSCALL_NUMBER: u16 = 0x20;
    const JUMP_EQUAL: u16 = 0x15;
    const RETURN: u16 = 0x06;
    const SECCOMP_RETURN_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RETURN_ALLOW: u32 = 0x7fff_0000;

    let denied = [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_recvfrom,
        libc::SYS_recvmsg,
        libc::SYS_shutdown,
    ];
    let mut filters = Vec::with_capacity(denied.len() * 2 + 2);
    filters.push(libc::sock_filter {
        code: LOAD_SYSCALL_NUMBER,
        jt: 0,
        jf: 0,
        k: 0,
    });
    for syscall in denied {
        filters.push(libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: syscall as u32,
        });
        filters.push(libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_RETURN_ERRNO | libc::EPERM as u32,
        });
    }
    filters.push(libc::sock_filter {
        code: RETURN,
        jt: 0,
        jf: 0,
        k: SECCOMP_RETURN_ALLOW,
    });
    let program = libc::sock_fprog {
        len: filters.len().try_into().expect("seccomp filter length"),
        filter: filters.as_mut_ptr(),
    };

    // SAFETY: `program` points to `filters` for the duration of both prctl
    // calls. NO_NEW_PRIVS permits this thread to add a strictly more
    // restrictive seccomp filter; the filter returns EPERM for networking
    // syscalls and ALLOW for every other syscall.
    unsafe {
        assert_eq!(
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
            0,
            "enable no-new-privileges"
        );
        assert_eq!(
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ),
            0,
            "install network-denying seccomp filter"
        );
    }
}

fn create_real_clone(temp: &TempDir) -> PathBuf {
    let source = temp.path().join("source");
    let clone = temp.path().join("clone");
    std::fs::create_dir(&source).expect("source directory");
    SleyRepository::init(&source).expect("isolate source from ancestor Git repositories");
    heddle(&["init"], Some(&source), temp.path());
    std::fs::write(source.join("README.md"), "offline clone\n").expect("seed file");
    heddle(
        &["capture", "-m", "seed offline clone"],
        Some(&source),
        temp.path(),
    );
    heddle(
        &[
            "clone",
            source.to_str().expect("source UTF-8"),
            clone.to_str().expect("clone UTF-8"),
        ],
        None,
        temp.path(),
    );
    assert!(clone.join(".heddle").is_dir(), "real clone must exist");
    clone
}

fn install_owner_keyring(clone: &Path) -> (CloneKeyringStore, AuthorizationKey, Vec<u8>) {
    let authority = AuthorizationKey::from_seed([41; 32]).expect("authority");
    let recovery = RecoverySetup::recommended(vec![
        GuardianSigner::paper(AuthorizationKey::from_seed([42; 32]).expect("paper")),
        GuardianSigner::social(AuthorizationKey::from_seed([43; 32]).expect("social")),
    ])
    .expect("recovery");
    let root = create_human_owner_root([44; 16], &authority, &recovery).expect("owner root");
    let state = verify_owner_root(&root).expect("owner state");
    let subject_key = AuthorizationKey::from_seed([45; 32]).expect("subject");
    let spool_uuid = [46; 16];
    let capability = create_direct_capability(
        &state,
        &authority,
        CapabilityPrincipal {
            kind: CapabilityPrincipalKind::Agent as i32,
            principal_id: b"offline-agent".to_vec(),
            key: Some(subject_key.verification_key()),
        },
        vec![SpoolCapabilityGrant {
            spool: Some(SpoolSelector {
                root_spool_uuid: spool_uuid.to_vec(),
                path_segments: vec!["acme".to_string()],
                include_descendants: true,
            }),
            actions: vec![SpoolCapabilityAction::Read as i32],
        }],
        NOW - 10,
        NOW + 200,
        limits(),
    )
    .expect("public clone capability");
    let biscuit = mint_subject_biscuit(
        capability.capability.as_ref().expect("capability"),
        &subject_key,
    )
    .expect("subject Biscuit");
    let keyring = create_clone_keyring(
        spool_uuid,
        vec!["acme".to_string(), "heddle".to_string()],
        CloneOwnerPinKind::InvitationFingerprint,
        state.owner_id(),
        NOW,
        root,
        Vec::new(),
        vec![capability],
        NOW,
        limits(),
    )
    .expect("keyring");
    let store = CloneKeyringStore::new(clone.join(".heddle"), limits());
    store.install(keyring, NOW).expect("pin clone keyring");
    println!("CLONE_KEYRING={}", store.path().display());
    (store, subject_key, biscuit)
}

fn prove_offline_allow_and_deny(
    store: CloneKeyringStore,
    subject_key: AuthorizationKey,
    biscuit: Vec<u8>,
) {
    disconnect_network_for_current_thread();
    let probe = TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9).into(),
        Duration::from_millis(50),
    )
    .expect_err("seccomp must make the network genuinely unreachable");
    assert_eq!(probe.raw_os_error(), Some(libc::EPERM));
    println!("NETWORK_PROBE=blocked errno=EPERM syscall=socket");

    let authorizer = OfflineAuthorizer::new(
        store
            .load(NOW)
            .expect("load keyring after network is blocked"),
    );
    let request = |action| OfflineRequest {
        subject_kind: CapabilityPrincipalKind::Agent,
        principal_id: b"offline-agent".to_vec(),
        subject_key: Some(subject_key.verification_key()),
        subject_biscuit: biscuit.clone(),
        path_segments: vec!["acme".to_string(), "heddle".to_string(), "src".to_string()],
        action,
        now_unix_seconds: NOW,
    };
    authorizer
        .authorize(&request(SpoolCapabilityAction::Read))
        .expect("offline READ allow");
    println!("OFFLINE_ALLOW=READ path=acme/heddle/src");
    assert!(matches!(
        authorizer.authorize(&request(SpoolCapabilityAction::Write)),
        Err(AuthorizationError::CapabilityDenied(_))
    ));
    println!("OFFLINE_DENY=WRITE path=acme/heddle/src");

    let sibling_request = OfflineRequest {
        subject_kind: CapabilityPrincipalKind::Agent,
        principal_id: b"offline-agent".to_vec(),
        subject_key: Some(subject_key.verification_key()),
        subject_biscuit: biscuit,
        path_segments: vec!["acme".to_string(), "sibling".to_string()],
        action: SpoolCapabilityAction::Read,
        now_unix_seconds: NOW,
    };
    assert!(matches!(
        authorizer.authorize(&sibling_request),
        Err(AuthorizationError::CapabilityDenied(_))
    ));
    println!("OFFLINE_DENY=READ path=acme/sibling reason=pinned-clone");
}

#[test]
fn real_clone_pins_keyring_then_allows_and_denies_with_network_syscalls_blocked() {
    let temp = TempDir::new().expect("tempdir");
    let clone = create_real_clone(&temp);
    let (store, subject_key, biscuit) = install_owner_keyring(&clone);
    prove_offline_allow_and_deny(store, subject_key, biscuit);
}
