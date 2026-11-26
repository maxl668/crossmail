// Tauri 命令封装：与 src-tauri/src/main.rs 的命令一一对应
import { invoke } from "@tauri-apps/api/core";
import { save, open } from "@tauri-apps/plugin-dialog";

export interface Account {
  id: string;
  email_address: string;
  display_name: string | null;
  protocol: "Jmap" | "Imap";
  sync_days: number;
  jmap?: { endpoint: string; credential: unknown } | null;
  imap?: { host: string; port: number; security: string; credential: unknown } | null;
  smtp?: { host: string; port: number; security: string; credential: unknown } | null;
}

export interface MailRule {
  id: string;
  account_id: string;
  name: string;
  field: string;
  keyword: string;
  target_mailbox_id: string;
  enabled: boolean;
  created_at: string;
}

export interface Mailbox {
  id: string;
  account_id: string;
  name: string;
  role: string;
  total: number;
  unread: number;
}

export interface Attachment {
  id: string;
  name: string | null;
  content_type: string;
  size: number;
  blob_id: string | null;
  part_id: string | null;
  inline: boolean;
  cid: string | null;
}

export interface Email {
  id: string;
  account_id: string;
  mailbox_ids: string[];
  thread_id?: string | null;
  subject: string;
  from: { name: string | null; email: string }[];
  to: { name: string | null; email: string }[];
  cc: { name: string | null; email: string }[];
  bcc?: { name: string | null; email: string }[];
  reply_to?: { name: string | null; email: string }[];
  message_id?: string | null;
  in_reply_to?: string | null;
  references?: string[];
  preview: string;
  sent_at: string | null;
  received_at: string;
  flags: (string | { Custom: string })[];
  size: number;
  attachments?: Attachment[];
}

export interface EmailPage {
  items: Email[];
  total: number;
  next_offset: number | null;
}

export interface ReadEmail {
  email: Email;
  text_body: string | null;
  html_body: string | null;
  fetched_remote: boolean;
}

export interface EndpointReq {
  host: string;
  port: number;
  security: "implicit_tls" | "start_tls";
}

export interface NewAccountReq {
  email_address: string;
  display_name?: string;
  kind: "jmap" | "imap";
  jmap_endpoint?: string;
  imap?: EndpointReq;
  smtp?: EndpointReq;
  secret: string;
  sync_days?: number;
  send_test?: boolean;
}

export interface AttFile {
  name: string;
  content_type: string;
  data: string; // base64
}

export interface DraftReq {
  to: string[];
  cc?: string[];
  bcc?: string[];
  subject: string;
  body: string;
  html_body?: string | null;
  attachments?: { name: string; content_type: string; data: string }[];
  in_reply_to?: string | null;
  references?: string[];
}

export interface SyncSummary {
  inserted: number;
  updated: number;
  deleted: number;
  unchanged: number;
  has_more: boolean;
  full_resync: boolean;
}

// v1.6 修复：核心层 ProtocolKind serde 序列化为小写（"jmap"/"imap"），前端此前
// 全用 "Jmap" 比较，永远不命中——导致账号设置对 JMAP 账号显示空白的 IMAP/SMTP
// 字段、侧栏协议徽标全显示 I。统一在此归一化，UI 层保持 "Jmap"|"Imap"。
function normAccount<T extends { protocol: string }>(a: T): T & { protocol: "Jmap" | "Imap" } {
  return {
    ...a,
    protocol: a.protocol === "jmap" ? "Jmap" : a.protocol === "imap" ? "Imap" : (a.protocol as "Jmap" | "Imap"),
  };
}

