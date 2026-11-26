#!/usr/bin/env python3
"""CrossMail 真实账号端到端验证（Windows 本地库，真加载 x64 DLL + 真连 IMAP）。

用途：在Test User于绿色版账号页手工录入测试账号后，验证「持久化主密钥 -> vault 解密
-> IMAP 993 隐式 TLS -> 邮件落本地库 -> 三栏数据源」这条完整链路。

红线：
- 不读取、不回显任何明文口令；vault 密文只由 Rust 引擎在进程内解密。
- 不修改 Rust 核心 / UDL / FFI / C# 绑定，不新增依赖。
- 只读核对账号配置（凭据字段打码）；sync_account / subscribe_push 是正常业务调用，
  不调 ensure_system_mailboxes 等会写入本地文件夹的接口。
- 不连 Self-hosted server 管理接口、不上云、不跨机复制凭据。

同步失败不当成脚本崩溃：真实错误原样落盘并继续验证推送回调 vtable。
RESULT 四态：OK（同步 + 正文两入口都通）/ BLOCKED_BY_CORE（核心侧阻断）/
BODY_MISSING（正文按需回源未生效）/ FAIL（脚本自身错误）。
结果同时写 stdout 与 win/live-verify-result.txt。
"""
from __future__ import annotations

import os
import shutil
import sqlite3
import sys
import tempfile
import threading
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
WIN = os.path.dirname(HERE)
ROOT = os.path.dirname(WIN)
# 权威构建产物：重编 Rust 核心后这里立刻是新 DLL，不依赖用户解压目录是否刷新。
DLL = os.path.join(
    ROOT, "crossmail", "target", "x86_64-pc-windows-msvc", "release", "crossmail_ffi.dll"
)
BINDING = os.path.join(ROOT, "crossmail", "build", "bindings", "python", "crossmail_ffi.py")
DATA = os.path.join(os.environ["LOCALAPPDATA"], "CrossMail")
DB = os.path.join(DATA, "crossmail.db")
KEY = os.path.join(DATA, "master.key")
RESULT = os.path.join(WIN, "live-verify-result.txt")


def scrub(obj):
    """只读展示配置结构，任何凭据类字段一律打码。"""
    if isinstance(obj, dict):
        return {
            k: ("<redacted>" if k.lower() in ("credential", "secret", "password", "token") else scrub(v))
            for k, v in obj.items()
        }
    if isinstance(obj, list):
        return [scrub(x) for x in obj]
    return obj


