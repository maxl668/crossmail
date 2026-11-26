import type { ReactNode } from "react";
import { useEffect, useState, useCallback, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  addrName,
  formatMailTime,
  hasFlag,
  type Account,
  type Mailbox,
  type Email,
  type ReadEmail,
  type SyncSummary,
  type NewAccountReq,
  type EndpointReq,
  type MailRule,
  type Attachment,
  type AttFile,
} from "./api";

const ROLE_ICON: Record<string, string> = {
  Inbox: "📥", Sent: "📤", Trash: "🗑️", Archive: "📦",
  Drafts: "📝", Junk: "🚫", Other: "📁",
};

/// 常见邮箱文件夹中文名映射（v1.2，仅 UI 显示层，服务器原名不变）。
/// 匹配键小写化后按片段匹配，兼容 "INBOX"、"Sent Messages"、"Junk E-mail" 等变体。
const MAILBOX_CN: [RegExp, string][] = [
  [/inbox/i, "收件箱"],
  [/^(sent|已发送)/i, "已发送"],
  [/^(draft|草稿)/i, "草稿箱"],
  [/^(trash|deleted|bin|废纸)/i, "已删除"],
  [/^(junk|spam|垃圾)/i, "垃圾邮件"],
  [/^(archive|归档)/i, "归档"],
  [/^(outbox|发件)/i, "发件箱"],
];

/// 显示名：命中映射用中文，否则保留服务器原名。
const mailboxLabel = (name: string): string => {
  for (const [re, cn] of MAILBOX_CN) {
    if (re.test(name)) return cn;
  }
  return name;
};

/// 撰写窗预填内容（回复/转发/编辑草稿共用）
interface ComposePrefill {
  to?: string; cc?: string; bcc?: string; subject?: string; body?: string;
  html?: string; // 富文本草稿的 HTML 正文（v0.7）
  inReplyTo?: string; references?: string[]; draftId?: string;
  initialAtts?: AttFile[];
}

const addrStr = (a: { name: string | null; email: string }): string =>
  a.name ? `${a.name} <${a.email}>` : a.email;