export const api = {
  listAccounts: async () => (await invoke<Account[]>("list_accounts")).map(normAccount),
  createAccount: async (req: NewAccountReq) => normAccount(await invoke<Account>("create_account", { req })),
  deleteAccount: (accountId: string) => invoke<void>("delete_account", { accountId }),
  updateAccount: async (
    accountId: string,
    req: {
      display_name?: string; sync_days?: number; secret?: string;
      jmap_endpoint?: string;
      imap?: EndpointReq; smtp?: EndpointReq;
    },
  ) => normAccount(await invoke<Account>("update_account", { accountId, req })),
  listMailboxes: (accountId: string) => invoke<Mailbox[]>("list_mailboxes", { accountId }),
  listEmails: (accountId: string, mailboxId?: string, unreadOnly?: boolean, offset?: number) =>
    invoke<EmailPage>("list_emails", { accountId, mailboxId, unreadOnly, offset }),
  searchEmails: (accountId: string, keyword: string, offset?: number) =>
    invoke<EmailPage>("search_emails", { accountId, keyword, offset }),
  serverSearch: (accountId: string, keyword: string, limit?: number) =>
    invoke<Email[]>("server_search", { accountId, keyword, limit }),
  serverReadEmail: (accountId: string, emailId: string) =>
    invoke<ReadEmail>("server_read_email", { accountId, emailId }),
  serverSetFlag: (accountId: string, emailId: string, flag: string, enabled: boolean) =>
    invoke<void>("server_set_flag", { accountId, emailId, flag, enabled }),
  serverDeleteEmail: (accountId: string, emailId: string) =>
    invoke<void>("server_delete_email", { accountId, emailId }),
  // eml 归档（v1.1）：阅读时自动触发（幂等）；exportEml 另存为到用户指定路径
  archiveEml: (accountId: string, emailId: string) =>
    invoke<string>("archive_eml", { accountId, emailId }).catch(() => null),
  exportEml: async (accountId: string, emailId: string, subject: string): Promise<string | null> => {
    const path = await save({
      title: "导出邮件为 eml",
      defaultPath: `${(subject || "邮件").replace(/[\\/:*?"<>|]/g, "_").slice(0, 80)}.eml`,
    });
    if (!path) return null;
    return invoke<string>("export_eml", { accountId, emailId, savePath: path });
  },
  serverMoveEmail: (accountId: string, emailId: string, fromMailbox: string | undefined, toMailbox: string) =>
    invoke<void>("server_move_email", { accountId, emailId, fromMailbox, toMailbox }),
  readStoredAttachments: (emailId: string, parts: string[]) =>
    invoke<AttFile[]>("read_stored_attachments", { emailId, parts }),
  saveAttachment: async (accountId: string, emailId: string, att: Attachment): Promise<string | null> => {
    // IMAP 附件无 blobId，按 v0.8 约定拼 `{emailId}#{partId}` 走 provider.download_blob
    const blobId = att.blob_id ?? (att.part_id ? `${emailId}#${att.part_id}` : null);
    if (!blobId) throw new Error("该附件既无 blobId 也无 partId，无法下载");
    const path = await save({
      title: "保存附件",
      defaultPath: att.name ?? `attachment-${att.id}`,
    });
    if (!path) return null;
    const written = await invoke<number>("download_attachment", {
      accountId, blobId, savePath: path,
    });
    return `${path}（${written} 字节）`;
  },
  readEmail: (accountId: string, emailId: string) =>
    invoke<ReadEmail>("read_email", { accountId, emailId }),
  setRead: (emailId: string, read: boolean) => invoke<Email>("set_read", { emailId, read }),
  toggleStar: (emailId: string, starred: boolean) => invoke<Email>("toggle_star", { emailId, starred }),
  syncAccount: (accountId: string) => invoke<SyncSummary>("sync_account", { accountId }),
  sendEmail: (
    accountId: string, subject: string, to: string[],
    cc?: string[], body?: string, bcc?: string[],
    inReplyTo?: string, references?: string[],
    attachments?: AttFile[], htmlBody?: string | null,
  ) => invoke("send_email", { accountId, subject, to, cc, body, htmlBody, bcc, inReplyTo, references, attachments }),
  pickAttachments: async (): Promise<AttFile[]> => {
    const paths = await open({ title: "选择附件", multiple: true });
    if (!paths) return [];
    const list = Array.isArray(paths) ? paths : [paths];
    if (!list.length) return [];
    return invoke<AttFile[]>("read_attachment_files", { paths: list });
  },
  saveDraft: (accountId: string, req: DraftReq) => invoke<Email>("save_draft", { accountId, req }),
  listDrafts: (accountId: string) => invoke<Email[]>("list_drafts", { accountId }),
  sendDraft: (accountId: string, draftId: string) => invoke("send_draft", { accountId, draftId }),
  deleteDraft: (draftId: string) => invoke<void>("delete_draft", { draftId }),
  autostartStatus: () => invoke<boolean>("autostart_status"),
  autostartSet: (enable: boolean) => invoke<boolean>("autostart_set", { enable }),
  pushSubscribe: (accountId: string) => invoke<boolean>("push_subscribe", { accountId }),
  pushStop: (accountId: string) => invoke<boolean>("push_stop", { accountId }),
  getDataLocation: () => invoke<{ path: string; custom: boolean }>("get_data_location"),
  setDataLocation: (path: string | null) => invoke<void>("set_data_location", { path }),
  pickFolder: () => open({ title: "选择数据目录", directory: true }) as Promise<string | null>,
  // v1.3：连接验证 / 文件夹管理 / 收信规则
  testAccount: (req: NewAccountReq) =>
    invoke<{ ok: boolean; login_ok: boolean; send_ok: boolean | null; detail: string }>(
      "test_account", { req },
    ),
  // v1.6：账号设置页验证（用编辑中的服务器配置 + 已存密码，默认只验登录不发信）
  testAccountUpdate: (req: {
    account_id: string;
    jmap_endpoint?: string;
    imap?: EndpointReq; smtp?: EndpointReq;
    secret?: string; send_test?: boolean;
  }) =>
    invoke<{ ok: boolean; login_ok: boolean; send_ok: boolean | null; detail: string }>(
      "test_account_update", { req },
    ),
  createMailbox: (accountId: string, name: string) =>
    invoke<Mailbox>("create_mailbox", { accountId, name }),
  deleteMailbox: (accountId: string, mailboxId: string) =>
    invoke<void>("delete_mailbox", { accountId, mailboxId }),
  listRules: (accountId: string) => invoke<MailRule[]>("list_rules", { accountId }),
  saveRule: (accountId: string, name: string, field: string, keyword: string, targetMailboxId: string) =>
    invoke<MailRule>("save_rule", { accountId, name, field, keyword, targetMailboxId }),
  deleteRule: (ruleId: string) => invoke<void>("delete_rule", { ruleId }),
  toggleRule: (ruleId: string, enabled: boolean) => invoke<void>("toggle_rule", { ruleId, enabled }),
  applyRules: (accountId: string) =>
    invoke<{ matched: number; moved: number; failed: number }>("apply_rules", { accountId }),
};

export function addrName(a: { name: string | null; email: string }): string {
  return a.name && a.name.length > 0 ? a.name : a.email;
}

/// 邮件时间本地化：后端统一传 UTC RFC3339（...Z / +00:00），
/// 必须经 Date 解析后再按设备时区格式化，不能直接截取字符串。
export function formatMailTime(iso: string | null | undefined, withSeconds = false): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso.replace("T", " ").slice(0, withSeconds ? 19 : 16);
  const p = (n: number) => String(n).padStart(2, "0");
  const base = `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ` +
    `${p(d.getHours())}:${p(d.getMinutes())}`;
  return withSeconds ? `${base}:${p(d.getSeconds())}` : base;
}

export function hasFlag(e: Email, f: string): boolean {
  return e.flags.some((x) =>
    typeof x === "string" ? x.toLowerCase() === f.toLowerCase() : false,
  );
}
