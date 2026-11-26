//! S1-09：JMAP 核心接口离线单元测试（MockTransport，无网络依赖）。

use crossmail_core::error::{ErrorKind, MailError};
use crossmail_core::model::{Address, Flag, MailboxRole, OutgoingMessage};
use crossmail_core::provider::{CancellationToken, SyncRequest};
use crossmail_jmap::auth::AuthMethod;
use crossmail_jmap::client::{map_jmap_error, JmapClient};
use crossmail_jmap::parse::{build_email_create, EmailBlob};
use crossmail_jmap::session::{percent_encode, well_known_candidates, Session};
use crossmail_jmap::sync::parse_set;
use crossmail_jmap::transport::testing::{MockResponse, MockTransport};
use crossmail_jmap::transport::RequestKind;
use serde_json::{json, Value};

fn session_value() -> Value {
    json!({
        "username": "user@example.com",
        "apiUrl": "https://jmap.example.com/api/",
        "downloadUrl": "https://jmap.example.com/download/{accountId}/{blobId}/{name}?type={type}",
        "uploadUrl": "https://jmap.example.com/upload/{accountId}/",
        "eventSourceUrl": "https://jmap.example.com/events/",
        "state": "session-state-1",
        "capabilities": {
            "urn:ietf:params:jmap:core": { "maxSizeRequest": 50000000, "maxObjectsInGet": 256 },
            "urn:ietf:params:jmap:mail": { "maxSizeAttachmentsPerEmail": 25000000 },
            "urn:ietf:params:jmap:submission": {},
            "urn:ietf:params:jmap:blob": {}
        },
        "accounts": {
            "A1": {
                "name": "user@example.com", "isPersonal": true, "isReadOnly": false,
                "accountCapabilities": { "urn:ietf:params:jmap:mail": {} }
            }
        },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "A1" }
    })
}

fn session() -> Session {
    Session::from_value(&session_value()).expect("session 合法")
}

fn client(mock: &MockTransport) -> JmapClient<&MockTransport> {
    JmapClient::with_session(mock, session(), AuthMethod::bearer("tok-123"))
}

fn response(method: &str, args: Value, call_id: &str) -> Value {
    json!({
        "methodResponses": [[method, args, call_id]],
        "sessionState": "session-state-1"
    })
}

fn error_response(kind: &str, call_id: &str) -> Value {
    json!({
        "methodResponses": [["error", { "type": kind, "description": "mock failure" }, call_id]]
    })
}

fn email_json(id: &str) -> Value {
    json!({
        "id": id,
        "blobId": format!("B-{id}"),
        "threadId": "T1",
        "mailboxIds": { "M1": true, "M2": false },
        "keywords": { "$seen": true, "$flagged": true, "important": true },
        "size": 2048,
        "receivedAt": "2026-11-13T10:00:00Z",
        "sentAt": "2026-11-13T09:59:00Z",
        "subject": "Hello",
        "from": [{ "name": "Alice", "email": "alice@example.com" }],
        "to": [{ "email": "bob@example.com" }],
        "cc": [], "bcc": [], "replyTo": [],
        "messageId": ["<msg1@example.com>"],
        "inReplyTo": [], "references": [],
        "preview": "Hi Bob",
        "textBody": [{ "partId": "1", "type": "text/plain" }],
        "htmlBody": [{ "partId": "2", "type": "text/html" }],
        "bodyValues": { "1": { "value": "Hi Bob" }, "2": { "value": "<b>Hi Bob</b>" } },
        "attachments": [{
            "blobId": "B2", "type": "application/pdf", "name": "a.pdf",
            "size": 100, "partId": "3", "disposition": "attachment"
        }],
        "headers": [{ "name": "Message-ID", "value": "<msg1@example.com>" }]
    })
}

fn mailbox_json(id: &str, name: &str, role: &str) -> Value {
    json!({
        "id": id, "name": name, "parentId": null, "role": role,
        "totalEmails": 12, "unreadEmails": 3
    })
}

// ---------- J-001 Session / 能力协商 ----------

#[test]
fn session_parses_capabilities_accounts_and_urls() {
    let s = session();
    assert_eq!(s.primary_mail_account().unwrap(), "A1");
    assert_eq!(s.state, "session-state-1");
    assert_eq!(s.max_size_request(), 50_000_000);
    assert_eq!(s.max_objects_in_get(), 256);
    assert_eq!(s.max_size_attachments_per_email(), 25_000_000);
    let caps = s.provider_capabilities();
    assert!(caps.jmap_mail);
    assert!(caps.blob_upload);
    assert!(caps.push);
    assert!(!caps.condstore);
    assert_eq!(
        s.upload_url_for("A1"),
        "https://jmap.example.com/upload/A1/"
    );
    assert_eq!(
        s.download_url_for("A1", "B9", "a b.pdf", "application/pdf"),
        "https://jmap.example.com/download/A1/B9/a%20b.pdf?type=application%2Fpdf"
    );
}

#[test]
fn session_without_mail_capability_is_explicitly_unsupported() {
    let mut v = session_value();
    v["primaryAccounts"] = json!({});
    let s = Session::from_value(&v).unwrap();
    let err = s.primary_mail_account().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(!s.provider_capabilities().jmap_mail);
}

#[test]
fn session_missing_api_url_is_protocol_error() {
    let mut v = session_value();
    v.as_object_mut().unwrap().remove("apiUrl");
    assert_eq!(
        Session::from_value(&v).unwrap_err().kind(),
        ErrorKind::Protocol
    );
}