export default function App() {
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [accountId, setAccountId] = useState<string | null>(null);
  const [mailboxes, setMailboxes] = useState<Mailbox[]>([]);
  const [mailboxId, setMailboxId] = useState<string | null>(null);
  const [emails, setEmails] = useState<Email[]>([]);
  const [total, setTotal] = useState(0);
  const [keyword, setKeyword] = useState("");
  const [searchMode, setSearchMode] = useState(false);
  const [serverMode, setServerMode] = useState(false);
  const [moveTarget, setMoveTarget] = useState<Email | null>(null);
  const [draftsMode, setDraftsMode] = useState(false);
  const [composePrefill, setComposePrefill] = useState<ComposePrefill | null>(null);
  const [autoStart, setAutoStart] = useState(false);

  // 初始化：读取开机自启状态（v0.4）
  useEffect(() => { api.autostartStatus().then(setAutoStart).catch(() => {}); }, []);
  const toggleAutoStart = async () => {
    try {
      const on = await api.autostartSet(!autoStart);
      setAutoStart(on);
      notify(on ? "已开启开机自启" : "已关闭开机自启");
    } catch (e) { notify(`设置失败：${e}`); }
  };
  const [current, setCurrent] = useState<ReadEmail | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  // v1.7：新邮件醒目提醒卡片（浏览状态），与通用 toast 分离
  const [mailAlert, setMailAlert] = useState<{ inserted: number; unread: number } | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const [showDataDir, setShowDataDir] = useState(false);
  const [showFolders, setShowFolders] = useState(false);
  const [showRules, setShowRules] = useState(false);
  const [settingsAccount, setSettingsAccount] = useState<Account | null>(null);
  const [, setPushOn] = useState(false);
  // v1.4：自动收取默认开启（每 60 秒收取全部账号），可菜单里关闭
  const [autoFetch, setAutoFetch] = useState<boolean>(
    () => localStorage.getItem("cm-auto-fetch") !== "off");
  // v1.6：Ctrl+滚轮缩放邮件正文（0.5–2.5x，步进 10%）
  const [zoom, setZoom] = useState(1);
  const readerRef = useRef<HTMLElement>(null);
  useEffect(() => {
    const el = readerRef.current;
    if (!el) return;
    // passive:false 才能 preventDefault——拦掉 WebView2 的整页缩放，只缩正文
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey) return;
      e.preventDefault();
      setZoom((z) => Math.min(2.5, Math.max(0.5, +(z + (e.deltaY < 0 ? 0.1 : -0.1)).toFixed(2))));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [current]); // current 切换时 reader 才挂载，重新绑定
  // 批量操作（v1.0）：本地列表多选
  const [multiMode, setMultiMode] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [batchMoveOpen, setBatchMoveOpen] = useState(false);

  const notify = (m: string) => { setToast(m); setTimeout(() => setToast(null), 4000); };

  const refreshAccounts = useCallback(async () => {
    const list = await api.listAccounts();
    setAccounts(list);
    setAccountId((cur) => cur && list.some((a) => a.id === cur) ? cur : (list[0]?.id ?? null));
  }, []);

  const refreshMailboxes = useCallback(async (aid: string) => {
    setMailboxes(await api.listMailboxes(aid));
  }, []);

  const refreshEmails = useCallback(async (aid: string, mid: string | null) => {
    const page = await api.listEmails(aid, mid ?? undefined, false, 0);
    setEmails(page.items);
    setTotal(page.total);
  }, []);

  // 初始化
  useEffect(() => { refreshAccounts(); }, [refreshAccounts]);
  // 多账号推送并发（v0.5）：全部账号各自订阅（命令层对同账号重复订阅会先取消旧的）
  useEffect(() => {
    accounts.forEach((a) => { api.pushSubscribe(a.id).catch(() => {}); });
  }, [accounts]);
  useEffect(() => {
    if (!accountId) { setMailboxes([]); setMailboxId(null); setEmails([]); return; }
    // MenuBar 常用文件夹跨账号跳转：切账号时应用待选文件夹
    const pending = sessionStorage.getItem("cm-pending-mailbox");
    sessionStorage.removeItem("cm-pending-mailbox");
    refreshMailboxes(accountId);
    setMailboxId(pending);
    setEmails([]);
    setKeyword("");
    setSearchMode(false);
    // 默认订阅该账号推送
    api.pushSubscribe(accountId).then(() => setPushOn(true)).catch(() => setPushOn(false));
  }, [accountId, refreshMailboxes]);
  useEffect(() => {
    if (accountId && mailboxId) refreshEmails(accountId, mailboxId);
  }, [accountId, mailboxId, refreshEmails]);

  // 推送事件：刷新角标与列表
  useEffect(() => {
    const un = listen<any>("push-notice", (ev) => {
      const p = ev.payload;
      if (p.type === "Synced" && accountId) {
        refreshMailboxes(accountId);
        if (mailboxId && !searchMode) refreshEmails(accountId, mailboxId);
        if (p.inserted > 0) {
          // v1.7：新邮件醒目卡片（自动 8s 消失，点击直接进收件箱）
          setMailAlert({ inserted: p.inserted, unread: p.unread_total });
          window.setTimeout(() => setMailAlert(null), 8000);
        } else {
          notify(`同步完成，未读 ${p.unread_total}`);
        }
      } else if (p.type === "Failed") {
        notify(`推送异常：${p.error}${p.retry_in_ms ? `（${Math.round(p.retry_in_ms / 1000)}s 后重试）` : ""}`);
      }
    });
    return () => { un.then((f) => f()); };
  }, [accountId, mailboxId, searchMode, refreshMailboxes, refreshEmails]);

  // IMAP 服务器搜索逐文件夹进度（v0.9）：busy 文案实时更新
  useEffect(() => {
    const un = listen<{ folder: string; found: number }>("server-search-progress", (ev) => {
      setBusy(`服务器搜索中：${ev.payload.folder}（已找到 ${ev.payload.found} 封）`);
    });
    return () => { un.then((f) => f()); };
  }, []);

  // 搜索（v0.2/v0.5）：回车本地搜索，「服务器」按钮 JMAP filter 直查
  const doSearch = async () => {
    if (!accountId || !keyword.trim()) return;
    setBusy("搜索中…");
    try {
      const page = await api.searchEmails(accountId, keyword.trim(), 0);
      setEmails(page.items);
      setTotal(page.total);
      setSearchMode(true);
      setServerMode(false);
    } catch (e) { notify(`搜索失败：${e}`); }
    setBusy(null);
  };
  const doServerSearch = async () => {
    if (!accountId || !keyword.trim()) return;
    setBusy("服务器搜索中…");
    try {
      const list = await api.serverSearch(accountId, keyword.trim());
      setEmails(list);
      setTotal(list.length);
      setSearchMode(true);
      setServerMode(true);
      notify(`服务器搜索完成：${list.length} 封`);
    } catch (e) { notify(`服务器搜索失败：${e}`); }
    setBusy(null);
  };
  const clearSearch = () => {
    setKeyword("");
    setSearchMode(false);
    setServerMode(false);
    if (accountId && mailboxId) refreshEmails(accountId, mailboxId);
  };

  // 服务器搜索结果阅读（v0.5）：fetch_email 直取，不落本地库；v0.6 加已读直写
  const openServerEmail = async (e: Email) => {
    if (!accountId) return;
    setCurrent(null);
    try {
      const r = await api.serverReadEmail(accountId, e.id);
      setCurrent(r);
      // 自动标记已读（服务器直写），并同步列表项状态
      if (!hasFlag(e, "Seen")) {
        try {
          await api.serverSetFlag(accountId, e.id, "seen", true);
          const updated = { ...e, flags: [...e.flags, "seen"] };
          setEmails((list) => list.map((x) => (x.id === e.id ? updated : x)));
        } catch { /* 标记失败不影响阅读 */ }
      }
      // eml 归档（v1.1）：服务器搜索结果同样归档，保证离线可查
      api.archiveEml(accountId, e.id).catch(() => {});
    } catch (err) { notify(`读取失败：${err}`); }
  };

  // 服务器搜索结果星标（v0.6）：JMAP id 直写
  const toggleStarServer = async (e: Email) => {
    if (!accountId) return;
    const starred = !hasFlag(e, "Flagged");
    try {
      await api.serverSetFlag(accountId, e.id, "flagged", starred);
      const updated = {
        ...e,
        flags: starred
          ? [...e.flags.filter((f) => typeof f === "string" && f.toLowerCase() !== "flagged"), "flagged"]
          : e.flags.filter((f) => !(typeof f === "string" && f.toLowerCase() === "flagged")),
      };
      setEmails((list) => list.map((x) => (x.id === e.id ? updated : x)));
      if (current?.email.id === e.id) setCurrent({ ...current, email: updated });
    } catch (err) { notify(`标记失败：${err}`); }
  };

  // 服务器搜索结果删除（v0.8）：JMAP Email/destroy / IMAP 移回收站，直删服务器
  const deleteServerEmail = async (e: Email) => {
    if (!accountId) return;
    if (!confirm(`确定在服务器上删除「${e.subject || "(无主题)"}」？`)) return;
    try {
      await api.serverDeleteEmail(accountId, e.id);
      setEmails((list) => list.filter((x) => x.id !== e.id));
      if (current?.email.id === e.id) setCurrent(null);
      notify("已在服务器删除");
    } catch (err) { notify(`删除失败：${err}`); }
  };

  // 服务器搜索结果移动（v0.9）：JMAP patch mailboxIds / IMAP UID MOVE
  const onMoveServer = async (e: Email, toId: string, toName: string) => {
    if (!accountId) return;
    // JMAP 需要 from 做源 mailbox patch；IMAP 传空则 provider 回落到 id 内嵌文件夹
    const from = e.mailbox_ids?.[0] ?? "";
    setBusy("正在移动…");
    try {
      await api.serverMoveEmail(accountId, e.id, from, toId);
      setEmails((list) => list.filter((x) => x.id !== e.id));
      if (current?.email.id === e.id) setCurrent(null);
      setMoveTarget(null);
      notify(`已移动到「${toName}」`);
    } catch (err) { notify(`移动失败：${err}`); }
    setBusy(null);
  };

  // 附件下载（v0.2；v0.8 IMAP 走 emailId#partId 兜底）
  const onDownload = async (a: Attachment) => {
    if (!accountId || !current) return;
    try {
      const r = await api.saveAttachment(accountId, current.email.id, a);
      if (r) notify(`已保存：${r}`);
    } catch (e) { notify(`附件下载失败：${e}`); }
  };

  // ---------- 批量操作（v1.0）：本地列表多选 ----------
  const labelDone = (ok: number, all: number): string =>
    ok === all ? `已处理 ${ok} 封` : `完成 ${ok}/${all} 封（其余失败，可重试）`;

  const toggleMulti = () => {
    setMultiMode((m) => !m);
    setSelected(new Set());
  };
  const toggleSelect = (id: string) => {
    setSelected((cur) => {
      const next = new Set(cur);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
  };
  const selectAllVisible = () => {
    setSelected((cur) => (cur.size === emails.length ? new Set() : new Set(emails.map((e) => e.id))));
  };
  /// 逐封执行（每封独立容错），返回成功数
  const runBatch = async (
    ids: string[], label: string,
    op: (id: string) => Promise<unknown>,
  ): Promise<number> => {
    let ok = 0;
    for (let i = 0; i < ids.length; i++) {
      setBusy(`${label} ${i + 1}/${ids.length}…`);
      try { await op(ids[i]); ok++; } catch { /* 单封失败不中断 */ }
    }
    setBusy(null);
    return ok;
  };

  const batchSetRead = async (read: boolean) => {
    if (!selected.size || !accountId) return;
    const ids = [...selected];
    const ok = await runBatch(ids, read ? "标记已读" : "标记未读", (id) => api.setRead(id, read));
    notify(labelDone(ok, ids.length));
    setSelected(new Set());
    refreshEmails(accountId, mailboxId);
    refreshMailboxes(accountId);
  };

  const batchStar = async () => {
    if (!selected.size || !accountId) return;
    const ids = [...selected];
    const ok = await runBatch(ids, "加星标", (id) => api.toggleStar(id, true));
    notify(labelDone(ok, ids.length));
    setSelected(new Set());
    refreshEmails(accountId, mailboxId);
  };

  const batchDelete = async () => {
    if (!accountId || !selected.size) return;
    if (!confirm(`确定删除选中的 ${selected.size} 封邮件？（删除会同步到服务器）`)) return;
    const ids = [...selected];
    const ok = await runBatch(ids, "删除", (id) => api.serverDeleteEmail(accountId, id));
    notify(labelDone(ok, ids.length));
    setSelected(new Set());
    if (current && ids.includes(current.email.id)) setCurrent(null);
    refreshEmails(accountId, mailboxId);
    refreshMailboxes(accountId);
  };

  const batchMove = async (toMailbox: string, toName: string) => {
    if (!accountId || !selected.size) return;
    setBatchMoveOpen(false);
    const ids = [...selected];
    const ok = await runBatch(ids, `移动到 ${toName}`,
      (id) => api.serverMoveEmail(accountId, id, undefined, toMailbox));
    notify(`${labelDone(ok, ids.length)} → ${toName}`);
    setSelected(new Set());
    refreshEmails(accountId, mailboxId);
    refreshMailboxes(accountId);
  };

  // 草稿箱（v0.3）
  const loadDrafts = useCallback(async (aid: string) => {
    const ds = await api.listDrafts(aid);
    setEmails(ds);
    setTotal(ds.length);
    setDraftsMode(true);
    setSearchMode(false);
  }, []);

  // 回复 / 回复全部 / 转发（v0.3）
  const replyPrefill = (r: ReadEmail, all: boolean): ComposePrefill => {
    const em = r.email;
    const mid = em.message_id ?? undefined;
    const refs = mid ? [...(em.references ?? []), mid] : [...(em.references ?? [])];
    const replyTos = em.reply_to ?? [];
    const tos = all ? [...replyTos, ...em.from] : (replyTos.length ? replyTos : em.from);
    const sig = getSignature(accountId);
    return {
      to: tos.map(addrStr).join(", "),
      cc: all ? em.cc.map(addrStr).join(", ") : undefined,
      subject: /^re:/i.test(em.subject) ? em.subject : `Re: ${em.subject}`,
      body: `${sig ? `\n--\n${sig}\n` : ""}\n----- 原始邮件 -----\n发件人：${em.from.map(addrName).join(", ")}\n时间：${formatMailTime(em.sent_at ?? em.received_at, true)}\n\n${(r.text_body ?? em.preview).split("\n").map((l) => `> ${l}`).join("\n")}`,
      inReplyTo: mid,
      references: refs.length ? refs : undefined,
    };
  };
  const forwardPrefill = (r: ReadEmail): ComposePrefill => {
    const em = r.email;
    const sig = getSignature(accountId);
    return {
      subject: /^fwd:/i.test(em.subject) ? em.subject : `Fwd: ${em.subject}`,
      body: `${sig ? `\n--\n${sig}\n` : ""}\n----- 转发的邮件 -----\n发件人：${em.from.map(addrName).join(", ")}\n收件人：${em.to.map(addrName).join(", ")}\n主题：${em.subject}\n时间：${formatMailTime(em.sent_at ?? em.received_at, true)}\n\n${r.text_body ?? em.preview}`,
    };
  };
  const editDraftPrefill = async (r: ReadEmail): Promise<ComposePrefill> => {
    const em = r.email;
    // 草稿附件在本地缓存中（part_id = att{i}），读出后带入编辑窗（v0.5）
    let initialAtts: AttFile[] | undefined;
    const parts = (em.attachments ?? []).map((a) => a.part_id).filter((p): p is string => !!p);
    if (parts.length) {
      try {
        initialAtts = (await api.readStoredAttachments(em.id, parts)).map((f, i) => ({
          ...f,
          name: em.attachments?.find((a) => a.part_id === parts[i])?.name ?? f.name,
          content_type: em.attachments?.find((a) => a.part_id === parts[i])?.content_type ?? f.content_type,
        }));
      } catch { /* 缓存缺失则不带附件，不阻断编辑 */ }
    }
    return {
      to: em.to.map(addrStr).join(", "),
      cc: em.cc.map(addrStr).join(", "),
      subject: em.subject,
      body: r.text_body ?? "",
      html: r.html_body ?? undefined,
      inReplyTo: em.in_reply_to ?? undefined,
      references: (em.references ?? []).length ? em.references : undefined,
      draftId: em.id,
      initialAtts,
    };
  };

  // v1.5：收取菜单去重——删掉「收取当前账号」（与「收取全部账号」重复），统一走 syncAll

  // v1.5：邮件签名（任务4）——按账号存 localStorage，写信/回复默认带入
  const getSignature = (aid: string | null): string =>
    aid ? (localStorage.getItem(`cm-signature:${aid}`) ?? "") : "";

  const composeNewPrefill = (): ComposePrefill => {
    const sig = getSignature(accountId);
    return { to: "", cc: "", subject: "", body: sig ? `\n\n--\n${sig}` : "" };
  };

  const openEmail = async (e: Email) => {
    if (!accountId) return;
    setCurrent(null);
    try {
      const r = await api.readEmail(accountId, e.id);
      setCurrent(r);
      setEmails((list) => list.map((x) => (x.id === e.id ? r.email : x)));
      refreshMailboxes(accountId);
      // eml 归档（v1.1）：幂等、静默失败，失败不影响阅读
      api.archiveEml(accountId, e.id).catch(() => {});
    } catch (err) { notify(`读取失败：${err}`); }
  };

  const toggleStar = async (e: Email) => {
    const starred = !hasFlag(e, "Flagged");
    const updated = await api.toggleStar(e.id, starred);
    setEmails((list) => list.map((x) => (x.id === updated.id ? updated : x)));
    if (current?.email.id === updated.id) setCurrent({ ...current, email: updated });
  };

  const onAddAccount = async (req: NewAccountReq) => {
    setBusy("正在创建账号…");
    try {
      await api.createAccount(req);
      await refreshAccounts();
      setShowAdd(false);
      notify("账号已添加");
    } catch (e) { notify(`创建失败：${e}`); }
    setBusy(null);
  };

  const onDeleteAccount = async () => {
    if (!accountId) return;
    if (!confirm("确定删除该账号？（本地数据一并移除）")) return;
    await api.deleteAccount(accountId);
    setPushOn(false);
    await refreshAccounts();
  };

  const onSend = async (v: {
    to: string[]; cc: string[]; bcc: string[]; subject: string; body: string;
    html?: string | null;
    attachments?: AttFile[];
    inReplyTo?: string; references?: string[]; draftId?: string;
  }) => {
    if (!accountId) return;
    setBusy(v.draftId ? "正在发送草稿…" : "正在发送…");
    try {
      if (v.draftId) {
        await api.sendDraft(accountId, v.draftId);
      } else {
        await api.sendEmail(
          accountId, v.subject, v.to,
          v.cc.length ? v.cc : undefined, v.body,
          v.bcc.length ? v.bcc : undefined,
          v.inReplyTo, v.references,
          v.attachments?.length ? v.attachments : undefined,
          v.html ?? null,
        );
      }
      setComposePrefill(null);
      notify("发送成功");
      await refreshMailboxes(accountId);
      if (draftsMode) await loadDrafts(accountId);
    } catch (e) { notify(`发送失败：${e}`); }
    setBusy(null);
  };

  const onSaveDraft = async (v: {
    to: string[]; cc: string[]; bcc: string[]; subject: string; body: string;
    html?: string | null;
    attachments?: AttFile[];
    inReplyTo?: string; references?: string[]; draftId?: string;
  }) => {
    if (!accountId) return;
    setBusy("正在保存草稿…");
    try {
      const saved = await api.saveDraft(accountId, {
        to: v.to, cc: v.cc.length ? v.cc : undefined,
        bcc: v.bcc.length ? v.bcc : undefined,
        subject: v.subject, body: v.body,
        html_body: v.html ?? null,
        attachments: v.attachments?.length ? v.attachments : undefined,
        in_reply_to: v.inReplyTo ?? null, references: v.references,
      });
      if (v.draftId) await api.deleteDraft(v.draftId); // 编辑旧草稿：保存新版后删旧版
      setComposePrefill(null);
      notify("草稿已保存");
      if (draftsMode) await loadDrafts(accountId);
      void saved;
    } catch (e) { notify(`保存草稿失败：${e}`); }
    setBusy(null);
  };

  const account = accounts.find((a) => a.id === accountId);

  // v1.3：文件夹排序（FolderManagerDialog 持久化在 localStorage）
  const orderedMailboxes = getOrderedMailboxes(accountId, mailboxes);

  // v1.3：收取全部账号（silent = 自动收取时静默，不弹提示）
  const syncAll = async (silent = false) => {
    if (!accounts.length) return;
    setBusy(silent ? "自动收取中…" : "正在收取全部账号…");
    let failed = 0;
    for (const a of accounts) {
      try { await api.syncAccount(a.id); }
      catch (e) { failed++; if (!silent) notify(`${a.email_address} 同步失败：${e}`); }
    }
    if (accountId) {
      refreshMailboxes(accountId);
      if (mailboxId && !searchMode && !draftsMode) refreshEmails(accountId, mailboxId);
    }
    setBusy(null);
    if (!silent) notify(failed ? `收取完成（${failed} 个账号失败）` : "全部账号收取完成");
  };

  // v1.4：定时自动收取（默认开启，60 秒一轮；与推送互补，覆盖不支持 IDLE 的服务器）
  const toggleAutoFetch = () => {
    setAutoFetch((on) => {
      localStorage.setItem("cm-auto-fetch", on ? "off" : "on");
      notify(on ? "已关闭自动收取" : "已开启自动收取（每 60 秒）");
      return !on;
    });
  };
  useEffect(() => {
    if (!autoFetch || !accounts.length) return;
    const t = setInterval(() => { if (!busy) syncAll(true); }, 60_000);
    return () => clearInterval(t);
  }, [autoFetch, accounts.length, busy, accountId, mailboxId, searchMode, draftsMode]);

  return (
    <>
      <MenuBar
        accounts={accounts}
        mailboxes={orderedMailboxes}
        currentAccountId={accountId}
        onCompose={() => accountId && setComposePrefill(composeNewPrefill())}
        onSyncAll={syncAll}
        onSelectAccount={(id) => setAccountId(id)}
        onOpenAccountSettings={() => {
          if (!account) { notify("请先添加账号"); return; }
          setSettingsAccount(account);
        }}
        onOpenDataDir={() => setShowDataDir(true)}
        onOpenRules={() => accountId ? setShowRules(true) : notify("请先添加账号")}
        onOpenFolders={() => accountId ? setShowFolders(true) : notify("请先添加账号")}
        onAddAccount={() => setShowAdd(true)}
        onDeleteAccount={onDeleteAccount}
        hasAccount={!!account}
        autoStart={autoStart}
        onToggleAutoStart={toggleAutoStart}
        autoFetch={autoFetch}
        onToggleAutoFetch={toggleAutoFetch}
      />
    <div className="app">
      {/* 侧栏 */}
      <aside className="sidebar">
        <div className="brand">✉️ CrossMail</div>
        <div className="accounts">
          {accounts.map((a) => (
            <div key={a.id}
              className={`account ${a.id === accountId ? "active" : ""}`}
              onClick={() => setAccountId(a.id)}>
              <span className="proto">{a.protocol === "Jmap" ? "J" : "I"}</span>
              <span className="addr">{a.email_address}</span>
              <button className="acct-settings" title="账号设置"
                onClick={(ev) => { ev.stopPropagation(); setSettingsAccount(a); }}>⚙</button>
            </div>
          ))}
          {/* v1.4：添加账号 / 数据目录 / 文件夹 / 规则 / 删除账号 已合入顶部菜单栏
              （账号 / 工具 菜单），侧边栏仅保留账号列表。 */}
        </div>
        <div className="folders">
          {orderedMailboxes.map((m) => (
            <div key={m.id}
              className={`folder ${!draftsMode && m.id === mailboxId ? "active" : ""}`}
              onClick={() => { setMailboxId(m.id); setDraftsMode(false); setSearchMode(false); }}>
              <span>{ROLE_ICON[m.role] ?? "📁"} {mailboxLabel(m.name)}</span>
              {m.unread > 0 && <span className="badge">{m.unread}</span>}
            </div>
          ))}
          <div className={`folder ${draftsMode ? "active" : ""}`}
            onClick={() => { if (accountId) { setMailboxId(null); loadDrafts(accountId); } }}>
            <span>📝 草稿箱</span>
          </div>
        </div>
        {/* v1.5：侧栏操作区已清空——写邮件直点顶部菜单栏「写邮件」即写，
            收取/自启/签名等均在顶部菜单栏。 */}
      </aside>

      {/* 邮件列表 */}
      <section className="list">
        <div className="list-head">
          {draftsMode
            ? <span>📝 草稿箱</span>
            : searchMode
              ? <span>搜索「{keyword}」的结果{serverMode ? "（服务器端）" : "（本地）"}</span>
              : <span>{mailboxes.find((m) => m.id === mailboxId)?.name ? mailboxLabel(mailboxes.find((m) => m.id === mailboxId)!.name) : "全部邮件"}</span>}
          <span className="muted">{total} 封</span>
          {!draftsMode && !serverMode && (
            <button className={`btn tiny ${multiMode ? "on" : ""}`} onClick={toggleMulti}>
              {multiMode ? "退出多选" : "☑ 多选"}
            </button>
          )}
        </div>
        {multiMode && !draftsMode && !serverMode && (
          <div className="batch-bar">
            <button className="btn tiny" onClick={selectAllVisible}>
              {selected.size === emails.length && emails.length > 0 ? "取消全选" : "全选"}
            </button>
            <span className="muted">已选 {selected.size} 封</span>
            <button className="btn tiny" disabled={!selected.size} onClick={() => batchSetRead(true)}>✅ 标已读</button>
            <button className="btn tiny" disabled={!selected.size} onClick={() => batchSetRead(false)}>⚪ 标未读</button>
            <button className="btn tiny" disabled={!selected.size} onClick={batchStar}>⭐ 星标</button>
            <button className="btn tiny" disabled={!selected.size} onClick={() => setBatchMoveOpen(true)}>📁 移动到…</button>
            <button className="btn tiny danger" disabled={!selected.size} onClick={batchDelete}>🗑️ 删除</button>
          </div>
        )}
        {!draftsMode && (
          <div className="search-bar">
            <input value={keyword} placeholder="搜索主题 / 发件人 / 收件人，回车本地搜索"
              onChange={(e) => setKeyword(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter") doSearch(); }} />
            <button className="btn small" onClick={doServerSearch} title="JMAP 服务器端直查">🌐 服务器</button>
            {searchMode && <button className="btn small" onClick={clearSearch}>退出搜索</button>}
          </div>
        )}
        <div className="list-body">
          {emails.length === 0 && <div className="empty">暂无邮件，点「同步」拉取</div>}
          {emails.map((e) => {
            const unread = !hasFlag(e, "Seen");
            return (
              <div key={e.id} className={`mail ${e.id === current?.email.id ? "active" : ""} ${unread ? "unread" : ""}`}
                onClick={() => (serverMode ? openServerEmail(e) : openEmail(e))}>
                {multiMode && !draftsMode && !serverMode && (
                  <input type="checkbox" className="mail-check"
                    checked={selected.has(e.id)}
                    onClick={(ev) => ev.stopPropagation()}
                    onChange={() => toggleSelect(e.id)} />
                )}
                <div className="mail-line1">
                  <span className="mail-from">{addrName(e.from[0] ?? { name: null, email: "?" })}</span>
                  <span className="mail-time">{formatMailTime(e.sent_at ?? e.received_at)}</span>
                </div>
                <div className="mail-line2">
                  <span className="mail-subject">{hasFlag(e, "Flagged") ? "⭐ " : ""}{e.subject || "(无主题)"}</span>
                </div>
                <div className="mail-preview">{e.preview}</div>
                {!serverMode ? (
                  <button className="star" onClick={(ev) => { ev.stopPropagation(); toggleStar(e); }}>
                    {hasFlag(e, "Flagged") ? "★" : "☆"}
                  </button>
                ) : (
                  <button className="star" onClick={(ev) => { ev.stopPropagation(); toggleStarServer(e); }}>
                    {hasFlag(e, "Flagged") ? "★" : "☆"}
                  </button>
                )}
              </div>
            );
          })}
        </div>
      </section>

      {/* 阅读窗格 */}
      <section className="reader" ref={readerRef}>
        {!current && <div className="empty">选择一封邮件查看</div>}
        {current && (
          <>
            <div className="reader-head">
              <h2>{current.email.subject || "(无主题)"}</h2>
              <div className="meta">
                <div><b>发件人：</b>{current.email.from.map(addrName).join(", ")}</div>
                <div><b>收件人：</b>{current.email.to.map(addrName).join(", ")}</div>
                <div><b>时间：</b>{formatMailTime(current.email.sent_at ?? current.email.received_at, true)}</div>
                {current.fetched_remote && <div className="muted">（正文本次从服务器拉取）</div>}
              </div>
              {(current.email.attachments?.filter((a) => !a.inline).length ?? 0) > 0 && (
                <div className="attachments">
                  {current.email.attachments!.filter((a) => !a.inline).map((a) => (
                    <button key={a.id} className="att" onClick={() => onDownload(a)}>
                      📎 {a.name ?? a.id}（{a.size >= 1024 ? `${(a.size / 1024).toFixed(1)} KB` : `${a.size} B`}）
                    </button>
                  ))}
                </div>
              )}
            </div>
            <div className="reader-actions">
              {hasFlag(current.email, "Draft") ? (
                <button className="btn primary small"
                  onClick={() => editDraftPrefill(current).then((p) => setComposePrefill(p))}>
                  ✏️ 编辑草稿
                </button>
              ) : (
                <>
                  <button className="btn small" onClick={() => setComposePrefill(replyPrefill(current, false))}>↩️ 回复</button>
                  <button className="btn small" onClick={() => setComposePrefill(replyPrefill(current, true))}>↩️ 回复全部</button>
                  <button className="btn small" onClick={() => setComposePrefill(forwardPrefill(current))}>↪️ 转发</button>
                </>
              )}
              {serverMode && (
                <>
                  <button className="btn small" onClick={() => setMoveTarget(current.email)}>
                    📁 移动到…
                  </button>
                  <button className="btn small danger" onClick={() => deleteServerEmail(current.email)}>
                    🗑️ 服务器删除
                  </button>
                </>
              )}
              {!hasFlag(current.email, "Draft") && accountId && (
                <button className="btn small" onClick={async () => {
                  try {
                    const r = await api.exportEml(accountId, current.email.id, current.email.subject);
                    if (r) notify(`已导出：${r}`);
                  } catch (err) { notify(`导出失败：${err}`); }
                }}>📤 导出 eml</button>
              )}
            </div>
            <div className="reader-body">
              {/* v1.6：Ctrl+滚轮缩放邮件内容（0.5x–2.5x）；正文限宽居中 */}
              {zoom !== 1 && (
                <button className="btn tiny zoom-reset" title="恢复 100%"
                  onClick={() => setZoom(1)}>{Math.round(zoom * 100)}% ↺</button>
              )}
              <div className="reader-content" style={{ zoom }}>
                {current.html_body
                  ? <div className="html-body" dangerouslySetInnerHTML={{ __html: current.html_body }} />
                  : <pre className="text-body">{current.text_body ?? "(空正文)"}</pre>}
              </div>
            </div>
          </>
        )}
      </section>

      {showAdd && <AddAccountDialog onClose={() => setShowAdd(false)} onSubmit={onAddAccount} />}
      {showDataDir && <DataLocationDialog onClose={() => setShowDataDir(false)} />}
      {showFolders && accountId && (
        <FolderManagerDialog accountId={accountId} mailboxes={mailboxes}
          onClose={() => setShowFolders(false)}
          onChanged={() => refreshMailboxes(accountId)} />
      )}
      {showRules && accountId && (
        <RulesDialog accountId={accountId} mailboxes={mailboxes}
          onClose={() => setShowRules(false)}
          onChanged={() => {
            refreshMailboxes(accountId);
            if (mailboxId && !searchMode && !draftsMode) refreshEmails(accountId, mailboxId);
          }} />
      )}
      {settingsAccount && (
        <AccountSettingsDialog account={settingsAccount}
          onClose={() => setSettingsAccount(null)}
          onSaved={(updated) => {
            setAccounts((cur) => cur.map((a) => (a.id === updated.id ? updated : a)));
            setSettingsAccount(null);
            notify("账号设置已保存");
          }} />
      )}
      {batchMoveOpen && (
        <MoveDialog mailboxes={mailboxes} excludeId={mailboxId ?? undefined}
          onClose={() => setBatchMoveOpen(false)}
          onMove={(id, name) => batchMove(id, name)} />
      )}
      {moveTarget && (
        <MoveDialog mailboxes={mailboxes} excludeId={moveTarget.mailbox_ids?.[0]}
          onClose={() => setMoveTarget(null)}
          onMove={(id, name) => onMoveServer(moveTarget, id, name)} />
      )}
      {composePrefill !== null && (
        <ComposeDialog defaultFrom={account?.email_address ?? ""} prefill={composePrefill}
          onClose={() => setComposePrefill(null)} onSend={onSend} onSaveDraft={onSaveDraft} />
      )}
      {busy && <div className="busy">{busy}</div>}
      {toast && <div className="toast">{toast}</div>}
      {mailAlert && (
        <div className="mail-alert" onClick={() => {
          const inbox = mailboxes.find((m) => m.role === "Inbox");
          if (inbox) { setMailboxId(inbox.id); setDraftsMode(false); setSearchMode(false); }
          setMailAlert(null);
        }}>
          <div className="mail-alert-title">✉️ 收到新邮件</div>
          <div className="mail-alert-body">新增 {mailAlert.inserted} 封 · 未读共 {mailAlert.unread} 封</div>
          <div className="mail-alert-hint">点击查看收件箱</div>
        </div>
      )}
    </div>
    </>
  );
}

// ---------- 添加账号对话框 ----------

function AddAccountDialog({ onClose, onSubmit }: {
  onClose: () => void; onSubmit: (req: NewAccountReq) => void;
}) {
  const [kind, setKind] = useState<"jmap" | "imap">("jmap");
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [secret, setSecret] = useState("");
  const [jmapUrl, setJmapUrl] = useState("");
  const [imapHost, setImapHost] = useState("");
  const [imapPort, setImapPort] = useState(993);
  const [smtpHost, setSmtpHost] = useState("");
  const [smtpPort, setSmtpPort] = useState(465);
  const [days, setDays] = useState(30);
  const [preset, setPreset] = useState("custom");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);

  // 服务商预设（v1.3）：IMAP/SMTP 常见服务商一键填服务器；密码填「授权码」
  const PRESETS: Record<string, { imap: [string, number]; smtp: [string, number]; note: string }> = {
    qq: { imap: ["imap.qq.com", 993], smtp: ["smtp.qq.com", 465], note: "QQ 邮箱需在设置中开启 IMAP 并使用授权码登录" },
    "163": { imap: ["imap.163.com", 993], smtp: ["smtp.163.com", 465], note: "163 邮箱需使用客户端授权码登录" },
    "126": { imap: ["imap.126.com", 993], smtp: ["smtp.126.com", 465], note: "126 邮箱需使用客户端授权码登录" },
    gmail: { imap: ["imap.gmail.com", 993], smtp: ["smtp.gmail.com", 465], note: "Gmail 需两步验证并生成应用专用密码" },
    outlook: { imap: ["outlook.office365.com", 993], smtp: ["smtp.office365.com", 465], note: "Microsoft 365 建议使用应用密码" },
  };

  const applyPreset = (key: string) => {
    setPreset(key);
    if (key === "custom") return;
    const p = PRESETS[key];
    if (p) {
      setImapHost(p.imap[0]); setImapPort(p.imap[1]);
      setSmtpHost(p.smtp[0]); setSmtpPort(p.smtp[1]);
    }
  };

  const buildReq = (sendTest?: boolean): NewAccountReq => {
    const req: NewAccountReq = {
      email_address: email, display_name: name || undefined, kind, secret,
      sync_days: days,
    };
    if (sendTest !== undefined) req.send_test = sendTest;
    if (kind === "jmap") {
      req.jmap_endpoint = jmapUrl || `https://${email.split("@")[1]}/.well-known/jmap`;
    } else {
      req.imap = { host: imapHost, port: imapPort, security: "implicit_tls" };
      req.smtp = { host: smtpHost, port: smtpPort, security: "implicit_tls" };
    }
    return req;
  };

  const submit = () => {
    if (!email || !secret) { alert("邮箱地址与密码/授权码必填"); return; }
    if (kind === "imap" && (!imapHost || !smtpHost)) { alert("IMAP 账号需填写收/发服务器"); return; }
    onSubmit(buildReq());
  };

  const runTest = async () => {
    if (!email || !secret) { alert("请先填写邮箱地址与密码/授权码"); return; }
    if (kind === "imap" && (!imapHost || !smtpHost)) { alert("IMAP 账号需填写收/发服务器"); return; }
    setTesting(true);
    setTestResult(null);
    try {
      const r = await api.testAccount(buildReq(true));
      if (r.ok) {
        setTestResult(`✅ 验证通过：登录成功${r.send_ok ? "，收发链路正常（测试邮件已发送到本邮箱）" : ""}`);
      } else {
        setTestResult(`❌ ${r.detail || "验证未通过"}`);
      }
    } catch (e) {
      setTestResult(`❌ 验证失败：${e}`);
    }
    setTesting(false);
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>添加账号</h3>
        <div className="row">
          <label>协议</label>
          <select value={kind} onChange={(e) => setKind(e.target.value as "jmap" | "imap")}>
            <option value="jmap">JMAP</option>
            <option value="imap">IMAP + SMTP</option>
          </select>
        </div>
        {kind === "imap" && (
          <div className="row">
            <label>服务商</label>
            <select value={preset} onChange={(e) => applyPreset(e.target.value)}>
              <option value="custom">自定义</option>
              <option value="qq">QQ 邮箱</option>
              <option value="163">163 邮箱</option>
              <option value="126">126 邮箱</option>
              <option value="gmail">Gmail</option>
              <option value="outlook">Outlook / Microsoft 365</option>
            </select>
          </div>
        )}
        <div className="row"><label>邮箱地址</label><input value={email} onChange={(e) => setEmail(e.target.value)} placeholder="user@example.com" /></div>
        <div className="row"><label>显示名</label><input value={name} onChange={(e) => setName(e.target.value)} placeholder="可选" /></div>
        <div className="row"><label>密码 / 授权码</label><input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="服务商标注「授权码」时填授权码而非登录密码" /></div>
        {kind === "jmap" ? (
          <div className="row"><label>JMAP URL</label><input value={jmapUrl} onChange={(e) => setJmapUrl(e.target.value)} placeholder="https://host/.well-known/jmap（域名自动发现）" /></div>
        ) : (
          <>
            <div className="row"><label>IMAP 服务器</label><input value={imapHost} onChange={(e) => setImapHost(e.target.value)} /></div>
            <div className="row"><label>IMAP 端口</label><input type="number" value={imapPort} onChange={(e) => setImapPort(+e.target.value)} /></div>
            <div className="row"><label>SMTP 服务器</label><input value={smtpHost} onChange={(e) => setSmtpHost(e.target.value)} /></div>
            <div className="row"><label>SMTP 端口</label><input type="number" value={smtpPort} onChange={(e) => setSmtpPort(+e.target.value)} /></div>
            {preset !== "custom" && PRESETS[preset] && (
              <div className="row"><label></label><span className="muted small-note">{PRESETS[preset].note}</span></div>
            )}
          </>
        )}
        <div className="row"><label>同步天数</label><input type="number" value={days} onChange={(e) => setDays(+e.target.value)} /></div>
        {testResult && (
          <div className="row"><label></label><span className={`small-note ${testResult.startsWith("✅") ? "test-ok" : "test-fail"}`}>{testResult}</span></div>
        )}
        <div className="modal-actions">
          <button className="btn" disabled={testing} onClick={runTest}>
            {testing ? "验证中…" : "🔌 验证连接"}
          </button>
          <button className="btn" onClick={onClose}>取消</button>
          <button className="btn primary" onClick={submit}>创建</button>
        </div>
      </div>
    </div>
  );
}

// ---------- 账号设置对话框（v1.0 显示名/同步天数/凭证；v1.3 服务器信息） ----------

function AccountSettingsDialog({ account, onClose, onSaved }: {
  account: Account;
  onClose: () => void;
  onSaved: (updated: Account) => void;
}) {
  const [displayName, setDisplayName] = useState(account.display_name ?? "");
  const [syncDays, setSyncDays] = useState(String(account.sync_days ?? 30));
  const [secret, setSecret] = useState("");
  const [saving, setSaving] = useState(false);
  // v1.3：服务器信息编辑（仅展示对应协议的字段）
  const [jmapUrl, setJmapUrl] = useState(account.jmap?.endpoint ?? "");
  const [imapHost, setImapHost] = useState(account.imap?.host ?? "");
  const [imapPort, setImapPort] = useState(account.imap?.port ?? 993);
  const [smtpHost, setSmtpHost] = useState(account.smtp?.host ?? "");
  const [smtpPort, setSmtpPort] = useState(account.smtp?.port ?? 465);
  // v1.5：邮件签名（按账号存 localStorage，写信/回复默认带入）
  const [signature, setSignature] = useState(
    () => localStorage.getItem(`cm-signature:${account.id}`) ?? "");

  // v1.6：账号设置页连接验证——用编辑中的服务器配置探测；密码留空用已存密码；只验登录不发信
  const [verifying, setVerifying] = useState(false);
  const verify = async () => {
    setVerifying(true);
    try {
      const req: Parameters<typeof api.testAccountUpdate>[0] = {
        account_id: account.id, send_test: false,
      };
      if (account.protocol === "Jmap") {
        if (jmapUrl.trim()) req.jmap_endpoint = jmapUrl.trim();
      } else {
        if (imapHost.trim()) req.imap = { host: imapHost.trim(), port: imapPort, security: "implicit_tls" };
        if (smtpHost.trim()) req.smtp = { host: smtpHost.trim(), port: smtpPort, security: "implicit_tls" };
      }
      if (secret) req.secret = secret;
      const r = await api.testAccountUpdate(req);
      alert(r.ok
        ? "✅ 连接正常，登录成功。"
        : `❌ 验证未通过：${r.detail || "登录失败"}`);
    } catch (e) { alert(`验证失败：${e}`); }
    setVerifying(false);
  };

  const save = async () => {
    const days = parseInt(syncDays, 10);
    if (!Number.isNaN(days) && (days < 1 || days > 3650)) {
      alert("同步天数需在 1–3650 之间"); return;
    }
    if (secret && secret.length < 4) { alert("新密码过短"); return; }
    // 签名本地即存即生效（无后端参与）
    const sigKey = `cm-signature:${account.id}`;
    const sigChanged = signature !== (localStorage.getItem(sigKey) ?? "");
    if (sigChanged) localStorage.setItem(sigKey, signature);
    setSaving(true);
    try {
      const req: {
        display_name?: string; sync_days?: number; secret?: string;
        jmap_endpoint?: string; imap?: EndpointReq; smtp?: EndpointReq;
      } = {};
      if (displayName.trim() !== (account.display_name ?? "")) req.display_name = displayName.trim();
      if (!Number.isNaN(days) && days !== account.sync_days) req.sync_days = days;
      if (secret) req.secret = secret;
      if (account.protocol === "Jmap") {
        if (jmapUrl.trim() && jmapUrl.trim() !== (account.jmap?.endpoint ?? "")) {
          req.jmap_endpoint = jmapUrl.trim();
        }
      } else {
        if (imapHost.trim() && (imapHost.trim() !== account.imap?.host || imapPort !== account.imap?.port)) {
          req.imap = { host: imapHost.trim(), port: imapPort, security: "implicit_tls" };
        }
        if (smtpHost.trim() && (smtpHost.trim() !== account.smtp?.host || smtpPort !== account.smtp?.port)) {
          req.smtp = { host: smtpHost.trim(), port: smtpPort, security: "implicit_tls" };
        }
      }
      if (Object.keys(req).length === 0) {
        if (sigChanged) alert("签名已保存，新邮件 / 回复 / 转发时自动带入。");
        onClose(); return;
      }
      const updated = await api.updateAccount(account.id, req);
      onSaved(updated);
      alert(sigChanged
        ? "已保存（含签名）。服务器信息变更后，请重新「收取」一次以建立新连接。"
        : "已保存。服务器信息变更后，请重新「收取」一次以建立新连接。");
    } catch (e) { alert(`保存失败：${e}`); }
    setSaving(false);
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>账号设置 <span className="muted">（{account.email_address}）</span></h3>
        <div className="row"><label>显示名</label>
          <input value={displayName} onChange={(e) => setDisplayName(e.target.value)}
            placeholder="发件时显示的名称" /></div>
        <div className="row"><label>邮件签名</label>
          <textarea className="sig-input" rows={4} value={signature}
            onChange={(e) => setSignature(e.target.value)}
            placeholder="写信 / 回复 / 转发时默认带入的签名，留空则不带" /></div>
        <div className="row"><label>同步天数</label>
          <input type="number" min={1} max={3650} value={syncDays}
            onChange={(e) => setSyncDays(e.target.value)} />
          <span className="muted">首次全量同步回溯的天数</span></div>
        {account.protocol === "Jmap" ? (
          <div className="row"><label>JMAP 端点</label>
            <input value={jmapUrl} onChange={(e) => setJmapUrl(e.target.value)}
              placeholder="https://host/.well-known/jmap" /></div>
        ) : (
          <>
            <div className="row"><label>IMAP 服务器</label>
              <input value={imapHost} onChange={(e) => setImapHost(e.target.value)} /></div>
            <div className="row"><label>IMAP 端口</label>
              <input type="number" value={imapPort} onChange={(e) => setImapPort(+e.target.value)} /></div>
            <div className="row"><label>SMTP 服务器</label>
              <input value={smtpHost} onChange={(e) => setSmtpHost(e.target.value)} /></div>
            <div className="row"><label>SMTP 端口</label>
              <input type="number" value={smtpPort} onChange={(e) => setSmtpPort(+e.target.value)} /></div>
          </>
        )}
        <div className="row"><label>新密码</label>
          <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)}
            placeholder="留空表示不修改（密码/授权码）" /></div>
        <p className="muted small-note">
          当前协议：{account.protocol === "Jmap" ? "JMAP（收发都走 JMAP 端点，无独立 SMTP）" : "IMAP + SMTP"}；
          服务器字段已填入账号当前生效的配置，可直接修改后保存，或先「验证连接」。
        </p>
        <div className="modal-actions">
          <button className="btn" onClick={onClose}>取消</button>
          {/* v1.6：用编辑中的配置 + 已存密码验证，无需重输密码 */}
          <button className="btn" onClick={verify} disabled={verifying}>
            {verifying ? "验证中…" : "🔌 验证连接"}
          </button>
          <button className="btn primary" onClick={save} disabled={saving}>
            {saving ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ---------- 数据目录对话框（v1.2：邮件存储位置可配置） ----------

function DataLocationDialog({ onClose }: { onClose: () => void }) {
  const [loc, setLoc] = useState<{ path: string; custom: boolean } | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.getDataLocation().then(setLoc).catch(() => setLoc(null));
  }, []);

  const change = async () => {
    try {
      const picked = await api.pickFolder();
      if (!picked) return;
      if (!confirm(`将数据目录设为：\n${picked}\n\n确认后重启应用生效。已有数据不会自动移动，需要手动把旧目录里的文件拷贝到新目录。`)) return;
      setBusy(true);
      await api.setDataLocation(picked);
      alert("已保存。请完全退出 CrossMail（托盘右键 → 退出）后重新启动生效。");
      onClose();
    } catch (e) { alert(`设置失败：${e}`); }
    finally { setBusy(false); }
  };

  const reset = async () => {
    try {
      setBusy(true);
      await api.setDataLocation(null);
      alert("已恢复默认位置（exe 同级 crossmail-data）。重启应用生效。");
      onClose();
    } catch (e) { alert(`设置失败：${e}`); }
    finally { setBusy(false); }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>📁 邮件数据存储位置</h3>
        {loc ? (
          <div className="dataloc-info">
            <div className="dataloc-path" title={loc.path}>{loc.path}</div>
            <span className={`badge ${loc.custom ? "warn" : ""}`}>
              {loc.custom ? "自定义位置" : "默认（exe 同级）"}
            </span>
          </div>
        ) : <p className="muted">读取中…</p>}
        <p className="muted small-note">
          包含数据库、加密密钥、eml 归档与正文缓存。更改位置后需重启应用；
          旧目录数据不会自动迁移，请手动拷贝全部内容到新位置。
        </p>
        <div className="modal-actions">
          <button className="btn" disabled={busy} onClick={reset}>恢复默认</button>
          <button className="btn" onClick={onClose}>关闭</button>
          <button className="btn primary" disabled={busy} onClick={change}>更改位置…</button>
        </div>
      </div>
    </div>
  );
}

// ---------- 顶部菜单栏（v1.3，Foxmail 风格） ----------

function MenuBar({ accounts, mailboxes, currentAccountId, onCompose,
  onSelectAccount, onOpenAccountSettings, onOpenDataDir, onOpenRules, onOpenFolders,
  onAddAccount, onDeleteAccount, hasAccount, onSyncAll,
  autoStart, onToggleAutoStart, autoFetch, onToggleAutoFetch }: {
  accounts: Account[]; mailboxes: Mailbox[]; currentAccountId: string | null;
  onCompose: () => void;
  onSelectAccount: (id: string) => void; onOpenAccountSettings: () => void;
  onOpenDataDir: () => void; onOpenRules: () => void; onOpenFolders: () => void;
  onAddAccount: () => void; onDeleteAccount: () => void; hasAccount: boolean;
  onSyncAll: (silent?: boolean) => void;
  autoStart: boolean; onToggleAutoStart: () => void;
  autoFetch: boolean; onToggleAutoFetch: () => void;
}) {
  const [open, setOpen] = useState<string | null>(null);

  const item = (label: string, action: () => void): ReactNode => (
    <div key={label} className="menu-item" onClick={() => { setOpen(null); action(); }}>{label}</div>
  );

  const menus: Record<string, ReactNode> = {
    // v1.5：「写邮件」不再挂下拉，直点即写
    "收取": (
      <>
        {/* v1.5：去重——只保留「收取全部账号」（含当前账号），加自动收取开关 */}
        {item("🔄 收取全部账号", () => onSyncAll(false))}
        <div className="menu-sep" />
        <div className="menu-item" onClick={() => { setOpen(null); onToggleAutoFetch(); }}>
          {autoFetch ? "✓" : "○"} 自动收取（每 60 秒）
        </div>
      </>
    ),
    "常用文件夹": (
      <>
        {mailboxes.slice(0, 12).map((m) => (
          <div key={m.id} className="menu-item"
            onClick={() => { setOpen(null); onSelectAccount(m.account_id); window.dispatchEvent(new CustomEvent("cm-select-mailbox", { detail: m.id })); }}>
            {ROLE_ICON[m.role] ?? "📁"} {mailboxLabel(m.name)}（{m.total}）
          </div>
        ))}
        {mailboxes.length === 0 && <div className="menu-item muted">暂无文件夹</div>}
      </>
    ),
    "账号": (
      <>
        {accounts.map((a) => (
          <div key={a.id} className="menu-item"
            onClick={() => { setOpen(null); onSelectAccount(a.id); }}>
            {a.protocol === "Jmap" ? "J" : "I"} {a.email_address}
            {a.id === currentAccountId ? " ✓" : ""}
          </div>
        ))}
        <div className="menu-sep" />
        {item("⚙ 账号管理（当前账号设置）", onOpenAccountSettings)}
        {item("＋ 添加账号…", onAddAccount)}
        {hasAccount && item("🗑️ 删除当前账号", onDeleteAccount)}
      </>
    ),
    "工具": (
      <>
        {item("📁 文件夹管理（新建 / 排序 / 删除）", onOpenFolders)}
        {item("📜 收信规则", onOpenRules)}
        {item("🗄️ 数据目录…", onOpenDataDir)}
      </>
    ),
    "帮助": (
      <>
        {/* v1.4：开机自启从侧栏移入帮助菜单，可勾选 */}
        <div className="menu-item" onClick={() => { setOpen(null); onToggleAutoStart(); }}>
          {autoStart ? "✓" : "○"} 开机自动启动 CrossMail
        </div>
        <div className="menu-sep" />
        {item("ℹ️ 关于 CrossMail",
          () => alert("CrossMail v1.7\nJMAP + IMAP/SMTP 双协议本地邮件客户端\n绿色便携 · 数据仅存本机"))}
      </>
    ),
  };

  return (
    <div className="menubar" onMouseLeave={() => setOpen(null)}>
      {/* v1.5：写邮件直点即写（无下拉菜单） */}
      <div className="menubar-entry">
        <span className="menubar-label compose" title="写邮件"
          onClick={() => { setOpen(null); onCompose(); }}
        >✉️ 写邮件</span>
      </div>
      {Object.keys(menus).map((key) => (
        <div key={key} className="menubar-entry">
          <span
            className={open === key ? "menubar-label active" : "menubar-label"}
            onClick={() => setOpen(open === key ? null : key)}
            onMouseEnter={() => { if (open && open !== key) setOpen(key); }}
          >{key}</span>
          {open === key && <div className="menu-dropdown">{menus[key]}</div>}
        </div>
      ))}
      {open && <div className="menu-mask" onClick={() => setOpen(null)} />}
    </div>
  );
}

// ---------- 文件夹管理对话框（v1.3：新建 / 删除 / 排序） ----------

const FOLDER_ORDER_KEY = (accountId: string) => `cm-folder-order-${accountId}`;

export function getOrderedMailboxes(accountId: string | null, mailboxes: Mailbox[]): Mailbox[] {
  if (!accountId) return mailboxes;
  try {
    const raw = localStorage.getItem(FOLDER_ORDER_KEY(accountId));
    if (!raw) return mailboxes;
    const order: string[] = JSON.parse(raw);
    const idx = (id: string) => {
      const i = order.indexOf(id);
      return i === -1 ? order.length : i;
    };
    return [...mailboxes].sort((a, b) => idx(a.id) - idx(b.id));
  } catch {
    return mailboxes;
  }
}

function FolderManagerDialog({ accountId, mailboxes, onClose, onChanged }: {
  accountId: string; mailboxes: Mailbox[]; onClose: () => void; onChanged: () => void;
}) {
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);
  const ordered = getOrderedMailboxes(accountId, mailboxes);

  const persistOrder = (ids: string[]) => {
    localStorage.setItem(FOLDER_ORDER_KEY(accountId), JSON.stringify(ids));
    onChanged();
  };

  const move = (id: string, dir: -1 | 1) => {
    const ids = ordered.map((m) => m.id);
    const i = ids.indexOf(id);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= ids.length) return;
    [ids[i], ids[j]] = [ids[j], ids[i]];
    persistOrder(ids);
  };

  const create = async () => {
    const name = newName.trim();
    if (!name) return;
    setBusy(true);
    try {
      await api.createMailbox(accountId, name);
      setNewName("");
      onChanged();
    } catch (e) { alert(`创建失败：${e}`); }
    setBusy(false);
  };

  const remove = async (m: Mailbox) => {
    if (m.role !== "Other") { alert("系统文件夹（收件箱/已发送等）不可删除"); return; }
    if (!confirm(`确定删除文件夹「${mailboxLabel(m.name)}」？其中的邮件会被一并删除或移入废纸（取决于服务器）。`)) return;
    setBusy(true);
    try {
      await api.deleteMailbox(accountId, m.id);
      onChanged();
    } catch (e) { alert(`删除失败：${e}`); }
    setBusy(false);
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>📁 文件夹管理</h3>
        <div className="folder-list">
          {ordered.map((m, i) => (
            <div key={m.id} className="folder-row">
              <span className="folder-name">{ROLE_ICON[m.role] ?? "📁"} {mailboxLabel(m.name)}
                <span className="muted">（{m.name}）</span></span>
              <button className="btn tiny" disabled={i === 0} onClick={() => move(m.id, -1)}>↑</button>
              <button className="btn tiny" disabled={i === ordered.length - 1} onClick={() => move(m.id, 1)}>↓</button>
              <button className="btn tiny danger" disabled={m.role !== "Other"} title={m.role !== "Other" ? "系统文件夹不可删除" : "删除"}
                onClick={() => remove(m)}>🗑️</button>
            </div>
          ))}
        </div>
        <div className="row">
          <label>新建文件夹</label>
          <input value={newName} onChange={(e) => setNewName(e.target.value)} placeholder="文件夹名称"
            onKeyDown={(e) => { if (e.key === "Enter") create(); }} />
          <button className="btn small" disabled={busy || !newName.trim()} onClick={create}>创建</button>
        </div>
        <div className="modal-actions">
          <button className="btn primary" onClick={onClose}>完成</button>
        </div>
      </div>
    </div>
  );
}

