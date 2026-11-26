using System;

namespace CrossMail.App.Services;

// 全局 UI 状态。旧版每个页面各自手填 accountId / mailboxId / emailId，
// 无法完成「选账号 → 选文件夹 → 选邮件 → 就地操作」的连续任务。
// 这里只存 UI 选择，不缓存任何凭据，不持有 SafeHandle。
public sealed class AppState
{
    public static AppState Current { get; } = new();

    public string? AccountId { get; set; }
    public string? MailboxId { get; set; }
    public string? EmailId { get; set; }
    public string Query { get; set; } = "";
    public CoreSnapshot? Snapshot { get; set; }

    // 推送事件驱动的未读角标。null = 尚未收到任何推送事件（不伪造 0）。
    public ulong? UnreadTotal { get; set; }
    public string? LastPushSummary { get; set; }

    // v1.7：新邮件应用内醒目提醒（浏览状态），由推送事件写入
    public string? MailAlert { get; set; }

    public event Action? Changed;

    public void Notify() => Changed?.Invoke();

    public void ResetSelection()
    {
        MailboxId = null;
        EmailId = null;
        Notify();
    }
}
