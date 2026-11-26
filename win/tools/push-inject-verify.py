#!/usr/bin/env python3
"""CrossMail 真实 IDLE 推送端到端验证：投一封真邮件，等 Synced 角标事件。

链路：Self-hosted server Stalwart 自发自收投信 → IMAP IDLE(INBOX) 收到 EXISTS →
Rust run_push_sync 增量同步 → PushObserver 跨语言回调 Synced（带真实未读数）。

红线：
- 凭据不跨机：投信由 `ssh mail-server` 在 Self-hosted server 本机执行 mailctl.py，
  Windows 侧不读取、不传递、不回显任何口令。
- 只读核对本地库，同步/推送均为正常业务调用；不改 Rust / UDL / FFI / 绑定。
- 注入邮件留在专用测试账号内，主题带唯一 tag 便于识别，不做静默删除。

结果写 stdout 与 win/push-inject-result.txt。
"""
from __future__ import annotations

import json
import os
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
WIN = os.path.dirname(HERE)
ROOT = os.path.dirname(WIN)
DLL = os.path.join(
    ROOT, "crossmail", "target", "x86_64-pc-windows-msvc", "release", "crossmail_ffi.dll"
)
BINDING = os.path.join(ROOT, "crossmail", "build", "bindings", "python", "crossmail_ffi.py")
DATA = os.path.join(os.environ["LOCALAPPDATA"], "CrossMail")
DB = os.path.join(DATA, "crossmail.db")
KEY = os.path.join(DATA, "master.key")
RESULT = os.path.join(WIN, "push-inject-result.txt")

SSH_HOST = "mail-server"
SEND_CLI = "python3 /srv/data/stacks/mail/mailctl.py send"
RECIPIENT = "user@example.com"


def counts():
    con = sqlite3.connect("file:" + DB.replace("\\", "/") + "?mode=ro", uri=True)
    try:
        return {
            t: con.execute("select count(*) from " + t).fetchone()[0]
            for t in ("emails", "email_mailboxes", "sync_state", "accounts")
        }
    finally:
        con.close()


def main() -> int:
    lines: list[str] = []

    def out(key: str, value) -> None:
        text = f"{key}={value}"
        lines.append(text)
        print(text, flush=True)

    tag = "CrossMail-IDLE-live-%d-%d" % (time.time(), os.getpid())
    try:
        for label, path in (("dll", DLL), ("binding", BINDING), ("db", DB), ("key", KEY)):
            if not os.path.isfile(path):
                raise FileNotFoundError(f"{label} 不存在: {path}")

        before = counts()
        out("db.before", json_dumps(before))
        out("inject.tag", tag)

        tmp = tempfile.mkdtemp(prefix="crossmail-push-")
        try:
            shutil.copy2(BINDING, os.path.join(tmp, "crossmail_ffi.py"))
            shutil.copy2(DLL, os.path.join(tmp, "uniffi_crossmail_ffi.dll"))
            sys.path.insert(0, tmp)
            import crossmail_ffi as cm

            with open(KEY, "r", encoding="utf-8") as f:
                master_key = f.read().strip()
            engine = cm.open_engine(
                cm.EngineConfig(
                    db_path=DB,
                    cache_dir=os.path.join(DATA, "cache"),
                    cache_quota_bytes=64 * 1024 * 1024,
                    master_key_b64=master_key,
                )
            )
            account_id = engine.list_accounts()[0].id
            out("engine.account_id", account_id)

            events: list = []
            synced = threading.Event()
            closed = threading.Event()
            lock = threading.Lock()

            class Recorder:
                def on_event(self, event):
                    with lock:
                        events.append(event)
                    if event.kind == cm.PushEventKindDto.SYNCED:
                        synced.set()
                    if event.kind == cm.PushEventKindDto.CLOSED:
                        closed.set()

            sub = engine.subscribe_push(account_id, Recorder())
            out("push.subscribed", sub.is_cancelled() is False)

            # 等初始快照与 IDLE 连接建立；无「已就绪」信号，按真值给一段固定窗口。
            threading.Event().wait(timeout=8)
            out("push.events_before_inject", json_dumps([e.kind.name for e in events]))

            remote = (
                f"{SEND_CLI} --to {shlex.quote(RECIPIENT)} "
                f"--subject {shlex.quote(tag)} "
                f"--body {shlex.quote('live IDLE push probe from Windows')}"
            )
            proc = subprocess.run(
                ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", SSH_HOST, remote],
                capture_output=True,
                text=True,
                timeout=90,
            )
            out("inject.ssh_returncode", proc.returncode)
            out("inject.ssh_stdout", json_dumps((proc.stdout or "").strip()[:300]))
            out("inject.ssh_stderr", json_dumps((proc.stderr or "").strip()[:300]))

            synced_ok = synced.wait(timeout=75)
            out("push.synced_received", synced_ok)
            with lock:
                snapshot = list(events)
            out("push.event_kinds", json_dumps([e.kind.name for e in snapshot]))
            failed = [e for e in snapshot if e.kind == cm.PushEventKindDto.FAILED]
            out("push.failed_errors", json_dumps([e.error for e in failed]))

            synced_events = [
                e for e in snapshot if e.kind == cm.PushEventKindDto.SYNCED
            ]
            out("push.synced_count", len(synced_events))
            for i, ev in enumerate(synced_events):
                by = [(m.mailbox_id, m.unread) for m in ev.unread_by_mailbox]
                out(
                    f"push.synced[{i}]",
                    json_dumps(
                        {
                            "reason": ev.reason,
                            "inserted": ev.inserted,
                            "updated": ev.updated,
                            "unread_total": ev.unread_total,
                            "unread_by_mailbox": by,
                            "sum_ok": ev.unread_total == sum(n for _, n in by),
                            "coalesced": ev.coalesced,
                        }
                    ),
                )

            sub.cancel()
            out("push.cancelled", sub.is_cancelled() is True)
            out("push.closed_received", closed.wait(timeout=20))

            after = counts()
            out("db.after", json_dumps(after))
            out("db.emails_delta", after["emails"] - before["emails"])

            push_events = [e for e in synced_events if e.reason == "push"]
            ok = bool(
                synced_ok
                and push_events
                and push_events[0].unread_total > 0
                and push_events[0].unread_total
                == sum(m.unread for m in push_events[0].unread_by_mailbox)
                and after["emails"] > before["emails"]
                and not failed
            )
            out("PUSH_SYNCED_OK", ok)
            out("RESULT", "OK" if ok else "FAIL")
            return 0 if ok else 2
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
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
    return json.dumps(obj, ensure_ascii=False, sort_keys=True, default=str)


if __name__ == "__main__":
    sys.exit(main())
