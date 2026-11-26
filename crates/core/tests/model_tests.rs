use crossmail_core::*;

#[test]
fn flag_imap_roundtrip() {
    for f in [
        Flag::Seen,
        Flag::Flagged,
        Flag::Answered,
        Flag::Draft,
        Flag::Deleted,
    ] {
        assert_eq!(
            Flag::from_imap(&f.as_imap()),
            Some(f.clone()),
            "imap: {:?}",
            f
        );
        assert_eq!(
            Flag::from_jmap_keyword(&f.as_jmap_keyword()),
            Some(f.clone()),
            "jmap: {:?}",
            f
        );
    }
    assert_eq!(
        Flag::from_jmap_keyword("MyLabel"),
        Some(Flag::Custom("MyLabel".into()))
    );
    assert_eq!(Flag::from_jmap_keyword("$unknown"), None);
    assert_eq!(
        Flag::from_imap("\\Recent"),
        Some(Flag::Custom("\\Recent".into()))
    );
}

#[test]
fn mailbox_role_mapping() {
    assert_eq!(MailboxRole::from_special_use("\\Sent"), MailboxRole::Sent);
    assert_eq!(MailboxRole::from_special_use("\\Junk"), MailboxRole::Junk);
    assert_eq!(
        MailboxRole::from_special_use("\\AllMail"),
        MailboxRole::Archive
    );
    assert_eq!(MailboxRole::from_jmap_role("trash"), MailboxRole::Trash);
    assert_eq!(MailboxRole::from_jmap_role("nonsense"), MailboxRole::Other);
}

#[test]
fn email_flag_write_is_idempotent() {
    let mut e = Email::new("m1", "acc1", "inbox");
    e.set_flag(Flag::Seen, true);
    e.set_flag(Flag::Seen, true);
    assert_eq!(e.flags.len(), 1);
    assert!(e.is_read());
    e.set_flag(Flag::Custom("Work".into()), true);
    e.set_flag(Flag::Seen, false);
    assert_eq!(e.flags, vec![Flag::Custom("Work".into())]);
    assert!(!e.is_read());
}

#[test]
fn address_validation_and_domain() {
    let a = Address::with_name("Test User", "user@example.com");
    assert!(a.is_valid());
    assert_eq!(a.domain(), "example.com");
    assert!(!Address::new("not-an-email").is_valid());
    assert!(!Address::new("a@localhost").is_valid());
}

#[test]
fn outgoing_message_validation() {
    let mut m = OutgoingMessage::new(
        "acc",
        Address::new("me@example.com"),
        vec![Address::new("you@example.com")],
        "主题",
    );
    m.cc.push(Address::new("cc@example.com"));
    m.bcc.push(Address::new("bcc@example.com"));
    assert!(m.validate().is_ok());
    assert_eq!(m.recipients().len(), 3);
    m.to.clear();
    m.cc.clear();
    m.bcc.clear();
    assert!(m.validate().is_err());
}

#[test]
fn error_http_mapping_and_retry_policy() {
    assert_eq!(
        MailError::from_http(401, "x").kind(),
        ErrorKind::Authentication
    );
    assert_eq!(MailError::from_http(404, "x").kind(), ErrorKind::NotFound);
    assert_eq!(
        MailError::from_http(429, "x").kind(),
        ErrorKind::RateLimited
    );
    assert_eq!(MailError::from_http(507, "x").kind(), ErrorKind::Quota);
    assert_eq!(MailError::from_http(503, "x").kind(), ErrorKind::Server);
    assert!(MailError::from_http(409, "conflict").requires_full_resync());
    assert!(MailError::Conflict("s".into()).requires_full_resync());
    assert!(MailError::Network("down".into()).is_retryable());
    assert!(!MailError::Protocol("bad".into()).is_retryable()); // 协议错误终止（A-006）
    assert!(!MailError::InvalidInput("x".into()).is_retryable());
}

#[test]
fn conflict_priority_delete_beats_move_beats_flag() {
    let del = OperationKind::Delete {
        email_ids: vec!["e".into()],
        from: String::new(),
    };
    let mv = OperationKind::Move {
        email_ids: vec!["e".into()],
        from: "a".into(),
        to: "b".into(),
    };
    let flag = OperationKind::SetFlag {
        email_id: "e".into(),
        flag: Flag::Seen,
        enabled: true,
    };
    assert!(conflict_priority(&del) > conflict_priority(&mv));
    assert!(conflict_priority(&mv) > conflict_priority(&flag));
}

