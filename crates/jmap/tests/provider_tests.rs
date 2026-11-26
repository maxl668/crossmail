//! S3-02：JMAP Provider 适配层验证（全部离线，MockTransport）。

use crossmail_core::model::{Flag, MailboxRole, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{CancellationToken, MailProvider, SyncRequest};
use crossmail_jmap::auth::AuthMethod;
use crossmail_jmap::client::JmapClient;
use crossmail_jmap::provider::JmapProvider;
use crossmail_jmap::session::Session;
use crossmail_jmap::transport::testing::MockTransport;
use serde_json::{json, Value};
use std::sync::Arc;

fn session_value() -> Value {
    json!({
        "username": "user@example.com",
        "apiUrl": "https://jmap.example.com/api/",
        "downloadUrl": "https://jmap.example.com/download/{accountId}/{blobId}/{name}?type={type}",
        "uploadUrl": "https://jmap.example.com/upload/{accountId}/",
        "state": "session-state-1",
        "capabilities": {
            "urn:ietf:params:jmap:core": { "maxSizeRequest": 50000000, "maxObjectsInGet": 256 },
            "urn:ietf:params:jmap:mail": {},
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

fn provider(mock: &Arc<MockTransport>) -> JmapProvider<Arc<MockTransport>> {
    let session = Session::from_value(&session_value()).expect("session 合法");
    let client = JmapClient::with_session(Arc::clone(mock), session, AuthMethod::bearer("tok"));
    JmapProvider::from_client(client, "user@example.com")
}

fn response(method: &str, args: Value) -> Value {
    json!({ "methodResponses": [[method, args, "c0"]], "sessionState": "session-state-1" })
}

#[tokio::test]
async fn verify_credentials_reports_account_and_capabilities() {
    let mock = Arc::new(MockTransport::new());
    let p = provider(&mock);
    assert_eq!(p.protocol(), ProtocolKind::Jmap);

    let info = p.verify_credentials().await.unwrap();
    assert_eq!(info.account_id, "A1");
    assert_eq!(info.email_address, "user@example.com");
    assert!(info.capabilities.jmap_mail);
    assert!(info.capabilities.blob_upload);
    // 能力缓存同步可见（capabilities 是同步方法）
    assert!(p.capabilities().jmap_mail);
}

#[tokio::test]
async fn list_mailboxes_maps_roles_and_account() {
    let mock = Arc::new(MockTransport::new());
    mock.push_json(response(
        "Mailbox/get",
        json!({
            "accountId": "A1",
            "state": "st1",
            "list": [
                { "id": "M1", "name": "Inbox", "role": "inbox", "totalEmails": 12, "unreadEmails": 3 },
                { "id": "M2", "name": "已发送", "role": "sent", "totalEmails": 5, "unreadEmails": 0 },
                { "id": "M3", "name": "自定义", "role": null, "totalEmails": 1, "unreadEmails": 0 }
            ],
            "notFound": []
        }),
    ));

    let p = provider(&mock);
    let list = p.list_mailboxes().await.unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].role, MailboxRole::Inbox);
    assert_eq!(list[1].role, MailboxRole::Sent);
    assert_eq!(list[2].role, MailboxRole::Other);
    assert!(
        list.iter().all(|m| m.account_id == "A1"),
        "适配层要补齐 accountId"
    );
}

#[tokio::test]
async fn set_flags_uses_jmap_keyword_patch() {
    let mock = Arc::new(MockTransport::new());
    mock.push_json(response(
        "Email/set",
        json!({ "accountId": "A1", "updated": { "E1": null }, "newState": "st2" }),
    ));

    let p = provider(&mock);
    p.set_flags(&["E1".to_string()], Flag::Seen, true)
        .await
        .unwrap();

    let body = mock
        .last_call()
        .expect("应有一次调用")
        .body
        .expect("有请求体");
    let patch = &body["methodCalls"][0][1]["update"]["E1"];
    assert_eq!(
        patch["keywords/$seen"],
        json!(true),
        "标记走 JMAP patch 语法"
    );
}

#[tokio::test]
async fn partial_failure_is_reported_not_swallowed() {
    let mock = Arc::new(MockTransport::new());
    mock.push_json(response(
        "Email/set",
        json!({
            "accountId": "A1",
            "notUpdated": { "E1": { "type": "notFound" } }
        }),
    ));

    let p = provider(&mock);
    let err = p
        .set_flags(&["E1".to_string()], Flag::Flagged, false)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), crossmail_core::ErrorKind::Protocol);
}

#[tokio::test]
async fn move_between_mailboxes_sends_mailbox_patch() {
    let mock = Arc::new(MockTransport::new());
    mock.push_json(response(
        "Email/set",
        json!({ "accountId": "A1", "updated": { "E1": null } }),
    ));

    let p = provider(&mock);
    p.move_emails(&["E1".to_string()], &"M1".to_string(), &"M2".to_string())
        .await
        .unwrap();

    let body = mock.last_call().unwrap().body.unwrap();
    let patch = &body["methodCalls"][0][1]["update"]["E1"];
    assert_eq!(patch["mailboxIds/M1"], json!(null));
    assert_eq!(patch["mailboxIds/M2"], json!(true));
}

#[tokio::test]
async fn move_to_same_mailbox_is_noop() {
    let mock = Arc::new(MockTransport::new());
    let p = provider(&mock);
    p.move_emails(&["E1".to_string()], &"M1".to_string(), &"M1".to_string())
        .await
        .unwrap();
    assert!(mock.calls().is_empty(), "同文件夹移动不该发请求");
}

#[tokio::test]
async fn sync_emails_uses_state_from_request() {
    let mock = Arc::new(MockTransport::new());
    mock.push_json(response(
        "Email/changes",
        json!({
            "accountId": "A1",
            "oldState": "s1",
            "newState": "s2",
            "hasMoreChanges": false,
            "created": ["E9"],
            "updated": [],
            "destroyed": []
        }),
    ));
    mock.push_json(response(
        "Email/get",
        json!({
            "accountId": "A1",
            "state": "s2",
            "list": [{
                "id": "E9",
                "mailboxIds": { "M1": true },
                "keywords": { "$seen": true },
                "size": 10,
                "receivedAt": "2026-11-13T10:00:00Z",
                "subject": "增量邮件",
                "from": [], "to": [], "cc": [], "bcc": [], "replyTo": [],
                "preview": "p"
            }],
            "notFound": []
        }),
    ));

    let p = provider(&mock);
    let req = SyncRequest {
        account_id: "A1".into(),
        mailbox_id: None,
        since_state: Some("s1".into()),
        since_modseq: None,
        limit: 100,
        include_body: false,
    };
    let page = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(page.emails.len(), 1);
    assert_eq!(page.emails[0].subject, "增量邮件");
    assert_eq!(page.new_state.as_deref(), Some("s2"));
}

#[tokio::test]
async fn empty_ids_short_circuit_without_request() {
    let mock = Arc::new(MockTransport::new());
    let p = provider(&mock);
    p.set_flags(&[], Flag::Seen, true).await.unwrap();
    let err = p.delete_emails(&[]).await.unwrap_err();
    assert_eq!(err.kind(), crossmail_core::ErrorKind::InvalidInput);
    assert!(mock.calls().is_empty());
}

#[tokio::test]
async fn send_email_delegates_to_submission_identity() {
    let mock = Arc::new(MockTransport::new());
    let session = Session::from_value(&session_value()).unwrap();
    let client = JmapClient::with_session(Arc::clone(&mock), session, AuthMethod::bearer("tok"));
    let p = JmapProvider::from_client(client, "user@example.com").with_identity("identity-42");

    // 保存草稿需草稿箱，发送后写入已发送箱
    mock.push_json(response(
        "Mailbox/get",
        json!({
            "accountId": "A1", "state": "st1", "notFound": [],
            "list": [
                { "id": "MD", "name": "Drafts", "role": "drafts" },
                { "id": "MS", "name": "Sent", "role": "sent" }
            ]
        }),
    ));
    // submit_email 走一次批量调用：Email/set(create draft) + EmailSubmission/set
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "accountId": "A1", "newState": "st2",
                            "created": { "draft": { "id": "E42" } } }, "c0"],
            ["EmailSubmission/set", { "accountId": "A1",
                                      "created": { "sub": { "id": "S1", "emailId": "E42",
                                                            "sendAt": "2026-11-13T10:00:00Z" } } }, "c1"]
        ],
        "sessionState": "session-state-1"
    }));

    let msg = OutgoingMessage::new(
        "A1",
        crossmail_core::Address::new("user@example.com"),
        vec![crossmail_core::Address::new("bob@example.com")],
        "测试发送",
    );
    let receipt = p.send_email(msg).await.unwrap();
    assert_eq!(receipt.server_id.as_deref(), Some("S1"));
    assert_eq!(receipt.sent_mailbox_id.as_deref(), Some("MS"));
    assert!(receipt.message_id.starts_with('<') && receipt.message_id.ends_with('>'));

    let body = mock.last_call().expect("应有调用").body.expect("有请求体");
    assert_eq!(body["methodCalls"][0][0], json!("Email/set"));
    assert_eq!(
        body["methodCalls"][1][1]["create"]["sub"]["identityId"],
        json!("identity-42"),
        "必须使用注入的 identity"
    );
}

