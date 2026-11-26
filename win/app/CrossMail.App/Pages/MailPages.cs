using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using CrossMail.Bindings;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

public sealed class MailListPage : CorePage
{
    private readonly TextBox _account = Field("账号 ID", "");
    private readonly TextBox _mailbox = Field("邮箱 ID（可空）", "");
    private readonly TextBox _emailId = Field("邮件 ID", "");
    private readonly TextBox _keyword = Field("搜索关键字", "");

    public MailListPage() : base("邮件列表", "S5-04")
    {
        Root.Children.Add(Row(_account, _mailbox));
        Root.Children.Add(Row(_emailId, _keyword));
        Root.Children.Add(Row(
            Action("邮箱列表 (list_mailboxes)", Mailboxes),
            Action("邮件列表 (list_emails)", Emails),
            Action("未读计数 (unread_counts)", Unread)));
        Root.Children.Add(Row(
            Action("搜索 (search)", Search),
            Action("标记已读 (set_read)", SetRead),
            Action("星标 (toggle_star)", Star),
            Action("删除 (delete_email)", Delete)));
    }

    private async Task Mailboxes()
    {
        string acc = _account.Text;
        var list = await WithEngine("list_mailboxes", e => e.ListMailboxes(acc));
        if (list is not null)
            Show(list.Count == 0 ? "mailboxes.count = 0" :
                string.Join(Environment.NewLine, list.Select(m => $"{m.Id} | {m.Name} | {m.Role} | total={m.Total} unread={m.Unread}")));
    }

    private async Task Emails()
    {
        string acc = _account.Text, box = _mailbox.Text;
        var q = new EmailQueryDto
        {
            MailboxId = string.IsNullOrWhiteSpace(box) ? null : box,
            UnreadOnly = false, Limit = 20U, Offset = 0U,
        };
        var page = await WithEngine("list_emails", e => e.ListEmails(acc, q));
        if (page is not null) Show(FormatPage(page));
    }

    private async Task Unread()
    {
        string acc = _account.Text;
        var list = await WithEngine("unread_counts", e => e.UnreadCounts(acc));
        if (list is not null)
            Show(list.Count == 0 ? "unread_counts.count = 0" :
                string.Join(Environment.NewLine, list.Select(u => $"{u.MailboxId} | unread={u.Unread}")));
    }

    private async Task Search()
    {
        string acc = _account.Text, kw = _keyword.Text;
        var page = await WithEngine("search", e => e.Search(acc, kw, SearchScopeDto.All, 20U, 0U));
        if (page is not null) Show(FormatPage(page));
    }

    private async Task SetRead()
    {
        string id = _emailId.Text;
        var mail = await WithEngine("set_read", e => e.SetRead(id, true));
        if (mail is not null) Show("set_read -> " + mail.Id + " | " + mail.Subject);
    }

    private async Task Star()
    {
        string id = _emailId.Text;
        var mail = await WithEngine("toggle_star", e => e.ToggleStar(id, true));
        if (mail is not null) Show("toggle_star -> " + mail.Id + " | " + mail.Subject);
    }

    private async Task Delete()
    {
        string id = _emailId.Text;
        var r = await WithEngine("delete_email", e => e.DeleteEmail(id));
        if (r is not null) Show("delete_email -> " + r.EmailId + " trashed=" + r.Trashed);
    }

    private static string FormatPage(EmailPageDto p) =>
        $"total={p.Total} next_offset={p.NextOffset} items={p.Items.Count}" + Environment.NewLine +
        string.Join(Environment.NewLine, p.Items.Select(m => $"{m.Id} | {m.Subject} | {m.ReceivedAt:o}"));
}

public sealed class ReaderPage : CorePage
{
    private readonly TextBox _emailId = Field("邮件 ID", "");
    private readonly CheckBox _preferHtml = new() { Content = "优先 HTML", IsChecked = true };

    public ReaderPage() : base("阅读", "S5-05")
    {
        Root.Children.Add(Row(_emailId, _preferHtml));
        Root.Children.Add(Row(
            Action("读取邮件 (get_email)", Get),
            Action("读正文 (read_body)", Body),
            Action("渲染正文 (render_body)", Render)));
        Root.Children.Add(Row(
            Action("发件人信任 (sender_trust)", Trust),
            Action("允许远程图片 (allow_sender_remote_images)", Allow)));
    }