#[test]
fn cancellation_and_progress() {
    let c = CancellationToken::new();
    assert!(c.check().is_ok());
    c.cancel();
    assert_eq!(c.check().unwrap_err(), MailError::Cancelled);
    let p = Progress {
        phase: SyncPhase::Headers,
        processed: 250,
        total: 1000,
    };
    assert_eq!(p.percent(), 25);
    assert_eq!(
        Progress {
            phase: SyncPhase::Done,
            processed: 0,
            total: 0
        }
        .percent(),
        0
    );
}

#[test]
fn domain_model_serde_roundtrip() {
    let mut e = Email::new("e1", "acc", "inbox");
    e.subject = "测试".into();
    e.from.push(Address::with_name("发件人", "a@b.com"));
    e.set_flag(Flag::Flagged, true);
    e.attachments
        .push(Attachment::new("at1", "application/pdf"));
    let json = serde_json::to_string(&e).unwrap();
    let back: Email = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);
}

// ------------------------------------------- 陈旧主键重定向（S6-15）

/// 本地主键 `账号/文件夹/UID` 里的文件夹是同步快照；`copy_emails`/`delete_emails`
/// 的签名里没有 `from`，只能靠这段改写把操作打回邮件**当前**所在的文件夹。
#[test]
fn retarget_replaces_stale_folder_segment() {
    assert_eq!(
        retarget_email_id("acc/INBOX/42", "Archive"),
        "acc/Archive/42"
    );
    // 同文件夹 → 不必改写
    assert_eq!(retarget_email_id("acc/INBOX/42", "INBOX"), "acc/INBOX/42");
    // 空 from = 调用方没有更权威的位置，沿用 id 内嵌值
    assert_eq!(retarget_email_id("acc/INBOX/42", ""), "acc/INBOX/42");
}

/// 只认「末段是纯数字 UID」的形态，其他一律原样返回，否则会把 JMAP 的服务端 id 改坏。
#[test]
fn retarget_leaves_non_imap_ids_alone() {
    for id in [
        "qeaaaaeb",        // JMAP 服务端 emailId
        "a/b",             // 末段不是 UID
        "acc/INBOX",       // 只有两段
        "acc/INBOX/",      // 空 UID
        "acc/INBOX/uid-1", // 非数字 UID
    ] {
        assert_eq!(retarget_email_id(id, "Archive"), id, "不该改写 {id}");
    }
}

/// 文件夹自身可以带 `/`（层次文件夹），所以只能从右侧切 UID、从左侧切账号。
#[test]
fn retarget_handles_nested_folder_names() {
    assert_eq!(
        retarget_email_id("acc/Archive/2024/42", "INBOX"),
        "acc/INBOX/42"
    );
    assert_eq!(
        retarget_email_id("acc/INBOX/42", "Archive/2024"),
        "acc/Archive/2024/42"
    );
    assert_eq!(folder_segment("acc/Archive/2024/42"), Some("Archive/2024"));
    assert_eq!(folder_segment("qeaaaaeb"), None);
}

/// remap 之后，`Delete`/`Copy` 的 `from` 必须跟着指向新文件夹。
///
/// 不这么做的话，回放层会拿「刚搬完的新主键」配「旧文件夹」再重定向一次，
/// 把邮件又指回原处——等于亲手把 remap 抹掉，然后对着不存在的 UID 空转。
#[test]
fn remap_moves_mailbox_hint_for_copy_and_delete() {
    let old = "acc/INBOX/42".to_string();
    let new = "acc/Trash/99".to_string();

    let mut del = OperationKind::Delete {
        email_ids: vec![old.clone()],
        from: "INBOX".to_string(),
    };
    del.remap_email_id(&old, &new);
    assert_eq!(
        del,
        OperationKind::Delete {
            email_ids: vec![new.clone()],
            from: "Trash".to_string(),
        }
    );

    let mut cp = OperationKind::Copy {
        email_ids: vec![old.clone()],
        from: "INBOX".to_string(),
        to: "Archive".to_string(),
    };
    cp.remap_email_id(&old, &new);
    assert_eq!(
        cp,
        OperationKind::Copy {
            email_ids: vec![new],
            from: "Trash".to_string(),
            to: "Archive".to_string(),
        }
    );
}

/// 反向保护：`Move`/`Archive` 的 `from` 是**源文件夹**，不该被邮件主键重映射动到。
#[test]
fn remap_keeps_source_folder_for_move_and_archive() {
    let old = "acc/INBOX/42".to_string();
    let new = "acc/Archive/77".to_string();
    let mut mv = OperationKind::Move {
        email_ids: vec![old.clone()],
        from: "INBOX".to_string(),
        to: "Archive".to_string(),
    };
    mv.remap_email_id(&old, &new);
    assert_eq!(
        mv,
        OperationKind::Move {
            email_ids: vec![new],
            from: "INBOX".to_string(),
            to: "Archive".to_string(),
        }
    );
}
