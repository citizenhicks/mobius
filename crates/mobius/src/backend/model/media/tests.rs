use super::*;
use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};
use crate::protocol::{ContentPart, ImageDetail, ToolContent};

#[test]
fn replay_drops_only_old_images_after_measuring_full_body_and_keeps_fresh_batch() {
    let original = vec![
        serde_json::json!({"role":"user", "content":[{"type":"input_text","text":"keep text"},{"type":"input_image","image":{"file":{"id":"old"}}}]}),
        serde_json::json!({"type":"function_call","call_id":"a"}),
        serde_json::json!({"type":"function_call","call_id":"b"}),
        serde_json::json!({"type":"function_call_output","call_id":"a","output":[{"type":"input_image","image":{"file":{"id":"fresh-a"}}}]}),
        serde_json::json!({"type":"function_call_output","call_id":"b","output":[{"type":"input_text","text":"result text"},{"type":"input_image","image":{"file":{"id":"fresh-b"}}}]}),
    ];
    let mut prepared = original.clone();
    for part in prepared.iter_mut().filter_map(content_parts_mut).flatten() {
        if part.get("image").is_some() {
            *part = serde_json::json!({"type":"input_image", "data":"x".repeat(1000)});
        }
    }
    let mut prepared = prepared.into_iter().map(Arc::new).collect::<Vec<_>>();
    let measure = |input: &[Arc<Value>]| {
        crate::serialized_len(&serde_json::json!({"input":input,"tools":"schema".repeat(40)}))
    };
    let initial = measure(&prepared).expect("size");
    let unchanged = prepared.clone();
    bound_request(
        &mut prepared,
        (&original).into(),
        ImageInputLimits::default(),
        initial,
        true,
        measure,
    )
    .expect("exact limit");
    assert_eq!(prepared, unchanged);
    bound_request(
        &mut prepared,
        (&original).into(),
        ImageInputLimits::default(),
        initial - 500,
        true,
        measure,
    )
    .expect("strip oldest");
    assert_eq!(prepared[0]["content"][0]["text"], "keep text");
    assert!(
        prepared[0]["content"][1]["text"]
            .as_str()
            .expect("placeholder")
            .contains("old")
    );
    assert_eq!(prepared[1..], unchanged[1..]);
    assert!(
        bound_request(
            &mut prepared,
            (&original).into(),
            ImageInputLimits::default(),
            1000,
            true,
            measure
        )
        .is_err()
    );
    assert_eq!(prepared[1..], unchanged[1..]);
    let mut suffix = unchanged;
    assert!(
        bound_request(
            &mut suffix,
            (&original).into(),
            ImageInputLimits::default(),
            initial - 1,
            false,
            measure
        )
        .is_err()
    );
    assert_eq!(suffix[0]["content"][1]["type"], "input_image");
}

enum Vision {
    All,
    User,
    None,
}
impl Model for Vision {
    fn supports_image_input(&self) -> bool {
        !matches!(self, Self::None)
    }
    fn supports_tool_image_input(&self) -> bool {
        matches!(self, Self::All)
    }
    fn respond<'a>(
        &'a self,
        _: ModelRequest<'a>,
        _: ModelEventSink,
    ) -> crate::BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async { Err(Error::Provider("unused transport".into())) })
    }
}

