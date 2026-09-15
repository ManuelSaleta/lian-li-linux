use super::*;
use std::io::Write;

#[test]
fn destination_copies_received_descriptors_without_opening_source_labels() {
    transfer_fixture(false);
}

#[test]
fn unexpected_descriptor_path_discards_the_destination_stage() {
    transfer_fixture(true);
}

fn transfer_fixture(wrong_path: bool) {
    let root = tempfile::tempdir().unwrap();
    let label = root.path().join("source-that-does-not-exist.png");
    let lcd: LcdConfig =
        serde_json::from_value(serde_json::json!({"type":"image", "path":label})).unwrap();
    let bytes = serde_json::to_vec(&(vec![lcd], Vec::<LcdTemplate>::new())).unwrap();
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"descriptor contents").unwrap();
    let (left, right) = Channel::pair().unwrap();
    let worker = std::thread::spawn(move || -> Result<()> {
        let channel = Channel::new(left, Duration::from_secs(3))?;
        let sealed = crate::state_transfer::sealed_state(&bytes)?;
        channel.send(
            &Message::Selection {
                bytes: bytes.len(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
            Some(sealed.as_fd()),
        )?;
        channel.send(
            &Message::Asset {
                path: if wrong_path {
                    PathBuf::from("/unexpected")
                } else {
                    label
                },
            },
            Some(source.as_fd()),
        )?;
        let (message, fd) = channel.receive::<Message>()?;
        ensure!(
            matches!(message, Message::Complete) && fd.is_none(),
            "Expected completion"
        );
        Ok(())
    });
    let control = CopyControl::new(Duration::from_secs(3));
    let result = receive(right, root.path(), Path::new("/managed"), &control);
    let sender = worker.join().unwrap();
    if wrong_path {
        assert!(result.is_err());
        assert!(sender.is_err());
    } else {
        sender.unwrap();
        let prepared = result.unwrap();
        let name = prepared.lcds[0].path.as_ref().unwrap().file_name().unwrap();
        assert_eq!(
            std::fs::read(prepared.media.directory().join(name)).unwrap(),
            b"descriptor contents"
        );
        drop(prepared);
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn selection_sender_and_receiver_complete_with_original_files_preserved() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("original.png");
    std::fs::write(&source, b"original").unwrap();
    let lcd = serde_json::from_value(serde_json::json!({"type":"image", "path":source})).unwrap();
    let (left, right) = Channel::pair().unwrap();
    let worker = std::thread::spawn(move || {
        send(
            left,
            vec![lcd],
            vec![],
            &CopyControl::new(Duration::from_secs(3)),
        )
    });
    let prepared = receive(
        right,
        root.path(),
        Path::new("/managed"),
        &CopyControl::new(Duration::from_secs(3)),
    )
    .unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(prepared.media.unique_files(), 1);
    drop(prepared);
    assert_eq!(std::fs::read(source).unwrap(), b"original");
}
