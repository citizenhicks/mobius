use super::{Thumbnail, ThumbnailKey, Thumbnails};
use crate::ui::runtime::RuntimeState;
use std::collections::BTreeSet;
use std::path::Path;

fn thumbnail(path: &str) -> Thumbnail {
    Thumbnail {
        arrived: std::time::Instant::now(),
        path: Path::new(path).into(),
        width: 4,
        height: 3,
    }
}

#[test]
fn reconnects_keep_file_draws_but_gateway_changes_reject_stale_completions() {
    let mut runtime = RuntimeState::default();
    let session = "chat".parse().unwrap();
    let key = ThumbnailKey::file(&session, "file");
    runtime.thumbnails.requested.insert(key.clone());
    assert_eq!(
        runtime
            .thumbnails
            .retain(std::iter::once(&session), std::iter::empty()),
        BTreeSet::from([session.share()])
    );
    let generation = runtime.thumbnails.file_generation;

    // An explicit reconnect leaves the cache as it is.
    assert!(runtime.thumbnails.complete(
        key.clone(),
        generation,
        thumbnail("/tmp/reconnected.png")
    ));
    assert!(runtime.thumbnails.get(&key).is_some());

    runtime.thumbnails.clear_files();
    assert!(
        runtime.thumbnails.recent.is_empty(),
        "a gateway change must forget recent cache owners"
    );
    // A new gateway may request the same key before the previous draw finishes.
    runtime.thumbnails.requested.insert(key.clone());
    assert!(
        !runtime
            .thumbnails
            .complete(key.clone(), generation, thumbnail("/tmp/stale.png"))
    );
    assert!(runtime.thumbnails.get(&key).is_none());
    assert!(runtime.thumbnails.requested.contains(&key));
    assert!(
        runtime
            .thumbnails
            .discarded
            .contains(&Path::new("/tmp/stale.png").into())
    );
    assert!(runtime.thumbnails.complete(
        key.clone(),
        runtime.thumbnails.file_generation,
        thumbnail("/tmp/current.png")
    ));
    assert_eq!(
        runtime.thumbnails.get(&key).unwrap().path.as_ref(),
        Path::new("/tmp/current.png")
    );
}

/// Switching between chats keeps both chats' pictures: nothing is discarded, so
/// nothing is downloaded, drawn or decoded again. A chat older than the recent few
/// lets its pictures go.
#[test]
fn switching_chats_keeps_recent_pictures_and_lets_old_ones_go() {
    let mut thumbnails = Thumbnails::default();
    let chats: Vec<mobius::SessionId> = (0..6)
        .map(|index| format!("chat-{index}").parse().unwrap())
        .collect();
    let show = |thumbnails: &mut Thumbnails, chat: &mobius::SessionId| {
        thumbnails.retain(std::iter::once(chat), std::iter::empty());
    };
    // Each chat's picture arrives while it shows.
    for (index, chat) in chats.iter().take(2).enumerate() {
        show(&mut thumbnails, chat);
        let key = ThumbnailKey::file(chat, "picture");
        thumbnails.requested.insert(key.clone());
        let generation = thumbnails.file_generation;
        assert!(
            thumbnails.complete(key, generation, thumbnail(&format!("/tmp/{index}.png"))),
            "fixture thumbnail must complete for its current generation"
        );
    }
    show(&mut thumbnails, &chats[0]);
    show(&mut thumbnails, &chats[1]);
    assert!(
        thumbnails.discarded.is_empty(),
        "{:?}",
        thumbnails.discarded
    );
    assert!(
        thumbnails
            .get(&ThumbnailKey::file(&chats[0], "picture"))
            .is_some(),
        "recent chat thumbnail must remain cached"
    );
    thumbnails.forget_chats(&chats[..1]);
    show(&mut thumbnails, &chats[1]);
    assert!(
        thumbnails
            .get(&ThumbnailKey::file(&chats[0], "picture"))
            .is_none(),
        "deleted chat must release its recent picture"
    );
    assert!(
        thumbnails
            .get(&ThumbnailKey::file(&chats[1], "picture"))
            .is_some()
    );
    for chat in &chats[2..] {
        show(&mut thumbnails, chat);
    }
    assert_eq!(
        thumbnails.discarded,
        [
            Path::new("/tmp/0.png").into(),
            Path::new("/tmp/1.png").into()
        ]
    );
}

#[test]
fn promotion_discards_a_replaced_file_only_when_its_path_is_unshared() {
    let mut thumbnails = Thumbnails::default();
    let session = "chat".parse().unwrap();
    let draft = ThumbnailKey::Draft("draft".into());
    let file = ThumbnailKey::file(&session, "file");
    thumbnails.ready.insert(draft, thumbnail("/tmp/draft.png"));
    thumbnails
        .ready
        .insert(file.clone(), thumbnail("/tmp/old.png"));
    thumbnails.promote("draft", &session, "file");
    assert_eq!(thumbnails.discarded, vec![Path::new("/tmp/old.png").into()]);

    // Promoting the same shared draft twice must not schedule its live path for deletion.
    thumbnails.promote("draft", &session, "file");
    assert_eq!(thumbnails.discarded.len(), 1);
    thumbnails.forget_draft("draft");
    assert_eq!(thumbnails.discarded.len(), 1);
    assert!(thumbnails.get(&file).is_some());
}