/// 未显式注入身份时，必须先用 `Identity/get` 解析出服务端认可的 id。
///
/// 曾经的实现直接拿邮箱地址当 `identityId`，真机 Stalwart 会回
/// `notCreated[sub]={type:"invalidProperties",properties:["identityId"]}` ——
/// 发信链路 100% 失败。
#[tokio::test]
async fn send_email_resolves_identity_id_from_identity_get() {
    let mock = Arc::new(MockTransport::new());
    let p = provider(&mock);

    mock.push_json(response(
        "Identity/get",
        json!({
            "accountId": "A1", "state": "s1", "notFound": [],
            "list": [
                { "id": "alias", "name": "Alias", "email": "other@example.com" },
                { "id": "IA", "name": "Main", "email": "user@example.com" }
            ]
        }),
    ));
    mock.push_json(response(
        "Mailbox/get",
        json!({
            "accountId": "A1", "state": "st1", "notFound": [],
            "list": [
                { "id": "MD", "name": "Drafts", "role": "drafts" },
                { "id": "MS", "name": "Sent", "role": "sent" }
            ]
        }),
    ));
    mock.push_json(json!({
        "methodResponses": [
            ["Email/set", { "accountId": "A1", "created": { "draft": { "id": "E42" } } }, "c0"],
            ["EmailSubmission/set", { "accountId": "A1",
                "created": { "sub": { "id": "S1", "sendAt": "2026-11-14T01:15:03Z" } } }, "c1"]
        ],
        "sessionState": "session-state-1"
    }));

    let msg = OutgoingMessage::new(
        "A1",
        crossmail_core::Address::new("user@example.com"),
        vec![crossmail_core::Address::new("bob@example.com")],
        "解析身份",
    );
    let receipt = p.send_email(msg).await.unwrap();
    assert_eq!(receipt.server_id.as_deref(), Some("S1"));

    let body = mock.last_call().expect("应有调用").body.expect("有请求体");
    assert_eq!(
        body["methodCalls"][1][1]["create"]["sub"]["identityId"],
        json!("IA"),
        "identityId 必须是 Identity/get 返回的 id，不能是邮箱地址"
    );
}

/// `identities()` 是给 UI/排障用的公开入口：注入身份也要能查到服务端真实列表。
#[tokio::test]
async fn identities_lists_server_identities() {
    let mock = Arc::new(MockTransport::new());
    let p = provider(&mock);
    mock.push_json(response(
        "Identity/get",
        json!({
            "accountId": "A1", "state": "s1", "notFound": [],
            "list": [{ "id": "IA", "name": "Main", "email": "user@example.com", "mayDelete": false }]
        }),
    ));
    let list = p.identities().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "IA");
    assert_eq!(list[0].email, "user@example.com");
}
