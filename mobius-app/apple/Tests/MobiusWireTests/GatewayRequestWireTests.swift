import Foundation
import XCTest

extension GatewayWireTests {
    func testGlobalScratchpadRequestAndResponseAreGatewayScoped() throws {
        let request = try requestObject(.submitGlobalScratchpad(
            requestID: "scratchpad-1",
            operation: .capabilityCommand(
                capability: "scratchpad",
                command: "scratchpad",
                arguments: "refresh",
                input: nil,
                target: nil
            )
        ))
        XCTAssertEqual(request["type"] as? String, "submit_global_scratchpad")
        XCTAssertNil(request["session_id"])
        XCTAssertEqual(
            (request["operation"] as? [String: Any])?["arguments"] as? String,
            "refresh"
        )

        let response = try decodeEnvelope(
            #"{"version":48,"type":"global_scratchpad_changed","request_id":"scratchpad-1","contribution":{"capability":"scratchpad","accepts_file_attachments":false,"count":0,"commands":[],"widgets":[],"references":[]}}"#
        )
        guard case .globalScratchpadChanged(let requestID, let contribution) = response else {
            return XCTFail("Expected a global scratchpad response")
        }
        XCTAssertEqual(requestID, "scratchpad-1")
        XCTAssertEqual(contribution.capability, "scratchpad")
    }

    func testSessionCatalogRequestsMatchV28() throws {
        let list = try requestObject(.listSessions(requestID: "list-1"))
        XCTAssertEqual(list["type"] as? String, "list_sessions")
        XCTAssertEqual(list["request_id"] as? String, "list-1")

        let create = try requestObject(.createSession(
            requestID: "create-1",
            workspace: "/srv/mobius"
        ))
        XCTAssertEqual(create["type"] as? String, "create_session")
        XCTAssertEqual(create["workspace"] as? String, "/srv/mobius")

        let open = try requestObject(.openSession(
            requestID: "open-1",
            sessionID: "chat-1",
            lastSequence: 41
        ))
        XCTAssertEqual(open["type"] as? String, "open_session")
        XCTAssertEqual(open["session_id"] as? String, "chat-1")
        XCTAssertEqual(open["last_sequence"] as? Int, 41)
        XCTAssertNil(open["replay_epoch"])

        let freshOpen = try requestObject(.openSession(
            requestID: "open-2",
            sessionID: "chat-1",
            lastSequence: nil
        ))
        XCTAssertTrue(freshOpen["last_sequence"] is NSNull)
        XCTAssertNil(freshOpen["replay_epoch"])
    }

    func testSwarmManagementRequestsEncodeGatewayOwnedIdentityInputs() throws {
        let create = try requestObject(.createSwarm(
            requestID: "swarm-create-1",
            leaderSessionID: "chat-1",
            memberSessionIDs: ["chat-1", "chat-2", "chat-3"]
        ))
        XCTAssertEqual(create["type"] as? String, "create_swarm")
        XCTAssertEqual(create["request_id"] as? String, "swarm-create-1")
        XCTAssertEqual(create["leader_session_id"] as? String, "chat-1")
        XCTAssertEqual(
            create["member_session_ids"] as? [String],
            ["chat-1", "chat-2", "chat-3"]
        )
        XCTAssertNil(create["title"])
        XCTAssertNil(create["handles"])

        let add = try requestObject(.addSwarmMember(
            requestID: "swarm-add-1",
            swarmID: "swarm-1",
            sessionID: "chat-4"
        ))
        XCTAssertEqual(add["type"] as? String, "add_swarm_member")
        XCTAssertEqual(add["swarm_id"] as? String, "swarm-1")
        XCTAssertEqual(add["session_id"] as? String, "chat-4")

        let leave = try requestObject(.leaveSwarm(
            requestID: "swarm-leave-1",
            swarmID: "swarm-1",
            sessionID: "chat-4"
        ))
        XCTAssertEqual(leave["type"] as? String, "leave_swarm")
        XCTAssertEqual(leave["session_id"] as? String, "chat-4")

        let disband = try requestObject(.disbandSwarm(
            requestID: "swarm-disband-1",
            swarmID: "swarm-1"
        ))
        XCTAssertEqual(disband["type"] as? String, "disband_swarm")
        XCTAssertEqual(disband["swarm_id"] as? String, "swarm-1")
        XCTAssertNil(disband["session_id"])
    }

