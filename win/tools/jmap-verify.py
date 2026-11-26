#!/usr/bin/env python3
"""CrossMail JMAP 分支端到端验证（真加载 x64 DLL + 真连 Stalwart JMAP）。

为什么是「真实库副本 + 同 alias 复用密文」：
- 账号协议在建号时固定，`AccountPatchDto` 没有 `Protocol` 字段，官方接口切不了协议；
- 再让Test User手工输一次口令成本高，也没必要；
- 所以：把真实库与 master.key 拷到临时目录，在副本里追加一条 `protocol=jmap` 的账号，
  其 credential alias 指向**同一份本地密文**（`secrets` 表复制一行，alias 换成新账号的）。

红线：
- 明文口令全程不出现：脚本不解密、不打印、不落盘 sealed 字段，只由 Rust 引擎在进程内解。
- 不跨机复制凭据，不连 Self-hosted server 管理接口，不上云。
- 不改真实库（只在副本上写），不改 Rust 核心 / UDL / FFI / 绑定。

RESULT 三态：OK / FAILED（核心或网络真实失败，错误原样落盘）/ ERROR（脚本自身问题）。
"""
from __future__ import annotations

import json
import os
import shutil
import sqlite3
import sys
import tempfile
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
RESULT = os.path.join(WIN, "jmap-verify-result.txt")

# 按优先级尝试：公网 IPv6 直连（Let's Encrypt 正式证书）→ Tailscale Funnel 兜底。
ENDPOINTS = [
    "https://mail.example.com:19443/jmap/session",
    "https://mail.example.com:10000/jmap/session",
]
PROBE_ID = "jmap-probe"


def json_dumps(obj) -> str:
    return json.dumps(obj, ensure_ascii=False, sort_keys=True)


def scrub(obj):
    if isinstance(obj, dict):
        return {
            k: ("<redacted>" if k.lower() in ("credential", "secret", "password", "token") else scrub(v))
            for k, v in obj.items()
        }
    if isinstance(obj, list):
        return [scrub(x) for x in obj]
    return obj


def prepare_probe_db(tmp: str, endpoint: str) -> tuple[str, dict]:
    """在临时副本里追加一条 JMAP 账号，复用既有密文 alias。返回 (db 路径, 账号信息)。"""
    probe_db = os.path.join(tmp, "crossmail.db")
    shutil.copy2(DB, probe_db)

    con = sqlite3.connect(probe_db)
    con.row_factory = sqlite3.Row
    try:
        src = con.execute(
            "select id, email_address, display_name, config_json, sync_days, poll_interval_secs, created_at"
            " from accounts order by created_at limit 1"
        ).fetchone()
        if not src:
            raise RuntimeError("真实库无账号，无法复用凭据")
        cfg = json.loads(src["config_json"] or "{}")
        alias_src = None
        for key in ("imap", "jmap", "smtp"):
            node = cfg.get(key) or {}
            cred = node.get("credential") or {}
            if cred.get("alias"):
                alias_src = cred["alias"]
                break
        if not alias_src:
            raise RuntimeError("真实库账号无 credential alias，无法复用密文")

        sealed = con.execute("select sealed, updated_at from secrets where alias = ?", (alias_src,)).fetchone()
        if not sealed:
            raise RuntimeError("secrets 表缺少源密文行: " + alias_src)

        # alias 规则是「前缀 + 账号 id」，把源 id 换成探针 id 得到新 alias。
        src_id = src["id"]
        alias_new = (
            alias_src[: -len(src_id)] + PROBE_ID if alias_src.endswith(src_id) else alias_src + "-" + PROBE_ID
        )

        probe_cfg = {
            "jmap": {"endpoint": endpoint, "credential": {"type": "keyring", "alias": alias_new}},
            "imap": None,
            "smtp": None,
        }
        con.execute(
            "insert or replace into secrets(alias, sealed, updated_at) values(?,?,?)",
            (alias_new, sealed["sealed"], sealed["updated_at"]),
        )
        con.execute(
            "insert or replace into accounts"
            "(id, email_address, display_name, protocol, config_json, sync_days, poll_interval_secs, created_at)"
            " values(?,?,?,?,?,?,?,?)",
            (
                PROBE_ID,
                src["email_address"],
                src["display_name"],
                "jmap",
                json.dumps(probe_cfg),
                src["sync_days"],
                src["poll_interval_secs"],
                src["created_at"],
            ),
        )
        con.commit()
        return probe_db, {
            "email": src["email_address"],
            "alias_prefix_ok": alias_new != alias_src,
            "config": scrub(probe_cfg),
        }
    finally:
        con.close()