#[test]
fn well_known_discovery_and_percent_encoding() {
    let candidates = well_known_candidates("user@example.com").unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0], "https://example.com/.well-known/jmap");
    assert_eq!(candidates[1], "https://jmap.example.com/.well-known/jmap");
    assert_eq!(
        well_known_candidates("bad").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(percent_encode("a b/c?d"), "a%20b%2Fc%3Fd");
    assert_eq!(percent_encode("Az-._~09"), "Az-._~09");
}

#[tokio::test]
async fn discover_tries_candidates_and_returns_first_valid_endpoint() {
    let mock = MockTransport::new();
    mock.push_http_error(404, "not here");
    mock.push_json(session_value());
    let (client, url) = JmapClient::discover(&mock, "user@example.com", AuthMethod::bearer("t"))
        .await
        .unwrap();
    assert_eq!(url, "https://jmap.example.com/.well-known/jmap");
    assert_eq!(client.account_id().await.unwrap(), "A1");
    assert_eq!(mock.calls().len(), 2);
}

// ---------- J-002 认证 ----------

#[test]
fn bearer_auth_header_is_applied_to_every_request() {
    assert_eq!(AuthMethod::bearer("tok-123").header(), "Bearer tok-123");
    assert!(!AuthMethod::bearer("tok-123").is_expired());
}

#[tokio::test]
async fn api_requests_carry_authorization_header() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Mailbox/get",
        json!({ "list": [], "state": "s" }),
        "c0",
    ));
    client(&mock).mailbox_get(None).await.unwrap();
    let call = mock.last_call().unwrap();
    assert_eq!(call.auth_header.as_deref(), Some("Bearer tok-123"));
    assert_eq!(call.url, "https://jmap.example.com/api/");
    let body = call.body.unwrap();
    assert_eq!(body["methodCalls"][0][0], "Mailbox/get");
    assert!(body["using"].as_array().unwrap().len() >= 2);
}

#[test]
fn oauth2_authorization_url_contains_pkce_and_scope() {
    let auth = AuthMethod::oauth2("acc", Some("ref".into()), "https://id/token", "client-1");
    let url = auth
        .authorization_url(
            "https://id/auth?tenant=x",
            "https://app/cb",
            "state-9",
            "challenge-abc",
            &["urn:ietf:params:jmap:mail", "offline_access"],
        )
        .unwrap();
    assert!(url.starts_with("https://id/auth?tenant=x&response_type=code"));
    assert!(url.contains("client_id=client-1"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("code_challenge=challenge-abc"));
    assert!(url.contains("state=state-9"));
    assert!(url.contains("scope=urn%3Aietf%3Aparams%3Ajmap%3Amail%20offline_access"));
}

#[test]
fn oauth2_refresh_form_and_token_response_update_expiry() {
    let mut auth = AuthMethod::oauth2("old", Some("ref".into()), "https://id/token", "client-1");
    let form = auth.refresh_form().unwrap();
    assert_eq!(
        form[0],
        ("grant_type".to_string(), "refresh_token".to_string())
    );
    assert_eq!(form[1], ("refresh_token".to_string(), "ref".to_string()));
    auth.apply_token_response(&json!({
        "access_token": "new", "refresh_token": "ref2", "expires_in": 3600
    }))
    .unwrap();
    assert_eq!(auth.access_token(), "new");
    assert!(!auth.is_expired());
    let mut expired = AuthMethod::oauth2("old", None, "https://id/token", "c");
    expired
        .apply_token_response(&json!({ "access_token": "x", "expires_in": 0 }))
        .unwrap();
    assert!(expired.is_expired());
    assert_eq!(
        expired.refresh_form().unwrap_err().kind(),
        ErrorKind::Authentication
    );
}

#[tokio::test]
async fn oauth2_refresh_uses_token_endpoint_and_form_encoding() {
    let mock = MockTransport::new();
    mock.push_json(json!({ "access_token": "fresh", "expires_in": 7200 }));
    let auth = AuthMethod::oauth2("old", Some("ref".into()), "https://id/token", "client-1");
    let c = JmapClient::with_session(&mock, session(), auth);
    c.refresh_oauth2().await.unwrap();
    assert_eq!(c.auth_header_value(), "Bearer fresh");
    let call = mock.last_call().unwrap();
    assert_eq!(call.kind, RequestKind::Token);
    assert_eq!(call.url, "https://id/token");
    assert_eq!(call.body.unwrap()["grant_type"], "refresh_token");
}

// ---------- J-003 Mailbox ----------

#[tokio::test]
async fn mailbox_get_maps_roles_counts_and_state() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Mailbox/get",
        json!({
            "state": "mb-state",
            "list": [
                mailbox_json("M1", "Inbox", "inbox"),
                mailbox_json("M2", "Sent", "sent"),
                mailbox_json("M3", "Archive", "archive"),
                mailbox_json("M4", "Labels", "")
            ],
            "notFound": []
        }),
        "c0",
    ));
    let out = client(&mock).mailbox_get(None).await.unwrap();
    assert_eq!(out.state, "mb-state");
    assert_eq!(out.mailboxes.len(), 4);
    assert_eq!(out.mailboxes[0].role, MailboxRole::Inbox);
    assert_eq!(out.mailboxes[1].role, MailboxRole::Sent);
    assert_eq!(out.mailboxes[2].role, MailboxRole::Archive);
    assert_eq!(out.mailboxes[3].role, MailboxRole::Other);
    assert_eq!(out.mailboxes[0].total, 12);
    assert_eq!(out.mailboxes[0].unread, 3);
    assert_eq!(out.mailboxes[0].account_id, "A1");
}