def db_snapshot():
    """只读打开 SQLite，绝不解密 / 不打印 sealed 字段。"""
    con = sqlite3.connect("file:" + DB.replace("\\", "/") + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    try:
        tables = [r[0] for r in con.execute("select name from sqlite_master where type='table'")]
        counts = {}
        for t in ("accounts", "secrets", "emails", "mailboxes", "email_mailboxes", "sync_state"):
            if t in tables:
                counts[t] = con.execute("select count(*) from " + t).fetchone()[0]
        row = con.execute("select id, email_address, protocol, config_json from accounts").fetchone()
        account = None
        if row:
            account = {
                "id": row["id"],
                "email_address": row["email_address"],
                "protocol": row["protocol"],
                "config": scrub(sqlite3_json(row["config_json"])),
            }
        return counts, account
    finally:
        con.close()


def sqlite3_json(raw):
    import json
    return json.loads(raw) if raw else {}


def main() -> int:
    lines: list[str] = []

    def out(key: str, value) -> None:
        text = f"{key}={value}"
        lines.append(text)
        print(text, flush=True)

    try:
        for label, path in (("dll", DLL), ("binding", BINDING), ("db", DB), ("master.key", KEY)):
            if not os.path.isfile(path):
                raise FileNotFoundError(f"{label} 不存在: {path}")
            out(f"file.{label}.bytes", os.path.getsize(path))

        before_counts, account = db_snapshot()
        out("db.before", json_dumps(before_counts))
        if not account:
            raise RuntimeError("本地库无账号，请先在客户端账号页录入并新建账号")
        out("account.id", account["id"])
        out("account.email", account["email_address"])
        out("account.protocol", account["protocol"])
        out("account.config", json_dumps(account["config"]))

        # UniFFI Python 后端在 Windows 上按「与 .py 同目录的 uniffi_<name>.dll」加载。
        # 用临时目录装配，不改动仓库里任何构建产物。
        tmp = tempfile.mkdtemp(prefix="crossmail-live-")
        try:
            shutil.copy2(BINDING, os.path.join(tmp, "crossmail_ffi.py"))
            shutil.copy2(DLL, os.path.join(tmp, "uniffi_crossmail_ffi.dll"))
            sys.path.insert(0, tmp)
            import crossmail_ffi as cm

            with open(KEY, "r", encoding="utf-8") as f:
                master_key = f.read().strip()
            out("master_key.chars", len(master_key))

            cfg = cm.EngineConfig(
                db_path=DB,
                cache_dir=os.path.join(DATA, "cache"),
                cache_quota_bytes=64 * 1024 * 1024,
                master_key_b64=master_key,
            )
            engine = cm.open_engine(cfg)
            out("engine.opened", type(engine).__name__)

            accounts = engine.list_accounts()
            out("engine.accounts", len(accounts))
            account_id = accounts[0].id
            out("engine.account_id_matches", account_id == account["id"])

            # 阶段 B：一次性同步。失败不当成脚本崩溃，而是把真实错误与根因如实落盘，
            # 并继续跑后面的推送回调验证，避免一次阻断掩盖整条链路的真值。
            sync_ok = False
            try:
                report = engine.sync_account(account_id)
                sync_ok = True
                out("sync.status", "OK")
                out(
                    "sync.report",
                    json_dumps(
                        {
                            "inserted": report.inserted,
                            "updated": report.updated,
                            "deleted": report.deleted,
                            "unchanged": report.unchanged,
                            "has_more": report.has_more,
                            "full_resync": report.full_resync,
                            "new_state": report.new_state,
                        }
                    ),
                )
            except Exception as sync_exc:  # noqa: BLE001 - 真实核心错误必须原样保留
                sync_error = f"{type(sync_exc).__name__}: {sync_exc}"
                out("sync.status", "FAILED")
                out("sync.error", sync_error)
                if "IMAP 增量同步必须指定文件夹" in sync_error:
                    out(
                        "sync.root_cause",
                        "crates/app/src/drive.rs:82 sync_with_report(account_id, None, ...) "
                        "对 IMAP 传空 mailbox_id；crates/imap/src/provider.rs:466 强制要求。"
                        "UDL 只暴露 sync_account(account_id)，Windows 侧没有按文件夹同步入口。",
                    )
                    out("sync.root_cause_layer", "rust-core-app-orchestration")

            # 阶段 B2：无论同步成败，都读一次三栏数据源，如实反映落库状态。
            mailboxes = engine.list_mailboxes(account_id)
            out("engine.mailboxes", len(mailboxes))
            out("engine.mailbox_names", json_dumps(sorted(m.name for m in mailboxes)))
            unread = engine.unread_counts(account_id)
            out("engine.unread_counts", [(u.mailbox_id, u.unread) for u in unread])

            page = engine.list_emails(
                account_id,
                cm.EmailQueryDto(mailbox_id=None, unread_only=False, limit=50, offset=0),
            )
            out("engine.emails.total", page.total)
            out("engine.emails.subjects", json_dumps([e.subject for e in page.items]))

            body_ok = False
            if page.items:
                body = engine.read_body(page.items[0].id, False)
                body_ok = bool(body)
                out("engine.read_body.chars", len(body or ""))
                out("engine.read_body.head", json_dumps((body or "")[:60]))
            out("engine.read_body.ok", body_ok)

            # 阅读窗格同时走 render_body（Windows MailService 一次取四件套），
            # 两个入口都必须能拿到正文；首次调用会按需回源并落正文缓存。
            rendered_ok = False
            if page.items:
                rb = engine.render_body(page.items[0].id, False, False)
                rendered_text = rb.text or rb.html or ""
                rendered_ok = bool(rendered_text.strip())
                out("engine.render_body.text_chars", len(rendered_text))
                out("engine.render_body.head", json_dumps(rendered_text[:60]))
            out("engine.render_body.ok", rendered_ok)

            # 阶段 C：真订阅推送。核心修好后 worker 会持续 IDLE 监听，
            # 所以先观察一小段初始事件，再主动 cancel 收尾并等 Closed。
            # 核心若仍有缺陷，worker 会自发 Failed + Closed，同样如实记录。
            events: list = []
            closed = threading.Event()

            class Recorder:
                def on_event(self, event):
                    events.append(event)
                    if event.kind == cm.PushEventKindDto.CLOSED:
                        closed.set()

            sub = engine.subscribe_push(account_id, Recorder())
            out("push.subscribed", sub.is_cancelled() is False)
            threading.Event().wait(timeout=6)
            out("push.events_before_cancel", json_dumps([e.kind.name for e in events]))
            sub.cancel()
            out("push.cancelled", sub.is_cancelled() is True)
            closed_ok = closed.wait(timeout=20)
            out("push.closed_received", closed_ok)
            out("push.event_kinds", json_dumps([e.kind.name for e in events]))
            failed = [e for e in events if e.kind == cm.PushEventKindDto.FAILED]
            synced = [e for e in events if e.kind == cm.PushEventKindDto.SYNCED]
            out("push.failed_errors", json_dumps([e.error for e in failed]))
            out("push.synced_unread", json_dumps([e.unread_total for e in synced]))
            push_ok = closed_ok and sub.is_cancelled() and not failed
            out("PUSH_CALLBACK_OK", push_ok)

            after_counts, _ = db_snapshot()
            out("db.after", json_dumps(after_counts))
            out("db.emails_delta", after_counts.get("emails", 0) - before_counts.get("emails", 0))
            out("LIVE_SYNC_OK", sync_ok)
            out("BODY_FETCH_OK", bool(body_ok and rendered_ok))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

        # RESULT 是验证结论，不是验证动作是否成功：核心阻断也属「已真验证」。
        # 正文按需回源是阅读窗格可用的硬门槛，单独判一档避免被 sync 成功掩盖。
        if not sync_ok:
            result = "BLOCKED_BY_CORE"
        elif not (body_ok and rendered_ok):
            result = "BODY_MISSING"
        else:
            result = "OK"
        out("RESULT", result)
        return 0 if result == "OK" else 2
    except Exception as exc:  # noqa: BLE001 - 完整错误要落到结果文件
        out("RESULT", "FAIL")
        out("ERROR", f"{type(exc).__name__}: {exc}")
        lines.append(traceback.format_exc())
        print(traceback.format_exc(), file=sys.stderr, flush=True)
        return 1
    finally:
        with open(RESULT, "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join(lines) + "\n")


def json_dumps(obj) -> str:
    import json
    return json.dumps(obj, ensure_ascii=False, sort_keys=True, default=str)


if __name__ == "__main__":
    sys.exit(main())