def main() -> int:
    lines: list[str] = []

    def out(key: str, value) -> None:
        text = f"{key}={value}"
        lines.append(text)
        print(text, flush=True)

    def write_result(verdict: str) -> None:
        out("RESULT", verdict)
        with open(RESULT, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")

    try:
        for label, path in (("dll", DLL), ("binding", BINDING), ("db", DB), ("master.key", KEY)):
            if not os.path.isfile(path):
                raise FileNotFoundError(f"{label} 不存在: {path}")
            out(f"file.{label}.bytes", os.path.getsize(path))

        # 真实库指纹：本脚本只读它，跑完必须完全一致。
        with open(DB, "rb") as f:
            import hashlib

            real_sha_before = hashlib.sha256(f.read()).hexdigest()[:16]
        out("real_db.sha_before", real_sha_before)

        tmp = tempfile.mkdtemp(prefix="crossmail-jmap-")
        try:
            endpoint = ENDPOINTS[0]
            probe_db, acct = prepare_probe_db(tmp, endpoint)
            out("probe.email", acct["email"])
            out("probe.config", json_dumps(acct["config"]))
            out("probe.reused_local_ciphertext", True)

            shutil.copy2(BINDING, os.path.join(tmp, "crossmail_ffi.py"))
            shutil.copy2(DLL, os.path.join(tmp, "uniffi_crossmail_ffi.dll"))
            sys.path.insert(0, tmp)
            import crossmail_ffi as cm

            with open(KEY, "r", encoding="utf-8") as f:
                master_key = f.read().strip()

            cfg = cm.EngineConfig(
                db_path=probe_db,
                cache_dir=os.path.join(tmp, "cache"),
                cache_quota_bytes=64 * 1024 * 1024,
                master_key_b64=master_key,
            )
            engine = cm.open_engine(cfg)
            out("engine.opened", type(engine).__name__)

            ids = [(a.id, a.protocol) for a in engine.list_accounts()]
            out("engine.accounts", json_dumps([PROBE_ID in [i for i, _ in ids]]))

            sync_ok = False
            try:
                report = engine.sync_account(PROBE_ID)
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
            except Exception as sync_exc:  # noqa: BLE001 - 真实核心错误原样保留
                out("sync.status", "FAILED")
                out("sync.error", f"{type(sync_exc).__name__}: {sync_exc}")

            boxes = engine.list_mailboxes(PROBE_ID)
            out("engine.mailboxes", len(boxes))
            out("engine.mailbox_names", json_dumps(sorted(b.name for b in boxes)))
            out("engine.mailbox_roles", json_dumps(sorted({str(b.role) for b in boxes})))
            unread = engine.unread_counts(PROBE_ID)
            out("engine.unread_counts", json_dumps(sorted((u.mailbox_id, u.unread) for u in unread)))

            page = engine.list_emails(
                PROBE_ID,
                cm.EmailQueryDto(mailbox_id=None, unread_only=False, limit=50, offset=0),
            )
            out("engine.emails.total", page.total)
            subjects = sorted(e.subject for e in page.items)
            out("engine.emails.subjects", json_dumps(subjects))

            body_ok = False
            if page.items:
                body = engine.read_body(page.items[0].id, False)
                out("engine.read_body.chars", len(body or ""))
                out("engine.read_body.head", json_dumps((body or "")[:60]))
                rendered = engine.render_body(page.items[0].id, False, False)
                out("engine.render_body.text_chars", len(rendered.text or ""))
                body_ok = bool(body)
            out("engine.read_body.ok", body_ok)

            # JMAP 增量同步：第二次同步应无新增（游标生效），验证 state 复用。
            report2 = engine.sync_account(PROBE_ID)
            out(
                "sync.second",
                json_dumps({"inserted": report2.inserted, "unchanged": report2.unchanged}),
            )

            with open(DB, "rb") as f:
                import hashlib

                real_sha_after = hashlib.sha256(f.read()).hexdigest()[:16]
            out("real_db.sha_after", real_sha_after)
            out("real_db.untouched", real_sha_before == real_sha_after)

            ok = sync_ok and page.total >= 1 and body_ok and real_sha_before == real_sha_after
            out("JMAP_VERIFY_OK", ok)
            write_result("OK" if ok else "FAILED")
            return 0 if ok else 2
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
    except Exception as exc:  # noqa: BLE001
        out("fatal", f"{type(exc).__name__}: {exc}")
        out("traceback", json_dumps(traceback.format_exc().splitlines()[-6:]))
        write_result("ERROR")
        return 1


if __name__ == "__main__":
    sys.exit(main())