#[tokio::test]
async fn mailbox_query_set_and_create_are_well_formed() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Mailbox/query",
        json!({ "ids": ["M1", "M2"] }),
        "c0",
    ));
    let ids = client(&mock)
        .mailbox_query(Some(json!({ "name": "Work" })), 50)
        .await
        .unwrap();
    assert_eq!(ids, vec!["M1", "M2"]);
    let q = mock.last_call().unwrap().body.unwrap();
    assert_eq!(q["methodCalls"][0][1]["filter"]["name"], "Work");
    assert_eq!(q["methodCalls"][0][1]["limit"], 50);

    mock.push_json(response(
        "Mailbox/set",
        json!({ "created": { "mb": { "id": "M9" } }, "newState": "s2" }),
        "c0",
    ));
    let id = client(&mock)
        .create_mailbox("Work", Some("M1"))
        .await
        .unwrap();
    assert_eq!(id, "M9");
    let set = mock.last_call().unwrap().body.unwrap();
    assert_eq!(set["methodCalls"][0][1]["create"]["mb"]["name"], "Work");
    assert_eq!(set["methodCalls"][0][1]["create"]["mb"]["parentId"], "M1");
}

// ---------- J-004 / J-005 增量同步与内容解析 ----------

#[tokio::test]
async fn email_get_parses_body_attachments_keywords_and_headers() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Email/get",
        json!({ "list": [email_json("E1")], "state": "e-state" }),
        "c0",
    ));
    let emails = client(&mock).email_get(&["E1".into()], true).await.unwrap();
    assert_eq!(emails.len(), 1);
    let e = &emails[0];
    assert_eq!(e.subject, "Hello");
    assert_eq!(e.from[0].name.as_deref(), Some("Alice"));
    assert_eq!(e.to[0].email, "bob@example.com");
    assert_eq!(e.text_body.as_deref(), Some("Hi Bob"));
    assert_eq!(e.html_body.as_deref(), Some("<b>Hi Bob</b>"));
    assert_eq!(e.attachments.len(), 1);
    assert_eq!(e.attachments[0].blob_id.as_deref(), Some("B2"));
    assert_eq!(e.attachments[0].name.as_deref(), Some("a.pdf"));
    assert_eq!(e.attachments[0].size, 100);
    assert!(!e.attachments[0].inline);
    assert!(e.flags.contains(&Flag::Seen));
    assert!(e.flags.contains(&Flag::Flagged));
    assert!(e.flags.contains(&Flag::Custom("important".into())));
    assert_eq!(
        e.raw_headers.get("message-id").map(String::as_str),
        Some("<msg1@example.com>")
    );
    assert_eq!(e.mailbox_ids, vec!["M1"]);
    assert_eq!(e.state.as_deref(), Some("e-state"));
    assert_eq!(e.sent_at.unwrap().to_rfc3339(), "2026-11-13T09:59:00+00:00");
    let req = mock.last_call().unwrap().body.unwrap();
    let props = req["methodCalls"][0][1]["properties"].as_array().unwrap();
    assert!(props.contains(&json!("bodyValues")));
    assert_eq!(req["methodCalls"][0][1]["fetchAllBodyValues"], true);
}

#[tokio::test]
async fn email_get_metadata_only_does_not_request_body_values() {
    let mock = MockTransport::new();
    let mut metadata_only = email_json("E1");
    metadata_only.as_object_mut().unwrap().remove("bodyValues");
    mock.push_json(response(
        "Email/get",
        json!({ "list": [metadata_only] }),
        "c0",
    ));
    let emails = client(&mock)
        .email_get(&["E1".into()], false)
        .await
        .unwrap();
    // 服务端未返回 bodyValues 时正文保持 None，表示按需拉取
    assert!(emails[0].text_body.is_none());
    let props = mock.last_call().unwrap().body.unwrap()["methodCalls"][0][1]["properties"].clone();
    assert!(!props.as_array().unwrap().contains(&json!("bodyValues")));
}

#[tokio::test]
async fn email_query_parses_state_total_and_ids() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Email/query",
        json!({ "ids": ["E1", "E2"], "total": 42, "position": 0, "queryState": "q1", "canCalculateChanges": true }),
        "c0",
    ));
    let page = client(&mock).email_query(None, 2, 0).await.unwrap();
    assert_eq!(page.ids, vec!["E1", "E2"]);
    assert_eq!(page.total, 42);
    assert!(page.can_calculate_changes);
    assert_eq!(page.query_state.as_deref(), Some("q1"));
}

