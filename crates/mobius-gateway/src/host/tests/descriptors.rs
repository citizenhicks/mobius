//! Resident chats must not multiply handles to identical skill directories.

use super::*;

const CHILD: &str = "MOBIUS_GATEWAY_DESCRIPTOR_TEST_CHILD";
const TEST: &str = "host::tests::descriptors::resident_chats_share_pinned_skill_handles";

#[tokio::test]
async fn resident_chats_share_pinned_skill_handles() {
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", "ulimit -n 256; exec \"$@\"", "descriptor-test"])
            .arg(std::env::current_exe().expect("test executable"))
            .args([TEST, "--exact", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("run isolated descriptor test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{stdout}");
        return;
    }
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    for index in 0..32 {
        let skill = workspace.join(format!(".agents/skills/skill-{index}"));
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(skill.join("SKILL.md"), format!(
            "---\nname: skill-{index}\ndescription: Descriptor regression fixture.\n---\nRead files.\n"
        )).expect("skill");
    }
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("gateway");
    let mut composition = ensure_test_bot(&gateway)
        .await
        .expect("default Bot")
        .config
        .config;
    composition.middleware.set_enabled("extensions", true);
    let bot = bots
        .create_bot("Skills", "Descriptor regression", composition)
        .expect("Bot");
    let mut sessions = Vec::new();
    let mut first_count = 0;
    for index in 1..=MAX_ACTIVE_SESSIONS {
        sessions.push(
            gateway
                .create_session(&workspace, &bot.id)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "chat {index} failed with {} descriptors: {error:?}",
                        descriptors()
                    )
                }),
        );
        let count = descriptors();
        if index == 1 {
            first_count = count;
        }
        if [1, 6, MAX_ACTIVE_SESSIONS].contains(&index) {
            println!("resident chats={index}, descriptors={count}");
        }
    }
    assert!(
        descriptors() <= first_count + 2 * (MAX_ACTIVE_SESSIONS - 1),
        "resident chats duplicated shared directory handles"
    );
    drop(sessions);
    gateway.shutdown().await;
}

fn descriptors() -> usize {
    std::fs::read_dir("/dev/fd")
        .expect("descriptor inventory")
        .count()
}