#[tokio::test]
async fn replay_replaces_deleted_history_but_keeps_fresh_and_invalid_images_strict() {
    let history = crate::SessionId::new("history").expect("session");
    let other = crate::SessionId::new("other").expect("session");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(8, 8)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("PNG");
    let bytes = bytes.into_inner();
    let old = store
        .ingest_image(&history, "old.png".into(), bytes.clone(), ImageDetail::High)
        .await
        .expect("old image");
    store
        .delete_session(&history)
        .await
        .expect("purge old image");
    let fresh = store
        .ingest_image(&history, "fresh.png".into(), bytes, ImageDetail::High)
        .await
        .expect("fresh image");
    let input = vec![
        serde_json::json!({"role":"user","content":[{"type":"input_image","image":old}]}),
        serde_json::json!({"type":"function_call","call_id":"capture"}),
        serde_json::json!({"type":"function_call_output","call_id":"capture", super::super::TOOL_ERROR_FIELD:true, "output":[{"type":"input_text","text":"failed capture"},{"type":"input_image","image":old, super::super::PROMPT_CACHE_BREAKPOINT_FIELD:true}]}),
        serde_json::json!({"role":"assistant","content":"observed"}),
        serde_json::json!({"role":"user","content":[{"type":"input_text","text":"new image"},{"type":"input_image","image":fresh}]}),
    ];
    let media = MediaPreparation {
        files: Some(&store),
        limits: ImageInputLimits::default(),
    };
    for model in [Vision::None, Vision::User] {
        let prepared = hydrate(
            None,
            &history,
            (&input[2..3]).into(),
            &model,
            0,
            0,
            usize::MAX,
        )
        .await
        .expect("unsupported tool images do not require stored pixels");
        assert!(
            prepared[0]["output"][1]["text"]
                .as_str()
                .is_some_and(|text| text.contains(&old.file.id))
        );
    }
    assert!(
        media
            .prepare(None, (&input).into(), &Vision::All, true)
            .await
            .is_err()
    );
    assert!(
        media
            .prepare(None, (&[]).into(), &Vision::All, true)
            .await
            .unwrap()
            .is_none()
    );
    let prepared = media
        .prepare(Some(&history), (&input).into(), &Vision::All, true)
        .await
        .expect("cold replay after purge")
        .expect("image materialization");
    for part in [&prepared[0]["content"][0], &prepared[2]["output"][1]] {
        assert_eq!(part["type"], "input_text");
        assert!(
            part["text"]
                .as_str()
                .expect("unavailable")
                .contains("unavailable")
        );
    }
    assert!(super::super::has_prompt_cache_breakpoint(
        (&prepared).into()
    ));
    assert_eq!(prepared[2]["output"][0]["text"], "failed capture");
    assert_eq!(prepared[4]["content"][1]["type"], "input_image");
    assert_eq!(input[0]["content"][0]["type"], "input_image");
    assert_eq!(input[2]["output"][1]["type"], "input_image");
    assert!(
        media
            .prepare(Some(&history), (&input).into(), &Vision::All, false)
            .await
            .is_err()
    );
    assert!(
        media
            .prepare(Some(&other), (&input).into(), &Vision::All, true)
            .await
            .is_err()
    );

    let mut continued = input[..4].to_vec();
    continued.push(serde_json::json!({"role":"user","content":"continue without images"}));
    media
        .prepare(Some(&history), (&continued).into(), &Vision::All, true)
        .await
        .expect("purged prior batch does not poison later text turns");
    continued
        .push(serde_json::json!({"role":"user","content":[{"type":"input_image","image":old}]}));
    assert!(
        media
            .prepare(Some(&history), (&continued).into(), &Vision::All, true)
            .await
            .is_err()
    );

    let mut invalid = input.clone();
    invalid[0]["content"][0]["image"] = serde_json::to_value(fresh).expect("image reference");
    invalid[0]["content"][0]["image"]["file"]["size"] = serde_json::json!(1);
    assert!(matches!(
        media
            .prepare(Some(&history), (&invalid).into(), &Vision::All, true)
            .await,
        Err(Error::Tool(_))
    ));
}

#[tokio::test]
async fn images_past_the_cap_are_omitted_before_any_read_and_the_newest_batch_is_kept() {
    let session = crate::SessionId::new("capped").expect("session");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(8, 8)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("PNG");
    let bytes = bytes.into_inner();
    let ingest = async |name: &str| {
        store
            .ingest_image(&session, name.into(), bytes.clone(), ImageDetail::High)
            .await
            .expect("image")
    };
    let user_image = |image: &ImageReference| serde_json::json!({"role":"user","content":[{"type":"input_image","image":image}]});
    // A purged file reports "unavailable" only if something tried to read it.
    let purged = ingest("purged.png").await;
    store.delete_session(&session).await.expect("purge");
    let kept = ingest("kept.png").await;
    let answer = serde_json::json!({"role":"assistant","content":"seen"});
    let mut input = Vec::new();
    for image in std::iter::repeat_n(&purged, 36).chain(std::iter::repeat_n(&kept, 64)) {
        input.extend([user_image(image), answer.clone()]);
    }
    let media = MediaPreparation {
        files: Some(&store),
        limits: ImageInputLimits::default(),
    };
    let replay = async |input: &Vec<Value>, media: MediaPreparation<'_>| {
        media
            .prepare(Some(&session), input.into(), &Vision::All, true)
            .await
            .expect("replay")
            .expect("images")
    };
    let prepared = replay(&input, media).await;
    for item in prepared[..72].iter().step_by(2) {
        let omitted = item["content"][0]["text"].as_str().expect("omission");
        assert!(omitted.contains("omitted"), "{omitted}");
    }
    for item in prepared[72..].iter().step_by(2) {
        assert_eq!(item["content"][0]["type"], "input_image");
    }

    // An answered newest batch stays whole; only the older image yields.
    let input = vec![
        user_image(&kept),
        serde_json::json!({"type":"function_call","call_id":"capture"}),
        serde_json::json!({"type":"function_call_output","call_id":"capture","output":[
                {"type":"input_image","image":kept},{"type":"input_image","image":kept}]}),
        answer,
    ];
    let media = MediaPreparation {
        limits: ImageInputLimits {
            max_images: 1,
            ..ImageInputLimits::default()
        },
        ..media
    };
    let prepared = replay(&input, media).await;
    assert_eq!(prepared[0]["content"][0]["type"], "input_text");
    assert_eq!(prepared[2]["output"][0]["type"], "input_image");
    assert_eq!(prepared[2]["output"][1]["type"], "input_image");
}