#[tokio::test]
async fn sync_uses_since_state_changes_and_deduplicates() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Email/changes",
        json!({
            "oldState": "s1", "newState": "s2", "hasMoreChanges": false,
            "created": ["E1"], "updated": ["E1", "E2"], "destroyed": ["E3"]
        }),
        "c0",
    ));
    mock.push_json(response(
        "Email/get",
        json!({ "list": [email_json("E1"), email_json("E1"), email_json("E2")], "state": "es" }),
        "c0",
    ));
    let mut req = SyncRequest::new("A1");
    req.since_state = Some("s1".into());
    req.include_body = true;
    let page = client(&mock)
        .sync_emails(&req, &CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(page.emails.len(), 2, "重复 id 必须去重");
    assert_eq!(page.deleted_ids, vec!["E3"]);
    assert_eq!(page.new_state.as_deref(), Some("s2"));
    assert!(!page.full_resync);
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn sync_paginates_has_more_changes_and_drops_destroyed_from_updates() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Email/changes",
        json!({
            "oldState": "s1", "newState": "s2", "hasMoreChanges": true,
            "created": ["E1"], "updated": ["E4"], "destroyed": []
        }),
        "c0",
    ));
    mock.push_json(response(
        "Email/changes",
        json!({
            "oldState": "s2", "newState": "s3", "hasMoreChanges": false,
            "created": [], "updated": ["E4"], "destroyed": ["E4"]
        }),
        "c0",
    ));
    mock.push_json(response(
        "Email/get",
        json!({ "list": [email_json("E1")] }),
        "c0",
    ));
    let mut req = SyncRequest::new("A1");
    req.since_state = Some("s1".into());
    let page = client(&mock)
        .sync_emails(&req, &CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(page.emails.len(), 1);
    assert_eq!(page.deleted_ids, vec!["E4"]);
    assert_eq!(page.new_state.as_deref(), Some("s3"));
}

#[tokio::test]
async fn state_conflict_triggers_full_resync() {
    let mock = MockTransport::new();
    mock.push_json(error_response("cannotCalculateChanges", "c0"));
    mock.push_json(response(
        "Email/query",
        json!({ "ids": ["E1"], "total": 1, "queryState": "q-full" }),
        "c0",
    ));
    mock.push_json(response(
        "Email/get",
        json!({ "list": [email_json("E1")] }),
        "c0",
    ));
    let mut req = SyncRequest::new("A1");
    req.since_state = Some("stale".into());
    let page = client(&mock)
        .sync_emails(&req, &CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(page.full_resync, "状态冲突必须全量校准");
    assert_eq!(page.new_state.as_deref(), Some("q-full"));
    assert_eq!(page.emails.len(), 1);
}

#[tokio::test]
async fn cancelled_sync_stops_before_any_request() {
    let mock = MockTransport::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let req = SyncRequest::new("A1");
    let err = client(&mock)
        .sync_emails(&req, &cancel, None)
        .await
        .unwrap_err();
    assert_eq!(err, MailError::Cancelled);
    assert!(mock.calls().is_empty());
}

// ---------- J-006 / J-007 / J-008 ----------

#[tokio::test]
async fn set_keywords_uses_jmap_patch_syntax_both_directions() {
    for (enabled, expected) in [(true, json!(true)), (false, Value::Null)] {
        let mock = MockTransport::new();
        mock.push_json(response(
            "Email/set",
            json!({ "updated": { "E1": null } }),
            "c0",
        ));
        client(&mock)
            .set_keywords(&["E1".into()], &Flag::Seen, enabled)
            .await
            .unwrap();
        let patch = mock.last_call().unwrap().body.unwrap()["methodCalls"][0][1]["update"]["E1"]
            ["keywords/$seen"]
            .clone();
        assert_eq!(patch, expected);
    }
}

#[tokio::test]
async fn move_copy_and_delete_generate_correct_mailbox_patches() {
    let mock = MockTransport::new();
    mock.push_json(response("Email/set", json!({ "updated": {} }), "c0"));
    client(&mock)
        .move_emails(&["E1".into()], "M1", "M3")
        .await
        .unwrap();
    let update =
        mock.last_call().unwrap().body.unwrap()["methodCalls"][0][1]["update"]["E1"].clone();
    assert_eq!(update["mailboxIds/M1"], Value::Null);
    assert_eq!(update["mailboxIds/M3"], true);

    mock.push_json(response("Email/set", json!({ "updated": {} }), "c0"));
    client(&mock)
        .copy_emails(&["E1".into()], "M2")
        .await
        .unwrap();
    let update =
        mock.last_call().unwrap().body.unwrap()["methodCalls"][0][1]["update"]["E1"].clone();
    assert_eq!(update["mailboxIds/M2"], true);

    mock.push_json(response("Email/set", json!({ "updated": {} }), "c0"));
    client(&mock)
        .delete_emails(&["E1".into()], Some("M4"))
        .await
        .unwrap();
    let update =
        mock.last_call().unwrap().body.unwrap()["methodCalls"][0][1]["update"]["E1"].clone();
    // 这里原本断言的是 `mailboxIds/M4 == true`（只加标签）。
    // 真机联调推翻了它：只加不移 = 邮件同时留在原文件夹和回收站，
    // 用户看到的是「点了删除，东西还在原处」。删除必须是移动语义（整体替换）。
    assert_eq!(
        update["mailboxIds"],
        json!({ "M4": true }),
        "删除应整体替换 mailboxIds"
    );
    assert!(update.get("mailboxIds/M4").is_none(), "不能只加 Trash 标签");

    mock.push_json(response("Email/set", json!({ "destroyed": ["E1"] }), "c0"));
    client(&mock)
        .delete_emails(&["E1".into()], None)
        .await
        .unwrap();
    let args = mock.last_call().unwrap().body.unwrap()["methodCalls"][0][1].clone();
    assert_eq!(args["destroy"][0], "E1");
}

#[tokio::test]
async fn submit_email_creates_draft_and_moves_to_sent_atomically() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Mailbox/get",
        json!({
            "state": "mb",
            "list": [mailbox_json("MD", "Drafts", "drafts"), mailbox_json("MS", "Sent", "sent")]
        }),
        "c0",
    ));
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "created": { "draft": { "id": "E-NEW" } }, "newState": "e1" }, "c0"],
            ["EmailSubmission/set", { "created": { "sub": { "id": "S1", "sendAt": "2026-11-13T11:00:00Z" } } }, "c1"]
        ]
    }));
    let mut msg = OutgoingMessage::new(
        "A1",
        Address::with_name("Ma", "user@example.com"),
        vec![Address::new("bob@example.com")],
        "Report",
    );
    msg.text_body = Some("body".into());
    let receipt = client(&mock).submit_email(&msg, "ID1").await.unwrap();
    assert_eq!(receipt.server_id.as_deref(), Some("S1"));
    assert_eq!(receipt.sent_mailbox_id.as_deref(), Some("MS"));
    assert_eq!(receipt.sent_at.to_rfc3339(), "2026-11-13T11:00:00+00:00");
    assert!(receipt.message_id.starts_with("<"));
    let calls = mock.last_call().unwrap().body.unwrap();
    let method_calls = calls["methodCalls"].as_array().unwrap();
    assert_eq!(method_calls.len(), 2);
    assert_eq!(method_calls[0][0], "Email/set");
    assert_eq!(method_calls[1][0], "EmailSubmission/set");
    assert_eq!(method_calls[1][1]["create"]["sub"]["emailId"], "#draft");
    let on_success = &method_calls[1][1]["onSuccessUpdateEmail"]["#sub"];
    assert_eq!(on_success["mailboxIds/MD"], Value::Null);
    assert_eq!(on_success["mailboxIds/MS"], true);
    assert_eq!(on_success["keywords/$draft"], Value::Null);
    assert_eq!(on_success["keywords/$seen"], true);
}