    func testSessionScopedRequestsEncodeSessionID() throws {
        let submission = Submission(
            id: "input-1",
            op: .message(MessageSubmission(
                author: .user,
                text: "Hello",
                attachments: [],
                requestedDelivery: nil,
                targetTurnId: nil
            ))
        )
        let requests: [(GatewayRequest, String)] = [
            (.renameSession(requestID: "rename-1", sessionID: "chat-1", title: "Review"), "rename_session"),
            (.setSessionPinned(requestID: "pin-1", sessionID: "chat-1", pinned: true), "set_session_pinned"),
            (.deleteSession(requestID: "delete-1", sessionID: "chat-1"), "delete_session"),
            (.submit(sessionID: "chat-1", submission: submission), "submit"),
            (.configureSession(
                requestID: "config-1",
                sessionID: "chat-1",
                expectedRevision: 4,
                config: composition
            ), "configure_session"),
            (.getGitDiff(
                requestID: "diff-1",
                sessionID: "chat-1",
                scope: .unstaged
            ), "get_git_diff"),
            (.switchGitBranch(requestID: "branch-1", sessionID: "chat-1", branch: "feature"), "switch_git_branch"),
            (.getSessionHistory(
                requestID: "history-1",
                sessionID: "chat-1",
                beforeSequence: 40
            ), "get_session_history"),

        ]

        for (request, type) in requests {
            let object = try requestObject(request)
            XCTAssertEqual(object["type"] as? String, type)
            XCTAssertEqual(object["session_id"] as? String, "chat-1")
        }

        let configure = try requestObject(.configureSession(
            requestID: "config-1",
            sessionID: "chat-1",
            expectedRevision: 4,
            config: composition
        ))
        let encodedConfig = try XCTUnwrap(configure["config"] as? [String: Any])
        XCTAssertNil(encodedConfig["approval"])
        XCTAssertEqual(encodedConfig["max_model_steps"] as? Int, 256)
        let middleware = try XCTUnwrap(encodedConfig["middleware"] as? [String: Any])
        let enabled = try XCTUnwrap(middleware["enabled"] as? [String])
        XCTAssertEqual(Set(enabled), ["extensions", "subagents"])
        XCTAssertEqual(encodedConfig["extensions"] as? [String], ["plugin:ponytail"])
        let settings = try XCTUnwrap(middleware["settings"] as? [String: Any])
        let subagents = try XCTUnwrap(settings["subagents"] as? [String: Any])
        XCTAssertEqual(subagents["model_route"] as? String, "openai_socket/gpt-5.6-sol")

        let branch = try requestObject(.switchGitBranch(
            requestID: "branch-2",
            sessionID: "chat-1",
            branch: "feature"
        ))
        XCTAssertEqual(branch["branch"] as? String, "feature")

        let schedule = CronSchedule.interval(seconds: 3_700)
        let create = try requestObject(.createCron(
            requestID: "cron-1",
            sourceSessionID: "chat-1",
            task: "Review nightly",
            schedule: schedule,
            endsAt: nil
        ))
        XCTAssertEqual(create["type"] as? String, "create_cron")
        XCTAssertEqual(create["source_session_id"] as? String, "chat-1")
        XCTAssertEqual((create["schedule"] as? [String: Any])?["kind"] as? String, "interval")
        XCTAssertEqual((create["schedule"] as? [String: Any])?["every_seconds"] as? Int, 3_700)

        let update = try requestObject(.updateCron(
            requestID: "cron-2",
            id: "task-1",
            sourceSessionID: "chat-1",
            task: "Review nightly",
            schedule: .cron("0 9 * * *", timeZone: "America/New_York"),
            endsAt: 500,
            enabled: false
        ))
        XCTAssertEqual(update["type"] as? String, "update_cron")
        XCTAssertEqual(update["id"] as? String, "task-1")
        XCTAssertEqual(update["enabled"] as? Bool, false)
        XCTAssertEqual(
            (update["schedule"] as? [String: Any])?["time_zone"] as? String,
            "America/New_York"
        )

        let list = try requestObject(.listCron(requestID: "cron-3"))
        XCTAssertNil(list["session_id"])
        let history = try requestObject(.listCronHistory(requestID: "cron-4", id: nil))
        XCTAssertTrue(history["id"] is NSNull)

        let preview = try requestObject(.getCronRunPreview(
            requestID: "cron-5",
            id: "run-1",
            beforeSequence: 12
        ))
        XCTAssertEqual(preview["type"] as? String, "get_cron_run_preview")
        XCTAssertEqual(preview["before_sequence"] as? Int, 12)
    }

