#!/usr/bin/env python3
"""UI smoke 数据根隔离的对照实验（真写操作，不碰真实账号库）。

背景：--window-smoke 的 ComposeWindow.SaveToCoreAsync() 在有账号时会真调
save_draft。历轮验证链因此往用户真实库里写过 SHELL_SMOKE_SUBJECT 草稿。
本探针用一个完全独立的目录（CROSSMAIL_DATA_ROOT 覆盖），在其中放一个纯本地
假账号（127.0.0.1:993、假口令、不联网），再真跑 --window-smoke，验证：
  1) 草稿确实被真写出来（证明写路径被触发，而不是因为「无账号」被跳过）；
  2) 草稿只落在探针库；
  3) 真实账号库字节级 SHA-256 与业务计数前后完全一致。

红线：不读、不回显、不复制任何真实凭据；假账号 secret 为固定占位串，
不跨机、不上云。结果写 stdout 与 win/isolation-probe-result.txt。
"""
from __future__ import annotations

import hashlib
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
WIN = os.path.dirname(HERE)
ROOT = os.path.dirname(WIN)
PROBE = os.path.join(WIN, "dist", "_isolation-probe")
EXE = os.path.join(
    WIN, "app", "CrossMail.App", "bin", "x64", "Release",
    "net8.0-windows10.0.19041.0", "CrossMail.App.exe",
)
DLL = os.path.join(ROOT, "crossmail", "target", "x86_64-pc-windows-msvc", "release", "crossmail_ffi.dll")
BINDING = os.path.join(ROOT, "crossmail", "build", "bindings", "python", "crossmail_ffi.py")
REAL_DIR = os.path.join(os.environ["LOCALAPPDATA"], "CrossMail")
REAL_DB = os.path.join(REAL_DIR, "crossmail.db")
RESULT = os.path.join(WIN, "isolation-probe-result.txt")