#[tokio::test]
async fn submit_email_rejects_invalid_message_before_any_request() {
    let mock = MockTransport::new();
    let msg = OutgoingMessage::new(
        "A1",
        Address::new("invalid-address"),
        vec![Address::new("bob@example.com")],
        "bad",
    );
    let err = client(&mock).submit_email(&msg, "ID1").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert!(mock.calls().is_empty());
}

#[test]
fn build_email_create_includes_body_values_and_blob_references() {
    let mut msg = OutgoingMessage::new(
        "A1",
        Address::new("user@example.com"),
        vec![Address::new("bob@example.com")],
        "Sub",
    );
    msg.text_body = Some("plain".into());
    msg.html_body = Some("<b>rich</b>".into());
    msg.in_reply_to = Some("<prev@example.com>".into());
    let blobs = vec![EmailBlob {
        blob_id: "BX".into(),
        name: "file.txt".into(),
        content_type: "text/plain".into(),
        size: 4,
    }];
    let v = build_email_create(&msg, "MD", "<new@example.com>", &blobs);
    assert_eq!(v["mailboxIds"]["MD"], true);
    assert_eq!(v["keywords"]["$draft"], true);
    assert_eq!(v["bodyValues"]["body"]["value"], "plain");
    assert_eq!(v["bodyValues"]["html"]["value"], "<b>rich</b>");
    assert_eq!(v["attachments"][0]["blobId"], "BX");
    assert_eq!(v["inReplyTo"][0], "<prev@example.com>");
}

// ---------- J-009 Blob ----------

#[tokio::test]
async fn upload_and_download_blob_expand_url_templates() {
    let mock = MockTransport::new();
    mock.push_json(json!({ "blobId": "B7", "type": "text/plain", "size": 5 }));
    let blob = client(&mock)
        .upload_blob(b"hello", "text/plain")
        .await
        .unwrap();
    assert_eq!(blob.id, "B7");
    assert_eq!(blob.size, 5);
    let up = mock.last_call().unwrap();
    assert_eq!(up.kind, RequestKind::Upload);
    assert_eq!(up.url, "https://jmap.example.com/upload/A1/");
    assert_eq!(up.payload_len, 5);
    assert_eq!(up.content_type.as_deref(), Some("text/plain"));

    mock.push(MockResponse::Bytes(b"payload".to_vec()));
    let data = client(&mock)
        .download_blob("B7", "a b.txt", "text/plain")
        .await
        .unwrap();
    assert_eq!(data, b"payload");
    let down = mock.last_call().unwrap();
    assert_eq!(down.kind, RequestKind::Download);
    assert!(down.url.contains("/download/A1/B7/a%20b.txt"));
    assert!(down.url.contains("type=text%2Fplain"));
}

#[tokio::test]
async fn submit_email_uploads_attachments_before_submission() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Mailbox/get",
        json!({ "list": [mailbox_json("MD", "Drafts", "drafts"), mailbox_json("MS", "Sent", "sent")] }),
        "c0",
    ));
    mock.push_json(json!({ "blobId": "BU", "type": "text/plain", "size": 3 }));
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "created": { "draft": { "id": "E1" } } }, "c0"],
            ["EmailSubmission/set", { "created": { "sub": { "id": "S1" } } }, "c1"]
        ]
    }));
    let mut msg = OutgoingMessage::new(
        "A1",
        Address::new("user@example.com"),
        vec![Address::new("bob@example.com")],
        "With attachment",
    );
    msg.attachments
        .push(crossmail_core::model::OutgoingAttachment {
            name: "n.txt".into(),
            content_type: "text/plain".into(),
            data: b"abc".to_vec(),
        });
    client(&mock).submit_email(&msg, "ID1").await.unwrap();
    assert!(mock.calls().iter().any(|c| c.kind == RequestKind::Upload));
}

// ---------- J-011 错误处理 ----------