    func testSessionFileRequestsMatchV28() throws {
        let file = SessionFileReference(
            id: "file-1",
            name: "scan.png",
            size: 3,
            mediaType: "image/png"
        )
        let submit = try requestObject(.submit(
            sessionID: "chat-1",
            submission: Submission(
                id: "input-1",
                op: .message(MessageSubmission(
                    author: .user,
                    text: "Review this",
                    attachments: [file],
                    requestedDelivery: nil,
                    targetTurnId: nil
                ))
            )
        ))
        let submission = try XCTUnwrap(submit["submission"] as? [String: Any])
        let operation = try XCTUnwrap(submission["op"] as? [String: Any])
        XCTAssertEqual(operation["type"] as? String, "message")
        let message = try XCTUnwrap(operation["message"] as? [String: Any])
        let attachments = try XCTUnwrap(message["attachments"] as? [[String: Any]])
        XCTAssertEqual(attachments.first?["media_type"] as? String, "image/png")

        let begin = try requestObject(.beginSessionFileUpload(
            requestID: "begin-1",
            sessionID: "chat-1",
            name: "scan.png",
            size: 3,
            mediaType: "image/png"
        ))
        XCTAssertEqual(begin["type"] as? String, "begin_session_file_upload")
        XCTAssertEqual(begin["media_type"] as? String, "image/png")

        let append = try requestObject(.uploadSessionFileChunk(
            requestID: "chunk-1",
            sessionID: "chat-1",
            uploadID: "upload-1",
            offset: 0,
            data: Data([1, 2, 3])
        ))
        XCTAssertEqual(append["type"] as? String, "upload_session_file_chunk")
        XCTAssertEqual(append["data"] as? String, "AQID")

        let finish = try requestObject(.finishSessionFileUpload(
            requestID: "finish-1",
            sessionID: "chat-1",
            uploadID: "upload-1"
        ))
        XCTAssertEqual(finish["type"] as? String, "finish_session_file_upload")

        let list = try requestObject(.listSessionFiles(
            requestID: "list-1",
            sessionID: "chat-1"
        ))
        XCTAssertEqual(list["type"] as? String, "list_session_files")

        let read = try requestObject(.readSessionFile(
            requestID: "read-1",
            sessionID: "chat-1",
            fileID: "file-1",
            offset: 2,
            maxBytes: 262_144
        ))
        XCTAssertEqual(read["type"] as? String, "read_session_file")
        XCTAssertEqual(read["file_id"] as? String, "file-1")
        XCTAssertEqual(read["max_bytes"] as? Int, 262_144)
    }

