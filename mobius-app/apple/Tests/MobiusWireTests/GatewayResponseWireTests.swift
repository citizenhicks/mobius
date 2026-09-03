import Foundation
import XCTest

extension GatewayWireTests {
    func testV28MessageTargetsAreStrict() throws {
        let operation = try decoder().decode(
            AgentOperation.self,
            from: Data(
                #"{"type":"capability_command","capability":"sessions","command":"fork","arguments":"","input":null,"target":{"checkpoint_sequence":12,"batch_item_count":3}}"#.utf8
            )
        )
        guard case .capabilityCommand(_, _, _, let input, let target) = operation else {
            return XCTFail("Expected a capability command")
        }
        XCTAssertNil(input)
        XCTAssertEqual(target, MessageTarget(checkpointSequence: 12, batchItemCount: 3))
        let request = try requestObject(.submit(
            sessionID: "chat-1",
            submission: Submission(id: "fork-1", op: operation)
        ))
        let submission = try XCTUnwrap(request["submission"] as? [String: Any])
        let encodedOperation = try XCTUnwrap(submission["op"] as? [String: Any])
        let encodedTarget = try XCTUnwrap(encodedOperation["target"] as? [String: Any])
        XCTAssertEqual(encodedTarget["checkpoint_sequence"] as? Int, 12)
        XCTAssertEqual(encodedTarget["batch_item_count"] as? Int, 3)

        for fixture in [
            #"{"type":"capability_command","capability":"sessions","command":"fork","arguments":"","input":null}"#,
            #"{"type":"capability_command","capability":"sessions","command":"fork","arguments":"","target":null}"#,
            #"{"type":"capability_command","capability":"sessions","command":"fork","arguments":"","input":null,"target":{"checkpoint_sequence":12,"batch_item_count":0}}"#
        ] {
            XCTAssertThrowsError(
                try decoder().decode(AgentOperation.self, from: Data(fixture.utf8))
            )
        }

        let untargeted = try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"message","author":{"type":"user"},"delivery":"turn","text":"Hello","attachments":[],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let record) = untargeted else {
            return XCTFail("Expected an agent event")
        }
        XCTAssertEqual(record.event.msg["messageTarget"], JSONValue.null)
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"message","author":{"type":"user"},"delivery":"turn","text":"Hello","attachments":[]}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        ))
    }

    func testMessageEventDecodesTypedAuthorAndActualDelivery() throws {
        let envelope = try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"submission_id":"swarm-message-1","msg":{"type":"message","author":{"type":"peer","message_id":"message-1","session_id":"chat-reviewer","handle":"@reviewer"},"delivery":"steer","text":"Check the parser boundary.","attachments":[],"message_target":{"checkpoint_sequence":12,"batch_item_count":2}}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let record) = envelope else {
            return XCTFail("Expected an agent event")
        }
        let message = try MessageEventPayload(json: record.event.msg)
        XCTAssertEqual(
            message.author,
            .peer(messageID: "message-1", sessionID: "chat-reviewer", handle: "@reviewer")
        )
        XCTAssertEqual(message.delivery, .steer)
        XCTAssertEqual(message.text, "Check the parser boundary.")
        XCTAssertEqual(
            message.messageTarget,
            MessageTarget(checkpointSequence: 12, batchItemCount: 2)
        )
    }

    func testSessionFileResponsesMatchV28() throws {
        let record = #"{"id":"file-1","name":"scan.png","size":3,"media_type":"image/png"}"#

        let started = try decodeEnvelope(#"{"version":27,"type":"session_file_upload_ready","request_id":"begin-1","session_id":"chat-1","upload_id":"upload-1","max_chunk_bytes":262144}"#)
        guard case .sessionFileUploadReady(let requestID, let sessionID, let uploadID, let limit) = started else {
            return XCTFail("Expected session file upload start")
        }
        XCTAssertEqual([requestID, sessionID, uploadID], ["begin-1", "chat-1", "upload-1"])
        XCTAssertEqual(limit, 262_144)

        let accepted = try decodeEnvelope(#"{"version":27,"type":"session_file_upload_chunk_accepted","request_id":"chunk-1","session_id":"chat-1","upload_id":"upload-1","next_offset":3}"#)
        guard case .sessionFileUploadChunkAccepted(_, _, _, let nextOffset) = accepted else {
            return XCTFail("Expected session file chunk acknowledgement")
        }
        XCTAssertEqual(nextOffset, 3)

        let uploaded = try decodeEnvelope(#"{"version":27,"type":"session_file_upload_completed","request_id":"finish-1","session_id":"chat-1","file":\#(record)}"#)
        guard case .sessionFileUploadCompleted(_, _, let file) = uploaded else {
            return XCTFail("Expected uploaded session file")
        }
        XCTAssertEqual(file.mediaType, "image/png")

        let listed = try decodeEnvelope(#"{"version":27,"type":"session_files","request_id":"list-1","session_id":"chat-1","files":[{"origin":"user","file":\#(record)},{"origin":"agent","file":\#(record)}]}"#)
        guard case .sessionFiles(_, _, let files) = listed else {
            return XCTFail("Expected session file list")
        }
        XCTAssertEqual(files.map(\.origin), [.user, .agent])

        let chunk = try decodeEnvelope(#"{"version":27,"type":"session_file_chunk","request_id":"read-1","session_id":"chat-1","file_id":"file-1","offset":0,"data":"AQID","next_offset":null}"#)
        guard case .sessionFileChunk(_, _, _, let offset, let data, let finalOffset) = chunk else {
            return XCTFail("Expected session file chunk")
        }
        XCTAssertEqual(offset, 0)
        XCTAssertEqual(data, Data([1, 2, 3]))
        XCTAssertNil(finalOffset)
    }

    func testFrontendBlockRequiresFilesInV28() {
        let fixture = #"{"id":null,"group":null,"update":"replace","state":"complete","role":"notice","title":"Done","text":"","symbol":null,"format":"plain_text","tone":"neutral"}"#

        XCTAssertThrowsError(
            try decoder().decode(FrontendBlock.self, from: Data(fixture.utf8))
        ) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("frontend block is missing a required field")
            )
        }
    }

    func testWorkspaceViewerResponsesMatchV28() throws {
        let files = try decodeEnvelope(#"{"version":27,"type":"workspace_files","request_id":"files-1","session_id":"chat-1","files":[{"path":"Sources/App.swift","size":3}],"truncated":true}"#)
        guard case .workspaceFiles(_, _, let records, let truncated) = files else {
            return XCTFail("Expected workspace files")
        }
        XCTAssertEqual(records.first, WorkspaceFileRecord(path: "Sources/App.swift", size: 3))
        XCTAssertTrue(truncated)

        let complete = try decodeEnvelope(#"{"version":27,"type":"workspace_files","request_id":"files-2","session_id":"chat-1","files":[{"path":"Sources/App.swift","size":3}]}"#)
        guard case .workspaceFiles(_, _, _, let completeTruncated) = complete else {
            return XCTFail("Expected complete workspace files")
        }
        XCTAssertFalse(completeTruncated)

        let chunk = try decodeEnvelope(#"{"version":27,"type":"workspace_file_chunk","request_id":"read-1","session_id":"chat-1","path":"Sources/App.swift","offset":0,"data":"AQID","next_offset":null}"#)
        guard case .workspaceFileChunk(_, _, let path, let offset, let data, let nextOffset) = chunk else {
            return XCTFail("Expected workspace file chunk")
        }
        XCTAssertEqual(path, "Sources/App.swift")
        XCTAssertEqual(offset, 0)
        XCTAssertEqual(data, Data([1, 2, 3]))
        XCTAssertNil(nextOffset)
    }

    func testMalformedKnownAgentEventIsRejected() {
        let fixtures = [
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"turn_aborted","turn_id":"turn-1"}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"assistant_message","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","content":[{"output_index":0,"part_index":0,"phase":"future_phase","text":"Working","annotations":[]}],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"message","author":{"type":"peer","message_id":"message-1","session_id":"chat-reviewer"},"delivery":"steer","text":"Review","attachments":[],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"message","author":{"type":"user"},"delivery":"later","text":"Review","attachments":[],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        ]

        for fixture in fixtures {
            XCTAssertThrowsError(try decodeEnvelope(fixture))
        }
    }

    func testLegacyMessageAndTurnEventsAreRejected() {
        let fixtures = [
            #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"message","author":{"type":"user"},"delivery":"turn","message":"Hello","attachments":[],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"task_started","turn_id":"turn-1"}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"task_complete","turn_id":"turn-1"}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"agent_message","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","message":"Done","phase":"final_answer","message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"agent_message_content_delta","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","delta":"Done","phase":"final_answer"}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
            #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"agent_reasoning_content_delta","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","delta":"Thinking"}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
        ]

        for fixture in fixtures {
            XCTAssertThrowsError(try decodeEnvelope(fixture))
        }
    }

    func testUnknownAuxiliaryRenderedEventsAreRejected() {
        let fixtures = [
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"frontend","frontend_type":"preview","id":"/root/worker","title":"Worker","subtitle":"full","page_id":"latest","update":"replace","events":[],"next":null}},"stream_metrics":[],"blocks":[],"preview":{"id":"/root/worker","title":"Worker","subtitle":"full","page_id":"latest","update":"replace","events":[{"recorded_at_ms":1000,"event":{"type":"future_event"},"blocks":[]}],"next":null}}}"#
        ]

        for fixture in fixtures {
            XCTAssertThrowsError(try decodeEnvelope(fixture)) { error in
                XCTAssertEqual(
                    error as? GatewayWireError,
                    .invalidFrame("unknown agent event future_event")
                )
            }
        }
    }

    func testFrontendRenderAgentEventIsAccepted() throws {
        let fixture = #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"submission_id":"input-1","msg":{"type":"frontend","frontend_type":"render","capability":"tools","block":{"id":"call-1","group":"turn-1","update":"replace","state":"complete","role":"tool","title":"Read file","text":"Done","symbol":"task","format":"plain_text","tone":"neutral","files":[]}}},"stream_metrics":[],"blocks":[{"capability":"tools","block":{"id":"call-1","group":"turn-1","update":"replace","state":"complete","role":"tool","title":"Read file","text":"Done","symbol":"task","format":"plain_text","tone":"neutral","files":[]}}],"preview":null}}"#
        let envelope = try decodeEnvelope(fixture)

        guard case .agentEvent(_, let record) = envelope else {
            return XCTFail("Expected agent event envelope")
        }
        XCTAssertEqual(record.event.msg["frontendType"]?.stringValue, "render")
        XCTAssertEqual(record.blocks.first?.capability, "tools")
        XCTAssertEqual(record.blocks.first?.block.role, .tool)
        XCTAssertEqual(record.blocks.first?.block.title, "Read file")
        XCTAssertEqual(record.blocks.first?.block.text, "Done")
    }

    func testSessionsResponseAllowsOmittedRequestID() throws {
        let envelope = try decodeEnvelope(
            #"{"version":27,"type":"sessions","sessions":[\#(sessionRecordJSON)]}"#
        )
        guard case .sessions(let requestID, let sessions) = envelope else {
            return XCTFail("Expected sessions envelope")
        }
        XCTAssertNil(requestID)
        XCTAssertEqual(sessions.first?.sessionId, "chat-1")
        XCTAssertEqual(sessions.first?.pinned, true)
        XCTAssertEqual(sessions.first?.sessionContext.botId, "bot-1")
    }

    func testBackgroundApprovalResponseCarriesHiddenSessionOwnership() throws {
        let envelope = try decodeEnvelope(
            #"{"version":63,"type":"background_approvals","approvals":[{"session_id":"work-1","bot_id":"bot-1","turn_id":"turn-1","request_id":"approval-1"}]}"#
        )
        guard case .backgroundApprovals(let approvals) = envelope else {
            return XCTFail("Expected background approvals")
        }
        XCTAssertEqual(approvals, [BackgroundApproval(
            sessionId: "work-1",
            botId: "bot-1",
            turnId: "turn-1",
            requestId: "approval-1"
        )])
    }

    func testBotAndRoutineResponsesDecodeProtocolFields() throws {
        let bots = try decodeEnvelope(
            #"{"version":56,"type":"bots","request_id":"bots-1","bots":[\#(botJSON)]}"#
        )
        guard case .bots(let botsRequestID, let records) = bots else {
            return XCTFail("Expected bots envelope")
        }
        XCTAssertEqual(botsRequestID, "bots-1")
        XCTAssertEqual(records.first?.handle, "helper")
        XCTAssertEqual(records.first?.config.revision, 4)

        let gitDiff = try decodeEnvelope(#"{"version":27,"type":"git_diff","request_id":"diff-1","session_id":"chat-1","scope":"unstaged","diff":"diff --git a/a b/a"}"#)
        guard case .gitDiff(let diffRequestID, let diffSessionID, let scope, let diff) = gitDiff else {
            return XCTFail("Expected git diff envelope")
        }
        XCTAssertEqual(diffRequestID, "diff-1")
        XCTAssertEqual(diffSessionID, "chat-1")
        XCTAssertEqual(scope, .unstaged)
        XCTAssertTrue(diff.hasPrefix("diff --git"))

        let routines = try decodeEnvelope(#"{"version":56,"type":"routines","request_id":"routine-1","routines":[{"id":"routine-1","bot_id":"bot-1","workspace":"/srv/mobius","instructions":"Review open pull requests","schedule":{"kind":"interval","at":null,"every_seconds":120,"expression":null,"time_zone":null},"ends_at":null,"enabled":true,"finished":false,"next_run_at":500}]}"#)
        guard case .routines(let routineRequestID, let decodedRoutines) = routines else {
            return XCTFail("Expected routines envelope")
        }
        XCTAssertEqual(routineRequestID, "routine-1")
        XCTAssertEqual(decodedRoutines.first?.botId, "bot-1")
        XCTAssertEqual(decodedRoutines.first?.workspace, "/srv/mobius")
        XCTAssertEqual(decodedRoutines.first?.instructions, "Review open pull requests")
        XCTAssertEqual(decodedRoutines.first?.schedule.everySeconds, 120)
        XCTAssertEqual(decodedRoutines.first?.nextRunAt, 500)

        let history = try decodeEnvelope(#"{"version":56,"type":"routine_history","request_id":"history-1","runs":[{"id":"run-1","routine_id":"routine-1","bot_id":"bot-1","started_at":100,"finished_at":110,"status":"succeeded","session_id":"chat-2","message":null}]}"#)
        guard case .routineHistory(let historyRequestID, let runs) = history else {
            return XCTFail("Expected routine history envelope")
        }
        XCTAssertEqual(historyRequestID, "history-1")
        XCTAssertEqual(runs.first?.routineId, "routine-1")
        XCTAssertEqual(runs.first?.botId, "bot-1")
        XCTAssertEqual(runs.first?.status, .succeeded)
    }

    func testRoutineRunPreviewDecodesNestedPayload() throws {
        let envelope = try decodeEnvelope(#"{"version":56,"type":"routine_run_preview","request_id":"preview-1","preview":{"routine":{"id":"routine-1","bot_id":"bot-1","workspace":"/srv/mobius","instructions":"Review open pull requests","schedule":{"kind":"interval","at":null,"every_seconds":120,"expression":null,"time_zone":null},"ends_at":null,"enabled":true,"finished":false,"next_run_at":500},"run":{"id":"run-1","routine_id":"routine-1","bot_id":"bot-1","started_at":100,"finished_at":null,"status":"running","session_id":"chat-2","message":null},"records":[],"next_before_sequence":42}}"#)
        guard case .routineRunPreview(let preview) = envelope else {
            return XCTFail("Expected routine run preview envelope")
        }

        XCTAssertEqual(preview.requestID, "preview-1")
        XCTAssertEqual(preview.routine.id, "routine-1")
        XCTAssertEqual(preview.run.id, "run-1")
        XCTAssertEqual(preview.records.count, 0)
        XCTAssertEqual(preview.nextBeforeSequence, 42)
    }

    func testUnknownRoutineRunStatusIsRejected() {
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":56,"type":"routine_history","request_id":"history-1","runs":[{"id":"run-1","routine_id":"routine-1","bot_id":"bot-1","started_at":100,"finished_at":110,"status":"future_status","session_id":null,"message":null}]}"#
        ))
    }

    func testControlProviderProfileAndDirectoryResponsesDecode() throws {
        guard case .authenticated = try decodeEnvelope(#"{"version":27,"type":"authenticated"}"#) else {
            return XCTFail("Expected authenticated envelope")
        }
        guard case .accepted(let acceptedID) = try decodeEnvelope(#"{"version":27,"type":"accepted","request_id":"request-1"}"#) else {
            return XCTFail("Expected accepted envelope")
        }
        XCTAssertEqual(acceptedID, "request-1")

        guard case .providerCredentialSaved(let credentialID, let instance, let provider) = try decodeEnvelope(#"{"version":27,"type":"provider_credential_saved","request_id":"credential-1","instance":"openai-work","provider":"openai_socket"}"#) else {
            return XCTFail("Expected provider credential saved envelope")
        }
        XCTAssertEqual(credentialID, "credential-1")
        XCTAssertEqual(instance, "openai-work")
        XCTAssertEqual(provider, "openai_socket")

        guard case .pairingCode(let pairingID, let code, let expiresAt) = try decodeEnvelope(#"{"version":27,"type":"pairing_code","request_id":"pairing-1","code":"123456","expires_at":500}"#) else {
            return XCTFail("Expected pairing code envelope")
        }
        XCTAssertEqual(pairingID, "pairing-1")
        XCTAssertEqual(code, "123456")
        XCTAssertEqual(expiresAt, 500)

        guard case .providerLoginStarted(let loginRequestID, let loginID, let loginProvider, let verificationURL, let userCode) = try decodeEnvelope(#"{"version":27,"type":"provider_login_started","request_id":"login-1","login_id":"device-1","provider":"openai_codex","verification_url":"https://example.com/device","user_code":"ABCD"}"#) else {
            return XCTFail("Expected provider login started envelope")
        }
        XCTAssertEqual(loginRequestID, "login-1")
        XCTAssertEqual(loginID, "device-1")
        XCTAssertEqual(loginProvider, "openai_codex")
        XCTAssertEqual(verificationURL, "https://example.com/device")
        XCTAssertEqual(userCode, "ABCD")

        guard case .providerLoginFinished(let finishedRequestID, let finishedLoginID, let finishedProvider) = try decodeEnvelope(#"{"version":27,"type":"provider_login_finished","request_id":"login-1","login_id":"device-1","provider":"openai_codex"}"#) else {
            return XCTFail("Expected provider login finished envelope")
        }
        XCTAssertEqual(finishedRequestID, "login-1")
        XCTAssertEqual(finishedLoginID, "device-1")
        XCTAssertEqual(finishedProvider, "openai_codex")

        guard case .gitCredentialStatus(let gitCredentialID, let available, let legacyUsername) = try decodeEnvelope(#"{"version":42,"type":"git_credential_status","request_id":"git-credential-1","available":true}"#) else {
            return XCTFail("Expected Git credential status")
        }
        XCTAssertEqual(gitCredentialID, "git-credential-1")
        XCTAssertTrue(available)
        XCTAssertNil(legacyUsername)

        guard case .gitCredentialStatus(_, _, let username) = try decodeEnvelope(#"{"version":44,"type":"git_credential_status","request_id":"git-credential-2","available":true,"username":"octo"}"#) else {
            return XCTFail("Expected Git credential username")
        }
        XCTAssertEqual(username, "octo")

        guard case .sshIdentities(let sshListID, let identities) = try decodeEnvelope(#"{"version":44,"type":"ssh_identities","request_id":"ssh-list-1","identities":[{"label":"id_ed25519","algorithm":"ssh-ed25519","fingerprint":"SHA256:safe"}]}"#) else {
            return XCTFail("Expected SSH identities")
        }
        XCTAssertEqual(sshListID, "ssh-list-1")
        XCTAssertEqual(identities.first?.label, "id_ed25519")
        XCTAssertEqual(identities.first?.algorithm, "ssh-ed25519")
        XCTAssertEqual(identities.first?.fingerprint, "SHA256:safe")

        guard case .sshIdentityGenerated(
            let sshGenerateID,
            let generatedIdentity,
            let publicKey
        ) = try decodeEnvelope(#"{"version":44,"type":"ssh_identity_generated","request_id":"ssh-generate-1","identity":{"label":"id_ed25519","algorithm":"ssh-ed25519","fingerprint":"SHA256:new"},"public_key":"ssh-ed25519 AAAA mobius"}"#) else {
            return XCTFail("Expected generated SSH identity")
        }
        XCTAssertEqual(sshGenerateID, "ssh-generate-1")
        XCTAssertEqual(generatedIdentity.label, "id_ed25519")
        XCTAssertEqual(publicKey, "ssh-ed25519 AAAA mobius")

        let profileFixture = #"{"version":27,"type":"profile","request_id":"profile-1","profile":{"user_name":"Ada","daily_usage":[{"unix_day":100,"provider":"anthropic","usage":\#(usageJSON)},{"unix_day":100,"provider":"openai_socket","usage":\#(usageJSON)}],"run_stats":\#(runStatsJSON),"recent_run_groups":[{"session_id":"chat-1","title":"Thread title","runs":[{"session_id":"agent-1","submission_id":"input-1","turn_id":"turn-1","started_at_ms":1000,"finished_at_ms":10000,"elapsed_ms":9000,"outcome":"completed","model_calls":2,"tool_calls":3,"failed_tool_calls":0,"usage":\#(usageJSON)}]}]}}"#
        guard case .profile(let profileID, let profile) = try decodeEnvelope(profileFixture) else {
            return XCTFail("Expected profile envelope")
        }
        XCTAssertEqual(profileID, "profile-1")
        XCTAssertEqual(profile.userName, "Ada")
        XCTAssertEqual(profile.dailyUsage.map(\.provider), ["anthropic", "openai_socket"])
        XCTAssertEqual(profile.dailyUsage.map(\.unixDay), [100, 100])
        XCTAssertEqual(profile.runStats.runCount, 2)
        XCTAssertEqual(profile.recentRunGroups.first?.title, "Thread title")
        XCTAssertEqual(profile.recentRunGroups.first?.runs.first?.sessionId, "agent-1")
        XCTAssertEqual(profile.recentRunGroups.first?.runs.first?.toolCalls, 3)

        guard case .directories(let directoryID, let listing) = try decodeEnvelope(#"{"version":27,"type":"directories","request_id":"directories-1","listing":{"path":"/srv","parent":null,"entries":[]}}"#) else {
            return XCTFail("Expected directories envelope")
        }
        XCTAssertEqual(directoryID, "directories-1")
        XCTAssertEqual(listing.path, "/srv")
    }

    func testSwarmCatalogResponseCarriesOptionalRequestID() throws {
        let swarm = #"{"id":"swarm-1","title":"Quiet Foxes","leader_bot_id":"bot-1","members":[{"bot_id":"bot-1","handle":"leader"},{"bot_id":"bot-2","handle":"builder"}],"messages":[],"updated_at_ms":200}"#
        let mutation = try decodeEnvelope(
            #"{"version":51,"type":"swarms","request_id":"swarm-create-1","swarms":[\#(swarm)]}"#
        )
        guard case .swarms(let requestID, let records) = mutation else {
            return XCTFail("Expected correlated swarm catalog")
        }
        XCTAssertEqual(requestID, "swarm-create-1")
        XCTAssertEqual(records.first?.leaderBotId, "bot-1")
        XCTAssertEqual(records.first?.members.last?.handle, "builder")

        let broadcast = try decodeEnvelope(
            #"{"version":51,"type":"swarms","swarms":[\#(swarm)]}"#
        )
        guard case .swarms(let broadcastID, _) = broadcast else {
            return XCTFail("Expected broadcast swarm catalog")
        }
        XCTAssertNil(broadcastID)
    }

    func testBotSessionsResponseUsesTheExistingSessionRecord() throws {
        let envelope = try decodeEnvelope(
            #"{"version":60,"type":"bot_sessions","request_id":"bot-sessions-1","bot_id":"bot-1","sessions":[\#(sessionRecordJSON)]}"#
        )
        guard case .botSessions(let requestID, let botID, let sessions) = envelope else {
            return XCTFail("Expected Bot sessions")
        }
        XCTAssertEqual(requestID, "bot-sessions-1")
        XCTAssertEqual(botID, "bot-1")
        XCTAssertEqual(sessions.first?.sessionId, "chat-1")
        XCTAssertEqual(sessions.first?.sessionContext.botId, "bot-1")
    }

    func testPairedRejectedAndErrorResponsesDecode() throws {
        guard case .paired(let clientID, let token) = try decodeEnvelope(#"{"version":27,"type":"paired","client_id":"phone-7","token":"bearer"}"#) else {
            return XCTFail("Expected paired envelope")
        }
        XCTAssertEqual(clientID, "phone-7")
        XCTAssertEqual(token, "bearer")

        guard case .rejected(let rejection) = try decodeEnvelope(#"{"version":27,"type":"rejected","request_id":"request-1","code":"conflict","message":"stale","fatal":false}"#) else {
            return XCTFail("Expected rejected envelope")
        }
        XCTAssertEqual(rejection.requestId, "request-1")
        XCTAssertEqual(rejection.code, "conflict")
        XCTAssertFalse(rejection.fatal)

        guard case .error(let failure) = try decodeEnvelope(#"{"version":27,"type":"error","code":"internal","message":"failed","fatal":true}"#) else {
            return XCTFail("Expected error envelope")
        }
        XCTAssertEqual(failure.code, "internal")
        XCTAssertTrue(failure.fatal)
    }

    func testUnknownGatewayMessageAndOperationAreRejected() {
        XCTAssertThrowsError(try decodeEnvelope(#"{"version":27,"type":"future_message"}"#)) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("unknown gateway message future_message")
            )
        }

        let operation = #"{"type":"future_operation"}"#
        XCTAssertThrowsError(try decoder().decode(AgentOperation.self, from: Data(operation.utf8))) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("unknown agent operation future_operation")
            )
        }
    }

}
