use super::bots::gateway_with_bot;
use super::*;
use crate::wire::{
    BotAccess, CliHarnessConfig, HarnessConfig, HarnessKind, RoutineInteractionPolicy,
};

#[tokio::test]
async fn a_harness_without_an_adapter_is_unavailable_but_its_bots_still_load() {
    let (root, gateway, mobius_bot) = gateway_with_bot().await;
    assert_eq!(mobius_bot.harness, HarnessKind::Mobius);
    let options = CliHarnessConfig {
        model: None,
        settings: Default::default(),
        access: BotAccess::FullAccess,
    };
    let ready = gateway
        .configure_harness(HarnessKind::Codex, None, options.clone())
        .await
        .unwrap_err();
    assert_eq!(ready.code, "harness_unavailable");
    let rejection = gateway
        .create_bot(HarnessKind::Claude, "Reviewer", "Reviews code.")
        .await
        .unwrap_err();
    assert_eq!(rejection.code, "harness_unavailable");
    let descriptors = gateway.refresh_harnesses().await.unwrap().harnesses;
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].kind, HarnessKind::Mobius);

    // A Bot saved by a gateway that had the adapter loads, but cannot run.
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let codex = bots
        .create_bot(
            "Coder",
            "Writes code.",
            HarnessConfig::Codex(options).into(),
        )
        .unwrap();
    assert_eq!(codex.harness, HarnessKind::Codex);
    assert_eq!(
        codex.routine_interaction_policy,
        RoutineInteractionPolicy::Unattended
    );
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let host = gateway
        .create_session(&workspace, &mobius_bot.id)
        .await
        .unwrap();
    let rejection = gateway
        .reassign_session(host.session_id(), &codex.id)
        .await
        .unwrap_err();
    assert_eq!(rejection.code, "harness_unavailable");
    assert_eq!(host.bot_id().await.unwrap(), mobius_bot.id);
    let rejection = gateway
        .create_session(&workspace, &codex.id)
        .await
        .err()
        .unwrap();
    assert_eq!(rejection.code, "harness_unavailable");
}