    func testMessageOperationEncodesOneTypedPayload() throws {
        let request = try requestObject(.submit(
            sessionID: "chat-1",
            submission: Submission(
                id: "input-1",
                op: .message(MessageSubmission(
                    author: .user,
                    text: "Use the smaller patch",
                    attachments: [],
                    requestedDelivery: .queue,
                    targetTurnId: "turn-1"
                ))
            )
        ))

        let submission = try XCTUnwrap(request["submission"] as? [String: Any])
        let operation = try XCTUnwrap(submission["op"] as? [String: Any])
        XCTAssertEqual(operation["type"] as? String, "message")
        XCTAssertNil(operation["text"])
        let message = try XCTUnwrap(operation["message"] as? [String: Any])
        XCTAssertEqual((message["author"] as? [String: Any])?["type"] as? String, "user")
        XCTAssertEqual(message["text"] as? String, "Use the smaller patch")
        XCTAssertEqual(message["requested_delivery"] as? String, "queue")
        XCTAssertEqual(message["target_turn_id"] as? String, "turn-1")

        let decoded = try decoder().decode(
            AgentOperation.self,
            from: try encoder().encode(AgentOperation.message(MessageSubmission(
                author: .user,
                text: "Use the smaller patch",
                attachments: [],
                requestedDelivery: .queue,
                targetTurnId: "turn-1"
            )))
        )
        guard case .message(let payload) = decoded else {
            return XCTFail("Expected a message operation")
        }
        XCTAssertEqual(payload.author, .user)
        XCTAssertEqual(payload.requestedDelivery, .queue)
        XCTAssertEqual(payload.targetTurnId, "turn-1")

        for fixture in [
            #"{"type":"message","message":{"author":{"type":"peer","message_id":"peer-1","session_id":"chat-2"},"text":"Review","attachments":[],"requested_delivery":"steer","target_turn_id":"turn-1"}}"#,
            #"{"type":"message","message":{"author":{"type":"user"},"text":"Review","attachments":[],"requested_delivery":"later","target_turn_id":"turn-1"}}"#,
            #"{"type":"message","message":{"author":{"type":"user"},"text":"Review","attachments":[],"requested_delivery":"steer","target_turn_id":""}}"#,
            #"{"type":"message","message":{"author":{"type":"user"},"text":"Review","attachments":[],"target_turn_id":null}}"#,
            #"{"type":"message","message":{"author":{"type":"user"},"text":"Review","attachments":[],"requested_delivery":null}}"#,
        ] {
            XCTAssertThrowsError(
                try decoder().decode(AgentOperation.self, from: Data(fixture.utf8))
            )
        }
    }

    func testWorkspaceViewerRequestsMatchV28() throws {
        let diff = try requestObject(.getGitDiff(
            requestID: "diff-1",
            sessionID: "chat-1",
            scope: .committed
        ))
        XCTAssertEqual(diff["scope"] as? String, "committed")

        let list = try requestObject(.listWorkspaceFiles(
            requestID: "files-1",
            sessionID: "chat-1",
            scope: .modified
        ))
        XCTAssertEqual(list["type"] as? String, "list_workspace_files")
        XCTAssertEqual(list["scope"] as? String, "modified")

        let read = try requestObject(.readWorkspaceFile(
            requestID: "read-1",
            sessionID: "chat-1",
            path: "Sources/App.swift",
            offset: 4,
            maxBytes: 262_144
        ))
        XCTAssertEqual(read["type"] as? String, "read_workspace_file")
        XCTAssertEqual(read["path"] as? String, "Sources/App.swift")
        XCTAssertEqual(read["offset"] as? Int, 4)

        let write = try requestObject(.writeWorkspaceFile(
            requestID: "write-1",
            sessionID: "chat-1",
            path: ".env",
            content: "TOKEN=secret\n"
        ))
        XCTAssertEqual(write["type"] as? String, "write_workspace_file")
        XCTAssertEqual(write["session_id"] as? String, "chat-1")
        XCTAssertEqual(write["path"] as? String, ".env")
        XCTAssertEqual(write["content"] as? String, "TOKEN=secret\n")
    }

}