#[test]
fn jmap_error_types_map_to_unified_error_kinds() {
    let cases = [
        ("unauthorized", ErrorKind::Authentication),
        ("accountNotFound", ErrorKind::NotFound),
        ("cannotCalculateChanges", ErrorKind::Conflict),
        ("stateMismatch", ErrorKind::Conflict),
        ("overQuota", ErrorKind::Quota),
        ("rateLimit", ErrorKind::RateLimited),
        ("serverFail", ErrorKind::Server),
        ("unknownMethod", ErrorKind::Unsupported),
        ("invalidArguments", ErrorKind::InvalidInput),
        ("weird", ErrorKind::Protocol),
    ];
    for (kind, expected) in cases {
        let err = map_jmap_error(&json!({ "type": kind, "description": "d" }));
        assert_eq!(err.kind(), expected, "error type {kind}");
    }
    assert!(map_jmap_error(&json!({ "type": "cannotCalculateChanges" })).requires_full_resync());
}

#[tokio::test]
async fn method_level_error_is_mapped_and_short_circuits() {
    let mock = MockTransport::new();
    mock.push_json(error_response("unauthorized", "c0"));
    let err = client(&mock).mailbox_get(None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Authentication);
}

#[tokio::test]
async fn http_401_maps_to_authentication_and_429_to_rate_limit() {
    let endpoint = "https://jmap.example.com/.well-known/jmap";
    let mock = MockTransport::new();
    mock.push_http_error(401, "expired");
    let c1 = JmapClient::new(&mock, endpoint, AuthMethod::bearer("t"));
    assert_eq!(
        c1.session().await.unwrap_err().kind(),
        ErrorKind::Authentication
    );

    let mock2 = MockTransport::new();
    mock2.push_http_error(429, "slow down");
    let c2 = JmapClient::new(&mock2, endpoint, AuthMethod::bearer("t"));
    assert_eq!(
        c2.session().await.unwrap_err().kind(),
        ErrorKind::RateLimited
    );
}

#[test]
fn parse_set_rejects_partial_failures() {
    let ok = json!({ "created": { "x": { "id": "E1" } }, "newState": "s" });
    assert_eq!(parse_set(&ok).unwrap().new_state.as_deref(), Some("s"));
    let bad = json!({ "notUpdated": { "E1": { "type": "stateMismatch" } } });
    assert_eq!(parse_set(&bad).unwrap_err().kind(), ErrorKind::Protocol);
}

#[tokio::test]
async fn batch_call_matches_each_call_id_and_rejects_mismatched_method() {
    let mock = MockTransport::new();
    mock.push_json(json!({
        "methodResponses": [
            ["Mailbox/get", { "list": [], "state": "a" }, "c0"],
            ["Email/query", { "ids": [], "total": 0 }, "c1"]
        ]
    }));
    let out = client(&mock)
        .call_batch(vec![
            ("Mailbox/get", json!({ "accountId": "A1" })),
            ("Email/query", json!({ "accountId": "A1" })),
        ])
        .await
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0, "Mailbox/get");
    assert_eq!(out[1].0, "Email/query");

    mock.push_json(json!({
        "methodResponses": [["Email/get", { "list": [] }, "c0"]]
    }));
    let err = client(&mock)
        .call_batch(vec![("Mailbox/get", json!({}))])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Protocol);
}

// ---------- 真机联调（live）抓出的三个缺陷，固化为离线回归 ----------

/// 缺陷 1：`AuthMethod` 只有 Bearer/OAuth2，没有 Basic。
///
/// 真机上（Stalwart JMAP）Bearer 传 API token 直接 401，Basic 传账号口令才 200。
/// 自建服务器与企业网关基本只给 Basic —— 缺了它 JMAP 适配层在真机上进不去。
#[test]
fn basic_auth_header_is_rfc7617_utf8_base64() {
    let auth = AuthMethod::basic("user@example.com", "p@ss:word");
    assert_eq!(auth.header(), "Basic bWFAZXhhbXBsZS5jb206cEBzczp3b3Jk");
    // RFC 7617 §2.1：默认字符集是 UTF-8，不是 ISO-8859-1
    let cn = AuthMethod::basic("Test User", "test-secret");
    let expected = format!(
        "Basic {}",
        base64_encode(format!("{}:{}", "Test User", "test-secret").as_bytes())
    );
    assert_eq!(cn.header(), expected);
    assert!(!auth.is_expired(), "Basic 没有可判定的过期时间");
    assert_eq!(auth.label(), "basic");
}

fn base64_encode(raw: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].iter().enumerate() {
            let pad = i > chunk.len(); // 缺失字节对应的分组补 '='
            out.push(if pad {
                '='
            } else {
                T[((n >> shift) & 0x3F) as usize] as char
            });
        }
    }
    out
}

/// 缺陷 1 的附带项：`AuthMethod` derive 了 `Debug`，Basic 若直接放明文字段，
/// 密码会随 `{:?}` 进日志、进诊断包、进 panic 消息。
#[test]
fn basic_auth_debug_never_leaks_password() {
    let auth = AuthMethod::basic("user@example.com", "super-secret");
    let rendered = format!("{auth:?}");
    assert!(
        !rendered.contains("super-secret"),
        "Debug 输出泄露了口令: {rendered}"
    );
    assert!(
        rendered.contains("user@example.com"),
        "用户名应保留: {rendered}"
    );
    assert!(
        rendered.contains("redacted"),
        "应显式标注已打码: {rendered}"
    );
    // access_token 用于日志可读性，绝不能是密码
    assert_eq!(auth.access_token(), "user@example.com");
}