// ---------- 收信规则对话框（v1.3） ----------

const FIELD_LABEL: Record<string, string> = { from: "发件人", to: "收件人", subject: "主题" };

function RulesDialog({ accountId, mailboxes, onClose, onChanged }: {
  accountId: string; mailboxes: Mailbox[]; onClose: () => void; onChanged: () => void;
}) {
  const [rules, setRules] = useState<MailRule[]>([]);
  const [name, setName] = useState("");
  const [field, setField] = useState("from");
  const [keyword, setKeyword] = useState("");
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [applyResult, setApplyResult] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try { setRules(await api.listRules(accountId)); } catch { /* ignore */ }
  }, [accountId]);

  useEffect(() => { reload(); }, [reload]);

  const mbName = (id: string) => {
    const m = mailboxes.find((x) => x.id === id);
    return m ? mailboxLabel(m.name) : id;
  };

  const create = async () => {
    if (!keyword.trim()) { alert("关键词不能为空"); return; }
    if (!target) { alert("请选择目标文件夹"); return; }
    setBusy(true);
    try {
      await api.saveRule(accountId, name.trim() || `${FIELD_LABEL[field]}含「${keyword.trim()}」`, field, keyword.trim(), target);
      setKeyword(""); setName("");
      await reload();
      onChanged();
    } catch (e) { alert(`创建规则失败：${e}`); }
    setBusy(false);
  };

  const toggle = async (r: MailRule) => {
    try {
      await api.toggleRule(r.id, !r.enabled);
      await reload();
    } catch (e) { alert(`操作失败：${e}`); }
  };

  const remove = async (r: MailRule) => {
    if (!confirm(`删除规则「${r.name}」？已移动的邮件不会被移回。`)) return;
    try {
      await api.deleteRule(r.id);
      await reload();
    } catch (e) { alert(`删除失败：${e}`); }
  };

  const applyNow = async () => {
    setBusy(true);
    setApplyResult(null);
    try {
      const r = await api.applyRules(accountId);
      setApplyResult(`完成：命中 ${r.matched} 封，移动 ${r.moved} 封${r.failed ? `，失败 ${r.failed} 封` : ""}`);
      onChanged();
    } catch (e) { setApplyResult(`应用失败：${e}`); }
    setBusy(false);
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h3>📜 收信规则 <span className="muted">（收取/同步后自动按规则移动邮件）</span></h3>
        <div className="rule-list">
          {rules.length === 0 && <p className="muted">暂无规则。添加规则后，新收取的邮件会自动归类到指定文件夹。</p>}
          {rules.map((r) => (
            <div key={r.id} className="rule-row">
              <label className="rule-toggle">
                <input type="checkbox" checked={r.enabled} onChange={() => toggle(r)} /> 启用
              </label>
              <span className="rule-desc">
                <b>{r.name}</b>：{FIELD_LABEL[r.field] ?? r.field} 含「{r.keyword}」 → {mbName(r.target_mailbox_id)}
              </span>
              <button className="btn tiny danger" onClick={() => remove(r)}>删除</button>
            </div>
          ))}
        </div>
        <div className="rule-new">
          <div className="row">
            <label>规则名</label>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="可选，留空自动生成" />
          </div>
          <div className="row">
            <label>匹配</label>
            <select value={field} onChange={(e) => setField(e.target.value)}>
              <option value="from">发件人</option>
              <option value="to">收件人</option>
              <option value="subject">主题</option>
            </select>
            <span className="muted">包含</span>
            <input value={keyword} onChange={(e) => setKeyword(e.target.value)} placeholder="关键词" />
          </div>
          <div className="row">
            <label>移入</label>
            <select value={target} onChange={(e) => setTarget(e.target.value)}>
              <option value="">选择文件夹…</option>
              {mailboxes.map((m) => (
                <option key={m.id} value={m.id}>{mailboxLabel(m.name)}（{m.name}）</option>
              ))}
            </select>
            <button className="btn small primary" disabled={busy} onClick={create}>添加规则</button>
          </div>
        </div>
        {applyResult && <p className="small-note">{applyResult}</p>}
        <div className="modal-actions">
          <button className="btn" disabled={busy} onClick={applyNow}>⚡ 立即应用</button>
          <button className="btn primary" onClick={onClose}>完成</button>
        </div>
      </div>
    </div>
  );
}