#[tokio::test]
async fn large_native_observations_keep_exact_prefix_and_durable_file_references() {
    use image::ImageEncoder as _;
    let parent = crate::SessionId::new("parent").expect("session");
    let child = crate::SessionId::new("child").expect("session");
    let other = crate::SessionId::new("other").expect("session");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    let mut random = 1_u32;
    let pixels = (0..1800 * 1800 * 3)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            random.to_le_bytes()[0]
        })
        .collect::<Vec<_>>();
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&pixels, 1800, 1800, image::ExtendedColorType::Rgb8)
        .expect("PNG");
    assert!(bytes.len() > 8 * 1024 * 1024);
    let observation = store
        .ingest_image(&parent, "screen.png".into(), bytes, ImageDetail::High)
        .await
        .expect("image");
    let content = ToolContent(vec![
        ContentPart::Text {
            text: "before".into(),
        },
        ContentPart::Image {
            image: observation.clone(),
        },
        ContentPart::Text {
            text: "after".into(),
        },
        ContentPart::Image {
            image: observation.clone(),
        },
    ]);
    let mut input = vec![Arc::new(super::super::tool_output("call", &content, true))];
    super::super::reset_shared_prompt_cache_breakpoint(&mut input);
    input.push(Arc::new(super::super::user_message(
        "retained text beside the image",
    )));
    let prepared = hydrate(
        Some(&store),
        &parent,
        (&input).into(),
        &Vision::All,
        0,
        0,
        usize::MAX,
    )
    .await
    .expect("large images admitted");
    assert!(Arc::ptr_eq(&prepared[1], &input[1]));
    assert!(!Arc::ptr_eq(&prepared[0], &input[0]));
    assert!(input[0]["output"][1].get("image").is_some());
    assert_eq!(prepared[0]["output"][0]["text"], "before");
    assert_eq!(prepared[0]["output"][2]["text"], "after");
    assert_eq!(prepared[0]["output"][1]["detail"], "high");
    assert!(super::super::has_prompt_cache_breakpoint(
        (&prepared).into()
    ));
    let wired = serde_json::to_value(
        super::super::responses_wire::WireInput::new((&prepared).into(), true, "catalog", &[])
            .expect("native result"),
    )
    .expect("wire JSON");
    assert!(
        wired[0]["output"][1]["image_url"]
            .as_str()
            .expect("image url")
            .starts_with("data:image/jpeg;base64,")
    );
    input.push(Arc::new(super::super::tool_output(
        "next",
        &ToolContent(vec![ContentPart::Image {
            image: observation.clone(),
        }]),
        false,
    )));
    let appended = hydrate(
        Some(&store),
        &parent,
        (&input).into(),
        &Vision::All,
        0,
        0,
        usize::MAX,
    )
    .await
    .expect("append");
    assert_eq!(prepared, appended[..2]);
    assert!(
        hydrate(
            Some(&store),
            &other,
            (&input).into(),
            &Vision::All,
            0,
            0,
            usize::MAX
        )
        .await
        .is_err()
    );
    crate::backend::session_files::grant_context(Some(&store), &parent, &child, &input)
        .await
        .expect("fork grants");
    let published = store
        .publish_reference(&parent, &observation.file)
        .await
        .expect("publish existing observation");
    assert_eq!(
        store
            .read_file(&parent, &published)
            .await
            .expect("published bytes"),
        store
            .read_file(&parent, &observation.file)
            .await
            .expect("original bytes")
    );
    store.delete_session(&parent).await.expect("delete parent");
    assert_eq!(
        hydrate(
            Some(&store),
            &child,
            (&input).into(),
            &Vision::All,
            0,
            0,
            usize::MAX
        )
        .await
        .expect("fork retains blobs"),
        appended
    );
}
