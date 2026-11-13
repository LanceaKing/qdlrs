// SPDX-License-Identifier: BSD-3-Clause
use qdl::select_and_open;
use std::cell::Cell;

#[test]
fn missing_selection_never_opens_even_a_single_candidate() {
    for keys in [vec![], vec!["a".into()], vec!["a".into(), "b".into()]] {
        let calls = Cell::new(0);
        assert!(
            select_and_open(&keys, None, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn no_match_or_duplicate_match_never_opens() {
    for keys in [
        vec![],
        vec!["other".into()],
        vec!["target".into(), "target".into()],
    ] {
        let calls = Cell::new(0);
        assert!(
            select_and_open(&keys, Some("target"), |_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn exact_match_opens_only_the_requested_index_once() {
    let keys = vec!["usb:1".into(), "usb:2".into(), "usb:3".into()];
    let calls = Cell::new(0);
    let result = select_and_open(&keys, Some("usb:2"), |index| {
        calls.set(calls.get() + 1);
        Ok(index)
    })
    .unwrap();
    assert_eq!(result, 1);
    assert_eq!(calls.get(), 1);
}

#[test]
fn matching_is_exact_and_open_failure_is_preserved() {
    let keys = vec!["COM4".into()];
    for requested in ["com4", "COM", " COM4", ""] {
        assert!(
            select_and_open(&keys, Some(requested), |_| -> anyhow::Result<()> {
                panic!("unexpected open")
            })
            .is_err()
        );
    }
    let error = select_and_open(&keys, Some("COM4"), |_| -> anyhow::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
    })
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
