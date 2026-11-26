#!/usr/bin/env python3
"""跨语言冒烟：用 uniffi 生成的 Python 绑定真实调用 Rust 核心库（S5-01）。

这个脚本存在的意义：绑定"生成成功"和"能调通"是两件事。这里真的通过
ctypes → Rust FFI → 业务层跑一遍账号创建与查询，验证导出的符号、数据
结构映射、错误类型在真实进程里都成立。
"""
import base64
import json
import os
import sys
import tempfile
import threading
import time
import urllib.request

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(_HERE, "..", "build", "bindings", "python"))

import crossmail_ffi as cm


def jmap_session(url, user, password):
    """GET JMAP session，返回 (apiUrl, 邮件 accountId, 认证头)。仅用标准库。"""
    auth = base64.b64encode(f"{user}:{password}".encode()).decode()
    headers = {"Authorization": "Basic " + auth}
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=20) as r:
        session = json.load(r)
    return (
        session["apiUrl"],
        session["primaryAccounts"]["urn:ietf:params:jmap:mail"],
        headers,
    )


def jmap_call(api_url, headers, method_calls):
    """POST 一次 JMAP 方法调用，返回 methodResponses。仅用标准库。"""
    body = json.dumps({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": method_calls,
    }).encode()
    req = urllib.request.Request(
        api_url, data=body,
        headers={**headers, "Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)["methodResponses"]


def main() -> int:
    tempdir_root = None
    print("[1] core_status / protocol_display_name")
    status = cm.core_status()
    assert status.version, "版本不能为空"
    assert "jmap" in status.supported_protocols, status.supported_protocols
    assert cm.protocol_display_name("jmap") == "JMAP (RFC 8620)"
    print(f"    version={status.version} protocols={status.supported_protocols}")

    print("[2] open_engine（内存库 + 临时缓存目录）")
    with tempfile.TemporaryDirectory() as cache_dir:
        tempdir_root = cache_dir
        cfg = cm.EngineConfig(
            db_path=":memory:",
            cache_dir=cache_dir,
            cache_quota_bytes=8 * 1024 * 1024,
            master_key_b64=None,
        )
        engine = cm.open_engine(cfg)
        assert engine.list_accounts() == [], "新库不应有账号"

        print("[3] create_account（JMAP）")
        account = engine.create_account(
            cm.NewAccountDto(
                email_address="user@example.com",
                display_name="Test User",
                protocol=cm.ProtocolKindDto.JMAP,
                secret="python-side-secret",
                jmap_endpoint="https://jmap.example.com",
                imap_host=None,
                imap_port=None,
                imap_security=None,
                smtp_host=None,
                smtp_port=None,
                smtp_security=None,
                sync_days=30,
                poll_interval_secs=300,
            )
        )
        assert account.email_address == "user@example.com"
        assert account.display_name == "Test User"
        assert account.jmap_endpoint == "https://jmap.example.com"
        print(f"    id={account.id}")

        print("[4] ensure_system_mailboxes")
        boxes = engine.ensure_system_mailboxes(account.id)
        # 简单 enum 在 Python 侧是同一个类的不同实例，按 repr 取角色名
        roles = {repr(b.role).rsplit(".", 1)[-1].upper() for b in boxes}
        assert len(boxes) >= 6, boxes
        print(f"    文件夹 {len(boxes)} 个，角色: {sorted(roles)}")
        assert len(roles) == 6, roles

        print("[5] save_draft / list_drafts / read_body")
        draft = cm.DraftDto(
            to=[cm.AddressDto(name="Bob", email="bob@example.com")],
            cc=[],
            bcc=[],
            subject="跨语言冒烟",
            text_body="从 Python 写入的正文",
            html_body=None,
            in_reply_to=None,
            references=[],
        )
        saved = engine.save_draft(account.id, draft)
        assert saved.subject == "跨语言冒烟"
        drafts = engine.list_drafts(account.id, 10)
        assert len(drafts) == 1, drafts

        body = engine.read_body(saved.id, False)
        assert body == "从 Python 写入的正文", body
        # 请求 HTML 时应单向回退到纯文本，不返回空
        assert engine.read_body(saved.id, True) is not None

        print("[6] set_read / toggle_star / search")
        # 注意：uniffi 0.28.3 的 Python 后端为带值 enum 生成的 __eq__ 是坏的
        # （内部调用 other.is_draft() 却没绑 self，一比较就 TypeError），
        # 所以这里按类型名断言，既绕开该缺陷又照样验证了值确实跨语言传对了。
        def flag_names(flags):
            return {type(f).__name__.upper() for f in flags}

        read = engine.set_read(saved.id, True)
        assert any("SEEN" in n for n in flag_names(read.flags)), read.flags
        starred = engine.toggle_star(saved.id, True)
        assert any("FLAGGED" in n for n in flag_names(starred.flags)), starred.flags

        page = engine.search(account.id, "冒烟", cm.SearchScopeDto.SUBJECT, 10, 0)
        assert page.total == 1, page.total
        assert page.items[0].subject == "跨语言冒烟"

        print("[7] archive / delete（进回收站）")
        archived = engine.archive_email(saved.id)
        assert archived.subject == "跨语言冒烟"
        outcome = engine.delete_email(saved.id)
        assert outcome.trashed is True, outcome.trashed

        print("[8] 错误类型跨语言可捕获")
        try:
            engine.get_email("不存在的邮件")
        except cm.CrossMailError.NotFound as e:
            print(f"    捕获 NotFound: {e}")
        else:
            raise AssertionError("应当抛出 CrossMailError.NotFound")

        print("[9] cache_status / clear_cache")
        cache = engine.cache_status()
        assert cache.quota_bytes == 8 * 1024 * 1024
        engine.clear_cache()

        print("[10] render_body：HTML 净化 + 远程资源默认阻断（S6-01/02）")
        evil = cm.DraftDto(
            to=[cm.AddressDto(name="Bob", email="bob@example.com")],
            cc=[],
            bcc=[],
            subject="安全渲染冒烟",
            text_body="纯文本回退",
            html_body=(
                "<p>hi</p>"
                '<img src="https://tracker.example.com/p.gif">'
                '<script>alert(1)</script>'
                '<a href="javascript:alert(2)">click</a>'
                '<div style="background-image:url(https://t.example.com/x.gif)">x</div>'
            ),
            in_reply_to=None,
            references=[],
        )
        evil_saved = engine.save_draft(account.id, evil)
        rendered = engine.render_body(evil_saved.id, True, False)
        assert rendered.html is not None
        assert "script" not in rendered.html, rendered.html
        assert "javascript:" not in rendered.html, rendered.html
        assert "tracker.example.com" not in rendered.html, "远程图片必须被替换"
        assert "url(" not in rendered.html, "CSS 追踪必须被清掉"
        assert rendered.blocked_count >= 2, rendered.blocked_count
        assert rendered.remote_images_allowed is False
        assert rendered.removed_tag_count >= 1, rendered.removed_tag_count
        assert rendered.trust_level == "safe", rendered.trust_level
        print(
            f"    拦截 {rendered.blocked_count} 项远程资源，移除 "
            f"{rendered.removed_tag_count} 个危险标签，trust={rendered.trust_level}"
        )

        once = engine.render_body(evil_saved.id, True, True)
        assert once.remote_images_allowed is True
        # 放行后图片那条不再计为拦截，但 javascript: 链接依然被拦（协议白名单与资源策略独立）
        assert once.blocked_count == rendered.blocked_count - 1, once.blocked_count
        assert "tracker.example.com" in once.html, "放行后应保留原始地址"
        assert "javascript:" not in once.html, "放行远程资源不得放行脚本协议"

        print("[11] sender_trust")
        trust = engine.sender_trust(evil_saved.id)
        assert trust.address == "user@example.com", trust.address
        assert trust.level in ("safe", "caution", "danger"), trust.level
        print(f"    {trust.address} -> {trust.level} ({trust.domain})")

        print("[12] allow / list / revoke 远程资源放行")
        key = engine.allow_sender_remote_images(evil_saved.id, "sender")
        assert key == "user@example.com", key
        assert len(engine.list_remote_allowances()) == 1
        again = engine.render_body(evil_saved.id, True, False)
        assert again.remote_images_allowed is True, "白名单应让后续渲染直接放行"
        removed = engine.revoke_sender_remote_images(evil_saved.id)
        assert removed >= 1, removed
        assert engine.list_remote_allowances() == []
        # 撤销后回到默认阻断
        assert engine.render_body(evil_saved.id, True, False).remote_images_allowed is False

        print("[13] 日志分级 / 轮转 / 诊断包导出（S6-04）")
        st = engine.log_status()
        assert st.level == "info", st.level
        assert st.protocol_payload is False, "协议报文必须默认关闭"
        assert engine.set_log_level("debug") == "debug"
        assert engine.log_status().level == "debug"
        engine.set_protocol_payload_logging(True)
        assert engine.log_status().protocol_payload is True
        engine.set_protocol_payload_logging(False)
        engine.rotate_logs()

        diag_dir = os.path.join(tempdir_root, "diag")
        os.makedirs(diag_dir, exist_ok=True)
        diag_path = engine.export_diagnostics(diag_dir)
        assert os.path.isfile(diag_path), diag_path
        assert os.path.getsize(diag_path) > 1024, "诊断包过小，可能没打进日志"
        with open(diag_path, "rb") as f:
            head = f.read()
        assert b"manifest.json" in head, "诊断包应含 manifest"
        assert b"user@example.com" not in head, "诊断包必须脱敏（不得出现完整邮箱）"
        assert b"manifest.json" in head
        # 脱敏规则：本地部分 ≤2 位时整段掩成 ***，域名保留（定位问题需要知道是哪家服务器）
        assert b"***@example.com" in head, "诊断包应保留脱敏后的邮箱"
        assert b"user@example.com" not in head
        print(f"    诊断包 {os.path.getsize(diag_path)} 字节 -> {diag_path}")

        print("[14] 推送回调跨语言送达：sync_account + subscribe_push（S6-16）")
        # 可控假源：IMAP 账号显式声明 STARTTLS。当前传输层只实现隐式 TLS，
        # build_backend 在装配阶段就确定性地返回 Unsupported —— 零网络请求、
        # 不依赖真服务器与网络时序。这样能真跑一次 callback interface，
        # 同时验证错误类型映射与 PushEventDto 的跨语言拍平。
        push_account = engine.create_account(
            cm.NewAccountDto(
                email_address="push@example.com",
                display_name=None,
                protocol=cm.ProtocolKindDto.IMAP,
                secret="push-secret",
                jmap_endpoint=None,
                imap_host="imap.example.com",
                imap_port=143,
                imap_security=cm.SecurityModeDto.START_TLS,
                smtp_host=None,
                smtp_port=None,
                smtp_security=None,
                sync_days=None,
                poll_interval_secs=None,
            )
        )

        # 一次性同步：Unsupported 必须映射成可精确捕获的异常类
        try:
            engine.sync_account(push_account.id)
        except cm.CrossMailError.Unsupported as e:
            assert "STARTTLS" in str(e), str(e)
        else:
            raise AssertionError("STARTTLS 账号同步应抛 CrossMailError.Unsupported")

        # 真跑回调：Python 对象注册进 Rust，由 crossmail-app 的专用派发线程
        # 重新获取 GIL 后回调。PushObserver 是 duck-typing Protocol，实现 on_event 即可。
        events = []
        done = threading.Event()

        class Recorder:
            def on_event(self, event):
                events.append(event)
                if event.kind == cm.PushEventKindDto.CLOSED:
                    done.set()

        observer = Recorder()
        sub = engine.subscribe_push(push_account.id, observer)
        assert sub.is_cancelled() is False
        assert done.wait(timeout=10), "10 秒内应收到 Closed 事件（回调未跨语言送达）"

        failed = [e for e in events if e.kind == cm.PushEventKindDto.FAILED]
        closed = [e for e in events if e.kind == cm.PushEventKindDto.CLOSED]
        assert len(failed) == 1, events
        assert len(closed) == 1, events
        assert "STARTTLS" in failed[0].error, failed[0].error
        assert failed[0].reason == "push", failed[0].reason
        # 角标字段跨语言可读（失败事件恒为 0；Synced 带真实值的映射见 FFI Rust 单测）
        assert failed[0].unread_total == 0, failed[0].unread_total
        assert failed[0].unread_by_mailbox == [], failed[0].unread_by_mailbox
        assert failed[0].retry_in_ms is None, failed[0].retry_in_ms
        assert closed[0].reason == "", closed[0].reason
        assert closed[0].error == "", closed[0].error
        print(f"    回调送达 {len(events)} 条: {[e.kind.name for e in events]}")

        sub.cancel()
        assert sub.is_cancelled() is True

        print("[15] 真连服务器触发 Synced + 真实未读角标跨语言回调（S6-16 live，可选）")
        # 门控：未设真机凭据就静默跳过，离线 CI / 无服务器环境完全不受影响。
        live_url = os.environ.get("CROSSMAIL_LIVE_JMAP_URL")
        live_user = os.environ.get("CROSSMAIL_LIVE_JMAP_USER")
        live_pass = os.environ.get("CROSSMAIL_LIVE_JMAP_PASS")
        if not (live_url and live_user and live_pass):
            print("    跳过：未设置 CROSSMAIL_LIVE_JMAP_URL/USER/PASS")
        else:
            api_url, remote_account, headers = jmap_session(live_url, live_user, live_pass)

            def call(calls):
                return jmap_call(api_url, headers, calls)

            # 真机 folder 是短 id（如 Drafts=`d`），注入一封草稿制造真实 StateChange。
            boxes = call([["Mailbox/get", {"accountId": remote_account, "ids": None}, "c0"]])
            drafts = next(m["id"] for m in boxes[0][1]["list"] if m.get("role") == "drafts")

            live_account = engine.create_account(
                cm.NewAccountDto(
                    email_address=live_user, display_name=None,
                    protocol=cm.ProtocolKindDto.JMAP, secret=live_pass,
                    jmap_endpoint=live_url, imap_host=None, imap_port=None,
                    imap_security=None, smtp_host=None, smtp_port=None,
                    smtp_security=None, sync_days=None, poll_interval_secs=None,
                )
            )
            # sync_account 此前只测过 STARTTLS 的 Unsupported 错误映射；这里验真连成功路径。
            report = engine.sync_account(live_account.id)
            assert report.inserted >= 0, report

            live_events = []
            synced = threading.Event()

            class LiveRecorder:
                def on_event(self, ev):
                    live_events.append(ev)
                    if ev.kind == cm.PushEventKindDto.SYNCED:
                        synced.set()

            live_sub = engine.subscribe_push(live_account.id, LiveRecorder())
            created = []
            tag = f"CrossMail-live-smoke-{os.getpid()}"
            try:
                # worker 异步建连且没有「已就绪」信号，故轮询注入直到收到 Synced，
                # 不猜延迟；每次注入都会产生一次真实 StateChange。
                for i in range(8):
                    create = {"k": {
                        "mailboxIds": {drafts: True},
                        "keywords": {"$draft": True},
                        "from": [{"email": live_user}],
                        "to": [{"email": live_user}],
                        "cc": [], "bcc": [],
                        "subject": f"{tag}-{i}",
                        "messageId": [f"<{tag}-{i}@probe>"],
                        "bodyValues": {"body": {"value": "live smoke", "charset": "utf-8", "type": "text/plain"}},
                        "textBody": [{"partId": "body", "type": "text/plain"}],
                        "htmlBody": [], "attachments": [],
                    }}
                    resp = call([["Email/set", {"accountId": remote_account, "create": create}, "c0"]])
                    cid = resp[0][1].get("created", {}).get("k", {}).get("id")
                    if cid:
                        created.append(cid)
                    if synced.wait(timeout=3):
                        break
                    time.sleep(1)

                assert synced.is_set(), (
                    f"真连后未收到 Synced 事件，实际: {[e.kind.name for e in live_events]}"
                )
                ev = next(e for e in live_events if e.kind == cm.PushEventKindDto.SYNCED)
                # 角标必须是真机库里的真实非零值，且与分箱明细自洽——不是编造的 0。
                assert ev.unread_total > 0, ev.unread_total
                assert ev.unread_total == sum(m.unread for m in ev.unread_by_mailbox), ev.unread_by_mailbox
                assert len(ev.unread_by_mailbox) >= 1, "应带分箱明细"
                assert all(m.mailbox_id and m.unread > 0 for m in ev.unread_by_mailbox), ev.unread_by_mailbox
                assert ev.reason == "push", ev.reason
                assert ev.error == "", ev.error
                assert ev.retry_in_ms is None, ev.retry_in_ms
                print(
                    f"    真连 Synced: unread_total={ev.unread_total}, "
                    f"分箱={[(m.mailbox_id, m.unread) for m in ev.unread_by_mailbox]}, "
                    f"注入草稿 {len(created)} 封"
                )
            finally:
                live_sub.cancel()
                # 真服务器数据必须清理；前缀固定便于异常中断后人工识别残留。
                if created:
                    try:
                        call([["Email/set", {"accountId": remote_account, "destroy": created}, "c0"]])
                    except Exception as cleanup_err:  # noqa: BLE001 - 清理失败只告警
                        print(f"    清理注入草稿失败（前缀 {tag}）: {cleanup_err}")


    print("\n[OK] Python 绑定端到端调通（真实加载 .so，非模拟）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