    private async Task Get()
    {
        string id = _emailId.Text;
        var m = await WithEngine("get_email", e => e.GetEmail(id));
        if (m is null) return;
        Show(string.Join(Environment.NewLine,
            "id=" + m.Id, "subject=" + m.Subject, "from=" + string.Join(",", m.From.Select(a => a.Email)),
            "received_at=" + m.ReceivedAt.ToString("o"), "preview=" + m.Preview,
            "attachments=" + m.Attachments.Count, "flags=" + m.Flags.Count));
    }

    private async Task Body()
    {
        string id = _emailId.Text;
        bool html = _preferHtml.IsChecked == true;
        var s = await WithEngine("read_body", e => e.ReadBody(id, html));
        Show("read_body = " + (s ?? "(null)"));
    }

    private async Task Render()
    {
        string id = _emailId.Text;
        bool html = _preferHtml.IsChecked == true;
        var r = await WithEngine("render_body", e => e.RenderBody(id, html, false));
        if (r is null) return;
        Show(string.Join(Environment.NewLine,
            "remote_images_allowed=" + r.RemoteImagesAllowed, "blocked_count=" + r.BlockedCount,
            "removed_tag_count=" + r.RemovedTagCount, "truncated=" + r.Truncated,
            "trust_level=" + r.TrustLevel, "sender_domain=" + r.SenderDomain,
            "text=" + (r.Text ?? "(null)")));
    }

    private async Task Trust()
    {
        string id = _emailId.Text;
        var t = await WithEngine("sender_trust", e => e.SenderTrust(id));
        if (t is not null) Show($"address={t.Address}\ndomain={t.Domain}\nlevel={t.Level}\nmessage={t.Message}");
    }

    private async Task Allow()
    {
        string id = _emailId.Text;
        var s = await WithEngine("allow_sender_remote_images", e => e.AllowSenderRemoteImages(id, "sender"));
        Show("allow_sender_remote_images = " + s);
    }
}

public sealed class ComposePage : CorePage
{
    private readonly TextBox _account = Field("账号 ID", "");
    private readonly TextBox _to = Field("收件人", "to@example.com");
    private readonly TextBox _subject = Field("主题", "");
    private readonly TextBox _body = Field("正文", "");
    private readonly TextBox _draftId = Field("草稿 ID（标记已发送用）", "");

    public ComposePage() : base("撰写", "S5-06")
    {
        _body.AcceptsReturn = true;
        _body.Height = 120;
        Root.Children.Add(Row(_account, _to));
        Root.Children.Add(_subject);
        Root.Children.Add(_body);
        Root.Children.Add(_draftId);
        Root.Children.Add(Row(
            Action("保存草稿 (save_draft)", Save),
            Action("草稿列表 (list_drafts)", Drafts),
            Action("标记已发送 (mark_sent)", Sent),
            Action("确保系统邮箱 (ensure_system_mailboxes)", Ensure)));
    }

    private async Task Save()
    {
        string acc = _account.Text, to = _to.Text, subject = _subject.Text, body = _body.Text;
        var draft = new DraftDto
        {
            To = new List<AddressDto> { new AddressDto { Email = to, Name = null } },
            Cc = Array.Empty<AddressDto>(),
            Bcc = Array.Empty<AddressDto>(),
            Subject = subject,
            TextBody = body,
            HtmlBody = null,
            InReplyTo = null,
            References = Array.Empty<string>(),
        };
        var m = await WithEngine("save_draft", e => e.SaveDraft(acc, draft));
        if (m is null) return;
        _draftId.Text = m.Id;
        Show("save_draft -> " + m.Id + " | " + m.Subject);
    }

    private async Task Drafts()
    {
        string acc = _account.Text;
        var list = await WithEngine("list_drafts", e => e.ListDrafts(acc, 20U));
        if (list is not null)
            Show(list.Count == 0 ? "drafts.count = 0" :
                string.Join(Environment.NewLine, list.Select(m => m.Id + " | " + m.Subject)));
    }

    private async Task Sent()
    {
        string id = _draftId.Text;
        var m = await WithEngine("mark_sent", e => e.MarkSent(id, null));
        if (m is not null) Show("mark_sent -> " + m.Id + " | " + m.Subject);
    }

    private async Task Ensure()
    {
        string acc = _account.Text;
        var list = await WithEngine("ensure_system_mailboxes", e => e.EnsureSystemMailboxes(acc));
        if (list is not null)
            Show(string.Join(Environment.NewLine, list.Select(m => m.Id + " | " + m.Name + " | " + m.Role)));
    }
}