def sha(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        h.update(f.read())
    return h.hexdigest()[:16]


def counts(path: str) -> dict:
    con = sqlite3.connect("file:" + path.replace("\\", "/") + "?mode=ro", uri=True)
    try:
        names = [r[0] for r in con.execute("select name from sqlite_master where type='table'")]
        out = {}
        for t in ("accounts", "secrets", "emails", "mailboxes", "sync_state"):
            out[t] = con.execute("select count(*) from " + t).fetchone()[0] if t in names else -1
        out["smoke_drafts"] = (
            con.execute("select count(*) from emails where subject like 'SHELL_SMOKE_SUBJECT%'").fetchone()[0]
            if "emails" in names else -1
        )
        return out
    finally:
        con.close()


def run_window_smoke() -> str:
    env = dict(os.environ)
    env["CROSSMAIL_DATA_ROOT"] = PROBE
    r = subprocess.run(
        [EXE, "--window-smoke"], env=env, capture_output=True, text=True,
        timeout=240, encoding="utf-8", errors="replace",
    )
    return (r.stdout or "") + (r.stderr or "")


def enum_member(enum_type, *candidates):
    for name in candidates:
        value = getattr(enum_type, name, None)
        if value is not None:
            return value
    raise RuntimeError("enum member not found: " + "/".join(candidates))


def seed_fake_account() -> str:
    """用同一份 x64 DLL 在探针库里建一个纯本地假账号。"""
    key_path = os.path.join(PROBE, "master.key")
    with open(key_path, "r", encoding="utf-8") as f:
        master_key = f.read().strip()

    tmp = tempfile.mkdtemp(prefix="crossmail-isolation-")
    try:
        shutil.copy2(BINDING, os.path.join(tmp, "crossmail_ffi.py"))
        shutil.copy2(DLL, os.path.join(tmp, "uniffi_crossmail_ffi.dll"))
        sys.path.insert(0, tmp)
        import crossmail_ffi as cm

        cfg = cm.EngineConfig(
            db_path=os.path.join(PROBE, "crossmail.db"),
            cache_dir=os.path.join(PROBE, "cache"),
            cache_quota_bytes=16 * 1024 * 1024,
            master_key_b64=master_key,
        )
        engine = cm.open_engine(cfg)
        acc = engine.create_account(cm.NewAccountDto(
            email_address="isolation-probe@local.invalid",
            display_name=None,
            protocol=enum_member(cm.ProtocolKindDto, "IMAP", "Imap"),
            secret="isolation-probe-only-placeholder",
            jmap_endpoint=None,
            imap_host="127.0.0.1",
            imap_port=993,
            imap_security=enum_member(cm.SecurityModeDto, "IMPLICIT_TLS", "ImplicitTls"),
            smtp_host=None,
            smtp_port=None,
            smtp_security=None,
            sync_days=None,
            poll_interval_secs=None,
        ))
        return acc.id
    finally:
        sys.path.remove(tmp)
        shutil.rmtree(tmp, ignore_errors=True)


def main() -> int:
    lines: list[str] = []

    def out(key, value):
        text = f"{key}={value}"
        lines.append(text)
        print(text, flush=True)

    try:
        for label, path in (("exe", EXE), ("dll", DLL), ("binding", BINDING), ("real_db", REAL_DB)):
            if not os.path.isfile(path):
                raise FileNotFoundError(f"{label} 不存在: {path}")

        os.makedirs(PROBE, exist_ok=True)
        out("probe_dir", PROBE)
        real_sha_before = sha(REAL_DB)
        real_before = counts(REAL_DB)
        out("real.before", real_before)
        out("real.sha_before", real_sha_before)

        # 第一轮：空账号，只为让 C# 生成探针库与 master.key，并验证隔离开关生效。
        first = run_window_smoke()
        out("run1.isolated", "DATA_ROOT_ISOLATED=True" in first)
        out("run1.probe_root", "DATA_ROOT_NAME=_isolation-probe" in first)
        if "DATA_ROOT_ISOLATED=True" not in first or "DATA_ROOT_NAME=_isolation-probe" not in first:
            out("run1.tail", first[-600:].replace("\n", " | "))
            raise RuntimeError("第一轮 --window-smoke 没有使用探针数据根")
        db_path = os.path.join(PROBE, "crossmail.db")
        if not os.path.isfile(db_path) or not os.path.isfile(os.path.join(PROBE, "master.key")):
            raise RuntimeError("探针目录未生成 crossmail.db / master.key")
        out("probe.before", counts(db_path))

        # 放入假账号，让 SaveToCoreAsync 真走到 save_draft，而不是走「尚无账号」短路。
        account_id = seed_fake_account()
        out("probe.fake_account_id", account_id)
        out("probe.after_seed", counts(db_path))

        # 第二轮：真写草稿。
        second = run_window_smoke()
        out("run2.window_smoke_ok", "WINDOW_SMOKE_OK" in second)
        out("run2.compose_ok", "COMPOSE_WINDOW_OK compose_ok=True" in second)
        out("run2.save_draft_invoked", "save_draft ->" in second or "save_path_guarded=True" in second)
        out("run2.isolated", "DATA_ROOT_NAME=_isolation-probe" in second)

        probe_after = counts(db_path)
        real_sha_after = sha(REAL_DB)
        real_after = counts(REAL_DB)
        out("probe.after", probe_after)
        out("real.after", real_after)
        out("real.sha_after", real_sha_after)
        out("real.sha_unchanged", real_sha_before == real_sha_after)
        out("real.counts_unchanged", real_before == real_after)

        draft_written = probe_after["smoke_drafts"] >= 1
        real_clean = (real_before == real_after and real_sha_before == real_sha_after)
        out("DRAFT_WRITTEN_TO_PROBE", draft_written)
        out("REAL_DB_UNTOUCHED", real_clean)
        ok = draft_written and real_clean and "WINDOW_SMOKE_OK" in second
        out("ISOLATION_PROBE_OK", ok)
        out("RESULT", "OK" if ok else "FAIL")
    except Exception as ex:
        out("ERROR", type(ex).__name__ + ": " + str(ex))
        out("RESULT", "FAIL")
        ok = False
    finally:
        with open(RESULT, "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join(lines) + "\n")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
