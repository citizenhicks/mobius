import Foundation
import XCTest

extension GatewayWireTests {
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

    func testSessionScopedRequestsEncodeSessionID() throws {
        let submission = Submission(
            id: "input-1",
            op: .userInput(text: "Hello", attachments: [])
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
            (.startCronSetup(requestID: "setup-1", sessionID: "chat-1", task: "Review nightly"), "start_cron_setup"),
            (.listCron(requestID: "cron-1", sessionID: "chat-1"), "list_cron"),
            (.rescheduleCron(requestID: "cron-2", sessionID: "chat-1", id: "task-1", schedule: "0 9 * * *"), "reschedule_cron"),
            (.deleteCron(requestID: "cron-3", sessionID: "chat-1", id: "task-1"), "delete_cron"),
            (.runCron(requestID: "cron-4", sessionID: "chat-1", id: "task-1"), "run_cron"),
            (.listCronHistory(requestID: "cron-5", sessionID: "chat-1", id: nil), "list_cron_history")
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
        XCTAssertEqual(Set(enabled), ["cron", "extensions", "subagents"])
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

        let cronSetup = try requestObject(.startCronSetup(
            requestID: "setup-2",
            sessionID: "chat-1",
            task: nil
        ))
        XCTAssertTrue(cronSetup["task"] is NSNull)
        let history = try requestObject(.listCronHistory(
            requestID: "cron-6",
            sessionID: "chat-1",
            id: nil
        ))
        XCTAssertTrue(history["id"] is NSNull)
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
                op: .userInput(text: "Review this", attachments: [file])
            )
        ))
        let submission = try XCTUnwrap(submit["submission"] as? [String: Any])
        let operation = try XCTUnwrap(submission["op"] as? [String: Any])
        let attachments = try XCTUnwrap(operation["attachments"] as? [[String: Any]])
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