// ---------- 移动到文件夹对话框（v0.9：服务器搜索结果移动；v1.0 复用于批量移动） ----------

function MoveDialog({ mailboxes, excludeId, onClose, onMove }: {
  mailboxes: Mailbox[]; excludeId?: string; onClose: () => void;
  onMove: (mailboxId: string, mailboxName: string) => void;
}) {
  const list = mailboxes.filter((m) => m.id !== excludeId);
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>移动到…</h3>
        <div className="move-list">
          {list.length === 0 && <div className="empty">没有可选文件夹</div>}
          {list.map((m) => (
            <div key={m.id} className="move-item" onClick={() => onMove(m.id, m.name)}>
              {ROLE_ICON[m.role] ?? "📁"} {mailboxLabel(m.name)}
            </div>
          ))}
        </div>
        <div className="modal-actions">
          <button className="btn" onClick={onClose}>取消</button>
        </div>
      </div>
    </div>
  );
}

// ---------- 写邮件对话框（v0.3：预填回复/转发/草稿 + 密送 + 保存草稿；v0.7：富文本模式） ----------

const escapeHtml = (s: string): string =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

function ComposeDialog({ defaultFrom, prefill, onClose, onSend, onSaveDraft }: {
  defaultFrom: string; prefill: ComposePrefill; onClose: () => void;
  onSend: (v: { to: string[]; cc: string[]; bcc: string[]; subject: string; body: string; html?: string | null; attachments?: AttFile[]; inReplyTo?: string; references?: string[]; draftId?: string }) => void;
  onSaveDraft: (v: { to: string[]; cc: string[]; bcc: string[]; subject: string; body: string; html?: string | null; attachments?: AttFile[]; inReplyTo?: string; references?: string[]; draftId?: string }) => void;
}) {
  const [to, setTo] = useState(prefill.to ?? "");
  const [cc, setCc] = useState(prefill.cc ?? "");
  const [bcc, setBcc] = useState(prefill.bcc ?? "");
  const [subject, setSubject] = useState(prefill.subject ?? "");
  const [body, setBody] = useState(prefill.body ?? "");
  const [atts, setAtts] = useState<AttFile[]>(prefill.initialAtts ?? []);
  const [showCc, setShowCc] = useState(!!prefill.cc);
  const [showBcc, setShowBcc] = useState(!!prefill.bcc);
  // 富文本（v0.7）：HTML 草稿默认进入富文本模式，其余默认纯文本
  const [mode, setMode] = useState<"text" | "html">(prefill.html ? "html" : "text");
  const [initialHtml, setInitialHtml] = useState(prefill.html ?? "");
  const editorRef = useRef<HTMLDivElement | null>(null);

  const addAtts = async () => {
    try {
      const picked = await api.pickAttachments();
      if (picked.length) setAtts((cur) => [...cur, ...picked]);
    } catch (e) { alert(`读取附件失败：${e}`); }
  };

  const exec = (cmd: string) => {
    editorRef.current?.focus();
    document.execCommand(cmd, false);
  };

  // 模式切换：纯文本 → 富文本时把换行转成 <p>；富文本 → 纯文本取 innerText
  const switchToHtml = () => {
    if (mode === "html") return;
    setInitialHtml(body.trim() ? body.split("\n").map((l) => `<p>${escapeHtml(l) || "<br>"}</p>`).join("") : "");
    setMode("html");
  };
  const switchToText = () => {
    if (mode === "text") return;
    setBody(editorRef.current?.innerText ?? "");
    setMode("text");
  };

  const build = () => {
    const split = (s: string) => s.split(/[,;，；]/).map((x) => x.trim()).filter(Boolean);
    const tos = split(to);
    const ccs = split(cc);
    const bccs = split(bcc);
    if (!tos.length) { alert("收件人不能为空"); return null; }
    const html = mode === "html" ? (editorRef.current?.innerHTML ?? "") : null;
    const text = mode === "html" ? (editorRef.current?.innerText ?? "") : body;
    return {
      to: tos, cc: ccs, bcc: bccs, subject, body: text,
      html: html && html.trim() ? html : null,
      attachments: atts.length ? atts : undefined,
      inReplyTo: prefill.inReplyTo, references: prefill.references,
      draftId: prefill.draftId,
    };
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h3>
          {prefill.draftId ? "编辑草稿" : "写邮件"}
          <span className="muted">（{defaultFrom}）</span>
        </h3>
        <div className="row"><label>收件人</label><input value={to} onChange={(e) => setTo(e.target.value)} placeholder="多个用逗号分隔" /></div>
        {showCc
          ? <div className="row"><label>抄送</label><input value={cc} onChange={(e) => setCc(e.target.value)} /></div>
          : <div className="row"><label></label><a className="linklike" onClick={() => setShowCc(true)}>＋ 抄送</a></div>}
        {showBcc
          ? <div className="row"><label>密送</label><input value={bcc} onChange={(e) => setBcc(e.target.value)} /></div>
          : <div className="row"><label></label><a className="linklike" onClick={() => setShowBcc(true)}>＋ 密送</a></div>}
        <div className="row"><label>主题</label><input value={subject} onChange={(e) => setSubject(e.target.value)} /></div>
        <div className="compose-editor-head">
          <button className={`mode-tab ${mode === "text" ? "active" : ""}`} onClick={switchToText}>纯文本</button>
          <button className={`mode-tab ${mode === "html" ? "active" : ""}`} onClick={switchToHtml}>富文本</button>
          {mode === "html" && (
            <span className="html-tools">
              <button className="tool" onClick={() => exec("bold")} title="加粗"><b>B</b></button>
              <button className="tool" onClick={() => exec("italic")} title="斜体"><i>I</i></button>
              <button className="tool" onClick={() => exec("underline")} title="下划线"><u>U</u></button>
              <button className="tool" onClick={() => exec("insertUnorderedList")} title="列表">•≡</button>
            </span>
          )}
        </div>
        {mode === "html"
          ? <div className="compose-html" ref={editorRef} contentEditable suppressContentEditableWarning
              dangerouslySetInnerHTML={{ __html: initialHtml }} data-placeholder="正文…" />
          : <textarea value={body} onChange={(e) => setBody(e.target.value)} placeholder="正文…" rows={12} />}
        <div className="compose-atts">
          <button className="btn small" onClick={addAtts}>📎 添加附件</button>
          {atts.map((a, i) => (
            <span key={`${a.name}-${i}`} className="att-chip" title={a.content_type}>
              📎 {a.name}
              <button className="att-x" onClick={() => setAtts((cur) => cur.filter((_, j) => j !== i))}>×</button>
            </span>
          ))}
        </div>
        <div className="modal-actions">
          <button className="btn" onClick={() => { const v = build(); if (v) onSaveDraft(v); }}>💾 保存草稿</button>
          <button className="btn" onClick={onClose}>取消</button>
          <button className="btn primary" onClick={() => { const v = build(); if (v) onSend(v); }}>发送</button>
        </div>
      </div>
    </div>
  );
}
