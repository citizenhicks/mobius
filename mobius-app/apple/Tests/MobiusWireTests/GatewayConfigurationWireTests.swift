import Foundation
import XCTest

extension GatewayWireTests {
    func testExtensionLifecycleRequestsMatchV45() throws {
        let install = try requestObject(.installExtension(
            requestID: "extension-1",
            source: "https://github.com/DietrichGebert/ponytail.git",
            reference: "main",
            subdirectory: "packages/ponytail"
        ))
        XCTAssertEqual(install["type"] as? String, "install_extension")
        XCTAssertEqual(install["request_id"] as? String, "extension-1")
        XCTAssertEqual(install["reference"] as? String, "main")
        XCTAssertEqual(install["subdirectory"] as? String, "packages/ponytail")

        let update = try requestObject(.updateExtension(
            requestID: "extension-2",
            id: "plugin:ponytail"
        ))
        XCTAssertEqual(update["type"] as? String, "update_extension")
        XCTAssertEqual(update["id"] as? String, "plugin:ponytail")

        let uninstall = try requestObject(.uninstallExtension(
            requestID: "extension-3",
            id: "plugin:ponytail"
        ))
        XCTAssertEqual(uninstall["type"] as? String, "uninstall_extension")

        let trust = try requestObject(.trustExtensionHooks(
            requestID: "extension-4",
            id: "plugin:ponytail",
            expectedDigest: "abcdef0123456789"
        ))
        XCTAssertEqual(trust["type"] as? String, "trust_extension_hooks")
        XCTAssertEqual(trust["expected_digest"] as? String, "abcdef0123456789")

        let untrust = try requestObject(.revokeExtensionHooksTrust(
            requestID: "extension-5",
            id: "plugin:ponytail",
            expectedDigest: "abcdef0123456789"
        ))
        XCTAssertEqual(untrust["type"] as? String, "revoke_extension_hooks_trust")
        XCTAssertEqual(untrust["expected_digest"] as? String, "abcdef0123456789")
    }

    func testProviderAndUtilityRequestsMatchV28() throws {
        let credential = try requestObject(.setProviderCredential(
            requestID: "credential-1",
            instance: "openai-work",
            provider: "openai_socket",
            apiKey: "secret"
        ))
        XCTAssertEqual(credential["type"] as? String, "set_provider_credential")
        XCTAssertEqual(credential["api_key"] as? String, "secret")

        let endpointCredential = try requestObject(.setProviderEndpointCredential(
            requestID: "endpoint-1",
            instance: "responses-local",
            provider: "openai_compatible",
            baseURL: "https://models.example/v1",
            apiKey: "secret"
        ))
        XCTAssertEqual(endpointCredential["base_url"] as? String, "https://models.example/v1")

        let registered = try requestObject(.registerProvider(
            requestID: "register-1",
            config: composition.provider,
            label: "Work",
            tint: .purple,
            modelIds: ["gpt-5.6-sol", "gpt-5.6-mini"],
            reasoningEfforts: ["medium", "high"]
        ))
        let provider = try XCTUnwrap(registered["config"] as? [String: Any])
        XCTAssertEqual(provider["endpoint_auth"] as? String, "provider_default")
        XCTAssertEqual(provider["reasoning_effort"] as? String, "high")
        XCTAssertNil(provider["api_key_env"])
        XCTAssertEqual(registered["model_ids"] as? [String], ["gpt-5.6-sol", "gpt-5.6-mini"])
        XCTAssertEqual(registered["reasoning_efforts"] as? [String], ["medium", "high"])
        XCTAssertEqual(registered["replace_existing_selections"] as? Bool, false)
        XCTAssertEqual(registered["label"] as? String, "Work")
        XCTAssertEqual(registered["tint"] as? String, "purple")

        let directory = try requestObject(.createWorkspaceDirectory(
            requestID: "create-directory-1",
            parent: "/srv",
            name: "New Project"
        ))
        XCTAssertEqual(directory["type"] as? String, "create_workspace_directory")
        XCTAssertEqual(directory["parent"] as? String, "/srv")
        XCTAssertEqual(directory["name"] as? String, "New Project")

        let requests: [(GatewayRequest, String)] = [
            (.listDirectories(requestID: "directories-1", path: "/srv", includeFiles: true), "list_directories"),
            (.createPairingCode(requestID: "pairing-1"), "create_pairing_code"),
            (.startProviderLogin(requestID: "login-1", provider: "openai_codex"), "start_provider_login"),
            (.getProfile(requestID: "profile-1"), "get_profile")
        ]
        for (request, type) in requests {
            XCTAssertEqual(try requestObject(request)["type"] as? String, type)
        }
    }