/// 缺陷 2：`delete_emails` 走的是 `copy_emails`（只加 Trash 标签）。
///
/// 结果邮件同时属于原文件夹和回收站，用户看到「点了删除，东西还在原处」。
/// 真机回读 `mailboxIds` 为 `{drafts: true, trash: true}`。
#[tokio::test]
async fn delete_to_trash_replaces_mailbox_ids_instead_of_adding() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Email/set",
        json!({ "updated": { "e1": null, "e2": null }, "newState": "s2" }),
        "c0",
    ));
    client(&mock)
        .delete_emails(&["e1".into(), "e2".into()], Some("TRASH"))
        .await
        .unwrap();

    let body = mock.last_call().unwrap().body.unwrap();
    let update = &body["methodCalls"][0][1]["update"];
    // 必须是「整体替换」：mailboxIds 直接赋值，而不是 mailboxIds/TRASH = true
    assert_eq!(
        update["e1"]["mailboxIds"],
        json!({ "TRASH": true }),
        "删除应整体替换 mailboxIds（移动语义），实际: {update}"
    );
    assert!(
        update["e1"].get("mailboxIds/TRASH").is_none(),
        "不能只加 Trash 标签而不移除原文件夹（那就是复制不是移动）"
    );

    // 对照：copy 才是「只加一条」
    let mock2 = MockTransport::new();
    mock2.push_json(response(
        "Email/set",
        json!({ "updated": { "e1": null } }),
        "c0",
    ));
    client(&mock2)
        .copy_emails(&["e1".into()], "ARCH")
        .await
        .unwrap();
    let body2 = mock2.last_call().unwrap().body.unwrap();
    assert_eq!(
        body2["methodCalls"][0][1]["update"]["e1"]["mailboxIds/ARCH"],
        json!(true),
        "复制应当只加一条，语义不能和删除混为一谈"
    );
}

/// 缺陷 3：`/set` 部分失败只报数量，把服务端给的 `type` 全扔了。
///
/// 真机上销毁非空文件夹失败只看到 `notDestroyed=1`，
/// 无法区分 notFound / forbidden / mailboxHasEmail —— 三者处置方式完全不同。
#[test]
fn set_failure_surfaces_server_supplied_reason() {
    let err = parse_set(&json!({
        "notDestroyed": {
            "bz": { "type": "mailboxHasEmail", "description": "Mailbox is not empty." }
        }
    }))
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("mailboxHasEmail"), "缺少 type: {text}");
    assert!(
        text.contains("Mailbox is not empty."),
        "缺少 description: {text}"
    );
    assert!(text.contains("bz"), "缺少失败对象 id: {text}");

    // invalidProperties 要带上具体属性名，否则用户不知道该改哪个字段
    let err2 = parse_set(&json!({
        "notCreated": {
            "draft": { "type": "invalidProperties", "properties": ["from", "subject"] }
        }
    }))
    .unwrap_err();
    let text2 = err2.to_string();
    assert!(text2.contains("invalidProperties"), "{text2}");
    assert!(
        text2.contains("from") && text2.contains("subject"),
        "{text2}"
    );
}

// ---------------- S6-11：发送身份（Identity）与提交链路 ----------------

fn mailbox_get_body() -> Value {
    response(
        "Mailbox/get",
        json!({
            "accountId": "A1", "state": "st1", "notFound": [],
            "list": [
                { "id": "MD", "name": "Drafts", "role": "drafts" },
                { "id": "MS", "name": "Sent", "role": "sent" }
            ]
        }),
        "c0",
    )
}

fn outgoing() -> OutgoingMessage {
    OutgoingMessage::new(
        "A1",
        Address::new("user@example.com"),
        vec![Address::new("bob@example.com")],
        "身份解析",
    )
}

/// 真机 Stalwart 的 `Identity/get`：`id` 是服务端自建标识（`"b"`），不是邮箱地址。
#[tokio::test]
async fn identity_get_parses_server_identity() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Identity/get",
        json!({
            "accountId": "A1", "state": "s1", "notFound": [],
            "list": [{
                "id": "b", "name": "System administrator", "email": "admin@example.com",
                "replyTo": null, "textSignature": "", "mayDelete": true
            }]
        }),
        "c0",
    ));
    let c = client(&mock);
    let list = c.identity_get().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "b");
    assert_eq!(list[0].email, "admin@example.com");
    assert!(list[0].may_delete);

    let body = mock.last_call().expect("应有请求").body.expect("有请求体");
    assert_eq!(body["methodCalls"][0][0], json!("Identity/get"));
    assert!(
        body["using"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "urn:ietf:params:jmap:submission"),
        "Identity/get 必须带上 submission 能力，否则服务端会 unknownCapability"
    );
}

/// 多个身份时按邮箱地址精确匹配（大小写不敏感），不靠列表顺序撞运气。
#[tokio::test]
async fn resolve_identity_matches_email_case_insensitively() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Identity/get",
        json!({
            "accountId": "A1", "state": "s1", "notFound": [],
            "list": [
                { "id": "alias", "name": "Alias", "email": "other@example.com" },
                { "id": "IA", "name": "Main", "email": "MA@Example.com" }
            ]
        }),
        "c0",
    ));
    let c = client(&mock);
    assert_eq!(c.resolve_identity_id("user@example.com").await.unwrap(), "IA");
}

