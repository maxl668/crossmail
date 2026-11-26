using System.Collections.Generic;
using System.Threading.Tasks;
using CrossMail.Bindings;

namespace CrossMail.App.Services;

// 所有 UDL 接口都是同步阻塞，UI 线程禁止直调。
// 每次调用在后台线程内创建并释放 engine，避免跨线程持有 SafeHandle。
// 异常一律向上抛，由视图显示原文；这里不吞错、不填假数据。
public static class MailService
{
    private static CrossMailEngine Open() => CrossMailCore.OpenEngine(CoreBridge.CreateConfig());

    public static Task<IReadOnlyList<AccountDto>> AccountsAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ListAccounts();
    });

    public static Task<IReadOnlyList<MailboxDto>> MailboxesAsync(string accountId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ListMailboxes(accountId);
    });

    public static Task<IReadOnlyList<MailboxCountDto>> UnreadAsync(string accountId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.UnreadCounts(accountId);
    });

    public static Task<EmailPageDto> EmailsAsync(string accountId, EmailQueryDto query) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ListEmails(accountId, query);
    });

    public static Task<EmailPageDto> SearchAsync(string accountId, string keyword, uint limit) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.Search(accountId, keyword, SearchScopeDto.All, limit, 0U);
    });

    public static Task<EmailDto> EmailAsync(string emailId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.GetEmail(emailId);
    });

    public static Task<string?> BodyAsync(string emailId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ReadBody(emailId, false);
    });

    public static Task<RenderedBodyDto> RenderAsync(string emailId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.RenderBody(emailId, false, false);
    });

    public static Task<SenderTrustDto> TrustAsync(string emailId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.SenderTrust(emailId);
    });

    // 一次后台调用取齐阅读窗格所需数据，避免四次开关 SQLite 连接。
    public static Task<(EmailDto Email, string? Body, RenderedBodyDto Render, SenderTrustDto Trust)> DetailAsync(string emailId) =>
        CoreBridge.RunAsync(() =>
        {
            using var e = Open();
            return (e.GetEmail(emailId), e.ReadBody(emailId, false),
                    e.RenderBody(emailId, false, false), e.SenderTrust(emailId));
        });

    public static Task<EmailDto> SetReadAsync(string emailId, bool read) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.SetRead(emailId, read);
    });

    public static Task<EmailDto> StarAsync(string emailId, bool starred) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ToggleStar(emailId, starred);
    });

    public static Task<EmailDto> ArchiveAsync(string emailId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ArchiveEmail(emailId);
    });

    public static Task<DeleteOutcomeDto> DeleteAsync(string emailId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.DeleteEmail(emailId);
    });

    public static Task<IReadOnlyList<EmailDto>> DraftsAsync(string accountId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ListDrafts(accountId, 50U);
    });

    public static Task<EmailDto> SaveDraftAsync(string accountId, DraftDto draft) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.SaveDraft(accountId, draft);
    });

    // 核心没有 update_draft / delete_draft：save_draft 每次都生成新的 UUID 草稿。
    // 因此自动保存只落本地 JSON，只有用户显式动作才写核心库（见 ComposeWindow）。
    public static Task<EmailDto> MarkSentAsync(string draftId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.MarkSent(draftId, null);
    });

    // 真发信：核心内部「先落草稿 → SMTP/JMAP 提交 → 草稿转已发送」。
    // 失败会抛出 CrossMailException（认证/网络/服务器拒收），草稿仍留在草稿箱。
    public static Task<SendReceiptDto> SendAsync(string accountId, DraftDto draft) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.SendEmail(accountId, draft);
    });

    public static Task<SyncReportDto> SyncAsync(string accountId) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.SyncAccount(accountId);
    });
}