    func testGitCredentialRequestsUseOneExactTarget() throws {
        let probe = try requestObject(.probeGitCredential(
            requestID: "git-credential-1",
            target: "https://git.example.com/team/repo"
        ))
        XCTAssertEqual(probe["type"] as? String, "probe_git_credential")
        XCTAssertEqual(probe["target"] as? String, "https://git.example.com/team/repo")
        XCTAssertNil(probe["username"])
        XCTAssertNil(probe["token"])

        let approve = try requestObject(.approveGitCredential(
            requestID: "git-credential-2",
            target: "git.example.com",
            username: "octo",
            token: "secret"
        ))
        XCTAssertEqual(approve["type"] as? String, "approve_git_credential")
        XCTAssertEqual(approve["username"] as? String, "octo")
        XCTAssertEqual(approve["token"] as? String, "secret")

        let listSSH = try requestObject(.listSshIdentities(requestID: "ssh-list-1"))
        XCTAssertEqual(listSSH["type"] as? String, "list_ssh_identities")
        XCTAssertEqual(listSSH["request_id"] as? String, "ssh-list-1")

        let generateSSH = try requestObject(.generateSshIdentity(requestID: "ssh-generate-1"))
        XCTAssertEqual(generateSSH["type"] as? String, "generate_ssh_identity")
        XCTAssertEqual(generateSSH["request_id"] as? String, "ssh-generate-1")
    }

    func testRemoveProviderRequestUsesInstanceIdentity() throws {
        let request = try requestObject(.removeProvider(
            requestID: "remove-provider-1",
            instance: "openai-work"
        ))

        XCTAssertEqual(request["type"] as? String, "remove_provider")
        XCTAssertEqual(request["request_id"] as? String, "remove-provider-1")
        XCTAssertEqual(request["instance"] as? String, "openai-work")
    }

    func testConfigureDefaultAgentUsesDefaultRevisionWithoutSessionScope() throws {
        let request = try requestObject(.configureDefaultAgent(
            requestID: "default-1",
            expectedRevision: 4,
            config: composition
        ))

        XCTAssertEqual(request["type"] as? String, "configure_default_agent")
        XCTAssertEqual(request["request_id"] as? String, "default-1")
        XCTAssertEqual(request["expected_revision"] as? Int, 4)
        XCTAssertNil(request["session_id"])
        let config = try XCTUnwrap(request["config"] as? [String: Any])
        XCTAssertEqual(config["max_model_steps"] as? Int, 256)
        XCTAssertEqual(config["extensions"] as? [String], ["plugin:ponytail"])
        let middleware = try XCTUnwrap(config["middleware"] as? [String: Any])
        let settings = try XCTUnwrap(middleware["settings"] as? [String: Any])
        let subagents = try XCTUnwrap(settings["subagents"] as? [String: Any])
        XCTAssertEqual(subagents["model_route"] as? String, "openai_socket/gpt-5.6-sol")

        var inherited = composition
        inherited.middleware.setSetting(nil, middleware: "subagents", setting: "model_route")
        let inheritedRequest = try requestObject(.configureDefaultAgent(
            requestID: "default-2",
            expectedRevision: 4,
            config: inherited
        ))
        let inheritedConfig = try XCTUnwrap(inheritedRequest["config"] as? [String: Any])
        let inheritedMiddleware = try XCTUnwrap(inheritedConfig["middleware"] as? [String: Any])
        let inheritedSettings = try XCTUnwrap(inheritedMiddleware["settings"] as? [String: Any])
        XCTAssertNil(inheritedSettings["subagents"])
    }


    func testSameRevisionRefreshPreservesProviderDraft() {
        let snapshot = VersionedAgentConfig(revision: 4, config: composition)
        var draft = composition
        draft.provider.provider = "openrouter"

        let refreshed = refreshedAgentDraft(
            currentDraft: draft,
            currentSnapshot: snapshot,
            incomingSnapshot: snapshot
        )

        XCTAssertEqual(refreshed.provider.provider, "openrouter")
    }
}