/// 只有一个身份且邮箱不匹配时，直接用它是安全的（没有选错的可能）。
#[tokio::test]
async fn resolve_identity_falls_back_when_only_one_identity() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Identity/get",
        json!({
            "accountId": "A1", "state": "s1", "notFound": [],
            "list": [{ "id": "solo", "email": "admin@example.com" }]
        }),
        "c0",
    ));
    let c = client(&mock);
    assert_eq!(
        c.resolve_identity_id("user@example.com").await.unwrap(),
        "solo"
    );
}

/// 多个身份且无匹配：必须报错而不是猜——猜错等于冒用别人的名义发信。
#[tokio::test]
async fn resolve_identity_refuses_to_guess_between_multiple() {
    let mock = MockTransport::new();
    mock.push_json(response(
        "Identity/get",
        json!({
            "accountId": "A1", "state": "s1", "notFound": [],
            "list": [
                { "id": "x", "email": "a@example.com" },
                { "id": "y", "email": "b@example.com" }
            ]
        }),
        "c0",
    ));
    let c = client(&mock);
    let err = c.resolve_identity_id("user@example.com").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    let text = err.to_string();
    assert!(text.contains("x<a@example.com>"), "要列出可用身份: {text}");
    assert!(text.contains("y<b@example.com>"), "{text}");
}

/// 服务端没有 submission 能力时明确报 Unsupported，不假装能发信。
#[tokio::test]
async fn identity_get_without_submission_capability_is_unsupported() {
    let mut value = session_value();
    value["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("urn:ietf:params:jmap:submission");
    let s = Session::from_value(&value).unwrap();
    let mock = MockTransport::new();
    let c = JmapClient::with_session(&mock, s, AuthMethod::bearer("tok"));
    let err = c.identity_get().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(mock.calls().is_empty(), "不该发出请求");
}

/// 身份 id 不合法时，错误里必须带服务端原话（invalidProperties[identityId]），
/// 否则使用者只会看到「提交失败」，无从判断是身份选错还是网络问题。
#[tokio::test]
async fn submit_email_surfaces_identity_rejection_reason() {
    let mock = MockTransport::new();
    mock.push_json(mailbox_get_body());
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "accountId": "A1", "newState": "st2",
                            "created": { "draft": { "id": "E42" } } }, "c0"],
            ["EmailSubmission/set", { "accountId": "A1",
                "notCreated": { "sub": { "type": "invalidProperties",
                                          "description": "Field could not be set.",
                                          "properties": ["identityId"] } } }, "c1"]
        ],
        "sessionState": "session-state-1"
    }));
    let c = client(&mock);
    let err = c
        .submit_email(&outgoing(), "user@example.com")
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("invalidProperties"), "{text}");
    assert!(text.contains("identityId"), "必须指出是哪个属性: {text}");
}

/// 附件走 Blob 上传后，`Email/set` 里的 attachments 必须引用上传得到的 blobId。
#[tokio::test]
async fn submit_email_references_uploaded_blob_ids() {
    let mock = MockTransport::new();
    mock.push_json(mailbox_get_body());
    mock.push_json(json!({ "accountId": "A1", "blobId": "B-UP", "type": "text/csv", "size": 4 }));
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "accountId": "A1", "created": { "draft": { "id": "E42" } } }, "c0"],
            ["EmailSubmission/set", { "accountId": "A1",
                "created": { "sub": { "id": "S1", "sendAt": "2026-11-14T01:15:03Z" } } }, "c1"]
        ],
        "sessionState": "session-state-1"
    }));
    let c = client(&mock);
    let mut msg = outgoing();
    msg.attachments
        .push(crossmail_core::model::OutgoingAttachment {
            name: "报表.csv".into(),
            content_type: "text/csv".into(),
            data: b"a,b\n".to_vec(),
        });
    let receipt = c.submit_email(&msg, "IA").await.unwrap();
    assert_eq!(receipt.server_id.as_deref(), Some("S1"));

    let body = mock.last_call().expect("应有请求").body.expect("有请求体");
    let attachments = &body["methodCalls"][0][1]["create"]["draft"]["attachments"];
    assert_eq!(attachments[0]["blobId"], json!("B-UP"));
    assert_eq!(attachments[0]["name"], json!("报表.csv"));
}

/// 同一 call id 可能有多条响应：`onSuccessUpdateEmail` 触发的隐式 `Email/set`
/// 会被服务端追加返回，并且**复用提交调用的 call id**（真机 Stalwart 实测）。
///
/// 用 `insert` 收集会把先到的 `EmailSubmission/set` 覆盖掉，报出
/// 「期望 EmailSubmission/set，实际 Email/set」——功能其实成功了，却在客户端炸。
#[tokio::test]
async fn call_batch_tolerates_implicit_response_reusing_call_id() {
    let mock = MockTransport::new();
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "created": { "draft": { "id": "E1" } } }, "c0"],
            ["EmailSubmission/set", { "created": { "sub": { "id": "S1" } } }, "c1"],
            // 隐式 onSuccessUpdateEmail 产生的第三条，call id 仍是 c1
            ["Email/set", { "updated": { "E1": null } }, "c1"]
        ],
        "sessionState": "session-state-1"
    }));
    let c = client(&mock);
    let results = c
        .call_batch(vec![
            ("Email/set", json!({})),
            ("EmailSubmission/set", json!({})),
        ])
        .await
        .expect("重复 call id 不应让整批调用失败");
    assert_eq!(results[0].0, "Email/set");
    assert_eq!(results[1].0, "EmailSubmission/set");
    assert_eq!(results[1].1["created"]["sub"]["id"], json!("S1"));
}
