using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using CrossMail.App.Services;
using CrossMail.Bindings;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

// 参考 Bulwark Webmail /mail 的三栏工作区：文件夹 | 列表 | 阅读。
// 所有数据来自 Rust 引擎真调用；无账号/无邮箱/空文件夹/异常均显示真实原因，绝不填假数据。
public sealed class MailWorkspacePage : Page, IWorkspaceCommands
{
    private const string AllMail = "__all__";
    private const string UnreadOnly = "__unread__";

    private readonly Grid _columns = new();
    private readonly ListView _accounts = new();
    private readonly ListView _folders = new();
    private readonly ListView _messages = new();
    private readonly TextBlock _readerSubject = new() { FontSize = 20, TextWrapping = TextWrapping.Wrap };
    private readonly TextBlock _readerMeta = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.75 };
    private readonly TextBlock _readerBody = new() { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
    private readonly TextBlock _readerAttachments = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.75 };
    private readonly TextBlock _readerTrust = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.75 };
    private readonly TextBlock _hint = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.8 };
    private readonly TextBlock _folderHeader = new() { Text = "文件夹", Opacity = 0.65 };
    private readonly TextBlock _listHeader = new() { Text = "邮件", Opacity = 0.65 };

    private readonly StackPanel _compose = new() { Spacing = 8, Visibility = Visibility.Collapsed };
    private readonly TextBox _to = new() { Header = "收件人", PlaceholderText = "name@example.com" };
    private readonly TextBox _subject = new() { Header = "主题" };
    private readonly TextBox _body = new() { Header = "正文", AcceptsReturn = true, Height = 110, TextWrapping = TextWrapping.Wrap };
    private readonly TextBlock _composeResult = new() { TextWrapping = TextWrapping.Wrap };

    private string? _draftId;
    private EmailDto? _current;
    private bool _loading;
    // 首次阅读会在后台线程真连 IMAP 回源取正文（ensure_body）。快速连点邮件时，
    // 只有最后一次选择的结果允许写回阅读窗格，避免旧请求覆盖新选择。
    private int _openSeq;
    private readonly List<ComposeWindow> _composeWindows = new();
    // v1.7：当前账号的推送订阅（切换账号时先取消旧的）
    private PushSubscription? _pushSubscription;
    private string? _pushAccountId;

    public int OpenComposeCount => _composeWindows.Count;
    public void CloseComposeWindows()
    {
        foreach (var w in _composeWindows) w.CloseSafely();
        _composeWindows.Clear();
    }

    public int ColumnCount => _columns.ColumnDefinitions.Count;
    public int AccountItems => _accounts.Items.Count;
    public int FolderItems => _folders.Items.Count;
    public int MessageItems => _messages.Items.Count;
    public string Hint => _hint.Text;

    public MailWorkspacePage()
    {
        BuildUi();
        _ = Safe("初始化", RefreshAsync);
    }

    private void BuildUi()
    {
        var shell = new Grid { Padding = new Thickness(16, 12, 16, 8) };
        shell.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        shell.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        shell.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });

        var title = new TextBlock { Text = "邮件", FontSize = 22 };
        Grid.SetRow(title, 0);
        shell.Children.Add(title);

        _columns.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(230) });
        _columns.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1.4, GridUnitType.Star) });
        _columns.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1.8, GridUnitType.Star) });
        Grid.SetRow(_columns, 1);
        shell.Children.Add(_columns);

        _columns.Children.Add(BuildFolderColumn());
        _columns.Children.Add(BuildListColumn());
        _columns.Children.Add(BuildReaderColumn());

        Grid.SetRow(_hint, 2);
        shell.Children.Add(_hint);
        Content = shell;

        _accounts.SelectionChanged += OnAccountChanged;
        _folders.SelectionChanged += OnFolderChanged;
        _messages.SelectionChanged += OnMessageChanged;
    }

    private UIElement BuildFolderColumn()
    {
        var panel = new StackPanel { Margin = new Thickness(0, 8, 12, 0), Spacing = 8 };
        panel.Children.Add(new TextBlock { Text = "账号", Opacity = 0.65 });
        _accounts.MaxHeight = 116;
        panel.Children.Add(_accounts);
        panel.Children.Add(_folderHeader);
        panel.Children.Add(_folders);
        Grid.SetColumn(panel, 0);
        return panel;
    }

    private UIElement BuildListColumn()
    {
        var panel = new Grid { Margin = new Thickness(12, 8, 12, 0) };
        panel.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        panel.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        Grid.SetRow(_listHeader, 0);
        panel.Children.Add(_listHeader);
        Grid.SetRow(_messages, 1);
        panel.Children.Add(_messages);
        Grid.SetColumn(panel, 1);
        return panel;
    }

    private UIElement BuildReaderColumn()
    {
        var root = new ScrollViewer { Margin = new Thickness(12, 8, 0, 0) };
        var panel = new StackPanel { Spacing = 10 };
        panel.Children.Add(_readerSubject);
        panel.Children.Add(_readerMeta);

        var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        foreach (var (label, handler) in new (string, RoutedEventHandler)[]
                 {
                     ("标记已读", (_, _) => _ = Safe("set_read", () => ApplyAsync(e => MailService.SetReadAsync(e, true)))),
                     ("星标", (_, _) => _ = Safe("toggle_star", () => ApplyAsync(e => MailService.StarAsync(e, true)))),
                     ("归档", (_, _) => _ = Safe("archive_email", () => ApplyAsync(e => MailService.ArchiveAsync(e)))),
                     ("删除", (_, _) => _ = Safe("delete_email", () => ApplyAsync(e => MailService.DeleteAsync(e)))),
                     ("回复", (_, _) => StartCompose(true)),
                     ("转发", (_, _) => StartCompose(false)),
                 })
        {
            var b = new Button { Content = label };
            b.Click += handler;
            actions.Children.Add(b);
        }
        panel.Children.Add(actions);

        panel.Children.Add(_readerAttachments);
        panel.Children.Add(_readerTrust);
        panel.Children.Add(_readerBody);
        panel.Children.Add(BuildComposePanel());
        root.Content = panel;
        Grid.SetColumn(root, 2);
        return root;
    }

    private UIElement BuildComposePanel()
    {
        _compose.Children.Add(new TextBlock { Text = "撰写 / 草稿", FontSize = 16 });
        _compose.Children.Add(_to);
        _compose.Children.Add(_subject);
        _compose.Children.Add(_body);
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var save = new Button { Content = "保存草稿" };
        save.Click += (_, _) => _ = Safe("save_draft", SaveDraftAsync);
        var drafts = new Button { Content = "草稿列表" };
        drafts.Click += (_, _) => _ = Safe("list_drafts", ListDraftsAsync);
        var close = new Button { Content = "收起" };
        close.Click += (_, _) => ToggleCompose();
        row.Children.Add(save);
        row.Children.Add(drafts);
        row.Children.Add(close);
        _compose.Children.Add(row);
        _compose.Children.Add(_composeResult);
        return _compose;
    }

    private void OnAccountChanged(object sender, SelectionChangedEventArgs args)
    {
        if (_loading || args.AddedItems.Count == 0) return;
        if (args.AddedItems[0] is ListViewItem item && item.Tag is string id)
        {
            AppState.Current.AccountId = id;
            AppState.Current.ResetSelection();
            _ = Safe("加载文件夹", () => LoadFoldersAsync(id));
            _ = SubscribePushAsync(id);
        }
    }

    // v1.7：选账号后自动订阅推送，新邮件触发双态提醒。
    // PushSubscription 持有 Rust 侧独立 handle（PushPage 已验证），C# engine 释放不影响回调。
    private async Task SubscribePushAsync(string accountId)
    {
        if (_pushAccountId == accountId && _pushSubscription is not null) return;
        try
        {
            await CoreBridge.RunAsync(() =>
            {
                _pushSubscription?.Cancel();
                _pushSubscription = null;
                using var engine = CrossMailCore.OpenEngine(CoreBridge.CreateConfig());
                var observer = new MailPushObserver(
                    Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread(), OnMailPush);
                _pushSubscription = engine.SubscribePush(accountId, observer);
                return true;
            });
            _pushAccountId = accountId;
        }
        catch (Exception ex)
        {
            SetStatus("推送订阅失败（真实错误）：" + ex.Message);
        }
    }

    private void OnMailPush(PushEventDto evt)
    {
        // 角标始终更新
        AppState.Current.UnreadTotal = evt.UnreadTotal;
        AppState.Current.LastPushSummary = DateTime.Now.ToString("HH:mm:ss") +
            " kind=" + evt.Kind + " inserted=" + evt.Inserted + " unread_total=" + evt.UnreadTotal;
        AppState.Current.Notify();
        if (evt.Inserted > 0)
        {
            MailNotifier.NotifyNewMail((App.Current as App)?.MainWindow, evt.Inserted, evt.UnreadTotal);
            AppState.Current.MailAlert = $"✉️ 收到 {evt.Inserted} 封新邮件 · 未读共 {evt.UnreadTotal} 封";
            AppState.Current.Notify();
            _ = ClearMailAlertAfterDelay();
        }
    }

    private async Task ClearMailAlertAfterDelay()
    {
        await Task.Delay(8000);
        AppState.Current.MailAlert = null;
        AppState.Current.Notify();
    }

    private sealed class MailPushObserver : IPushObserver
    {
        private readonly Microsoft.UI.Dispatching.DispatcherQueue _queue;
        private readonly Action<PushEventDto> _onEvent;
        public MailPushObserver(Microsoft.UI.Dispatching.DispatcherQueue queue, Action<PushEventDto> onEvent)
        { _queue = queue; _onEvent = onEvent; }
        public void OnEvent(PushEventDto evt) => _queue.TryEnqueue(() => _onEvent(evt));
    }

    private void OnFolderChanged(object sender, SelectionChangedEventArgs args)
    {
        if (_loading || args.AddedItems.Count == 0) return;
        if (args.AddedItems[0] is ListViewItem item && item.Tag is string id)
        {
            AppState.Current.MailboxId = id == AllMail || id == UnreadOnly ? null : id;
            _ = Safe("加载邮件", LoadMessagesAsync);
        }
    }

    private void OnMessageChanged(object sender, SelectionChangedEventArgs args)
    {
        if (_loading || args.AddedItems.Count == 0) return;
        if (args.AddedItems[0] is ListViewItem item && item.Tag is string id)
        {
            AppState.Current.EmailId = id;
            int seq = ++_openSeq;
            // 回源可能要等一次网络往返，先给可见反馈；不占用 UI 线程（DetailAsync 走 Task.Run）。
            _readerBody.Text = "正在获取正文…（首次阅读需回源从服务器拉取，不改动邮件已读状态）";
            SetStatus("正在打开邮件 " + id + " · 后台回源中");
            _ = Safe("读取邮件", () => OpenEmailAsync(id, seq));
        }
    }

    public async Task RefreshAsync()
    {
        _loading = true;
        _accounts.Items.Clear();
        try
        {
            var accounts = await MailService.AccountsAsync();
            foreach (var a in accounts)
                _accounts.Items.Add(new ListViewItem
                {
                    Tag = a.Id,
                    Content = new TextBlock { Text = a.DisplayName ?? a.EmailAddress, TextWrapping = TextWrapping.Wrap },
                });
            if (accounts.Count == 0)
            {
                ClearWorkspace("本机尚无账号（list_accounts.count = 0）。请先在「账号」页创建账号。");
                return;
            }
            AppState.Current.AccountId = accounts[0].Id;
            _accounts.SelectedIndex = 0;
        }
        finally { _loading = false; }

        var snapshot = await CoreBridge.GetSnapshotAsync();
        AppState.Current.Snapshot = snapshot;
        AppState.Current.Notify();
        await LoadFoldersAsync(AppState.Current.AccountId!);
    }

    private async Task LoadFoldersAsync(string accountId)
    {
        _loading = true;
        _folders.Items.Clear();
        try
        {
            var boxes = await MailService.MailboxesAsync(accountId);
            IReadOnlyList<MailboxCountDto> counts;
            try { counts = await MailService.UnreadAsync(accountId); }
            catch { counts = Array.Empty<MailboxCountDto>(); }
            var byId = counts.ToDictionary(c => c.MailboxId, c => c.Unread);

            _folders.Items.Add(FolderItem("所有邮件", AllMail, null));
            _folders.Items.Add(FolderItem("未读", UnreadOnly, null));
            foreach (var m in boxes)
                _folders.Items.Add(FolderItem(m.Name + "  (" + Role(m.Role) + ")", m.Id,
                    byId.TryGetValue(m.Id, out ulong unread) ? unread : m.Unread));
            _folderHeader.Text = "文件夹  " + boxes.Count + " 个";
            _folders.SelectedIndex = 0;
        }
        finally { _loading = false; }

        AppState.Current.MailboxId = null;
        await LoadMessagesAsync();
    }

    private static ListViewItem FolderItem(string label, string tag, ulong? unread)
    {
        string text = unread is > 0 ? label + "   ● " + unread : label;
        return new ListViewItem { Tag = tag, Content = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap } };
    }

    private static string Role(MailboxRoleDto role) => role switch
    {
        MailboxRoleDto.Inbox => "收件箱",
        MailboxRoleDto.Sent => "已发送",
        MailboxRoleDto.Drafts => "草稿",
        MailboxRoleDto.Archive => "归档",
        MailboxRoleDto.Junk => "垃圾邮件",
        MailboxRoleDto.Trash => "已删除",
        _ => "文件夹",
    };

    private async Task LoadMessagesAsync()
    {
        string? accountId = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(accountId))
        {
            ClearWorkspace("未选择账号，无法查询邮件。");
            return;
        }

        _loading = true;
        _messages.Items.Clear();
        try
        {
            string query = AppState.Current.Query;
            string folderTag = _folders.SelectedItem is ListViewItem fi ? fi.Tag as string ?? AllMail : AllMail;
            EmailPageDto page;
            if (!string.IsNullOrWhiteSpace(query))
            {
                page = await MailService.SearchAsync(accountId, query, 50U);
                _listHeader.Text = "搜索结果  \"" + query + "\"   " + page.Total + " 条";
            }
            else
            {
                var q = new EmailQueryDto
                {
                    MailboxId = AppState.Current.MailboxId,
                    UnreadOnly = folderTag == UnreadOnly,
                    Limit = 50U,
                    Offset = 0U,
                };
                page = await MailService.EmailsAsync(accountId, q);
                _listHeader.Text = "邮件   " + page.Total + " 封";
            }

            foreach (var m in page.Items)
                _messages.Items.Add(MessageItem(m));
            if (page.Items.Count == 0)
                _messages.Items.Add(new ListViewItem
                {
                    Content = new TextBlock
                    {
                        Text = "该视图下没有邮件（total = " + page.Total + "）。同步账号后可刷新。",
                        TextWrapping = TextWrapping.Wrap,
                        Opacity = 0.7,
                    },
                });
            SetStatus("list_emails / search · OK   共 " + page.Total + " 条");
        }
        finally { _loading = false; }
    }

    private static ListViewItem MessageItem(EmailDto m)
    {
        string from = m.From.Count > 0 ? (m.From[0].Name ?? m.From[0].Email) : "(未知发件人)";
        // 后端 FFI 时间戳为 UTC（DateTimeOffset offset=0），展示必须转本地时区。
        string date = m.SentAt?.ToLocalTime().ToString("MM-dd HH:mm")
                      ?? m.ReceivedAt.ToLocalTime().ToString("MM-dd HH:mm");
        var panel = new StackPanel { Spacing = 2, Margin = new Thickness(0, 4, 0, 4) };
        panel.Children.Add(new TextBlock { Text = from + "   " + date, Opacity = 0.75 });
        panel.Children.Add(new TextBlock
        {
            Text = (IsUnread(m) ? "● " : string.Empty) + (IsStarred(m) ? "★ " : string.Empty) + m.Subject,
            TextWrapping = TextWrapping.Wrap,
        });
        panel.Children.Add(new TextBlock { Text = m.Preview, Opacity = 0.65, TextWrapping = TextWrapping.Wrap });
        if (m.Attachments.Count > 0)
            panel.Children.Add(new TextBlock { Text = "附件 " + m.Attachments.Count + " 个", Opacity = 0.6 });
        return new ListViewItem { Tag = m.Id, Content = panel };
    }

    private static bool IsUnread(EmailDto m) => !m.Flags.Any(f => f is FlagDto.Seen);
    private static bool IsStarred(EmailDto m) => m.Flags.Any(f => f is FlagDto.Flagged);

    private async Task OpenEmailAsync(string emailId, int seq)
    {
        var detail = await MailService.DetailAsync(emailId);
        if (seq != _openSeq) return; // 用户已切到另一封 / 工作区已刷新，丢弃过期结果
        var m = detail.Email;
        _current = m;
        _readerSubject.Text = string.IsNullOrWhiteSpace(m.Subject) ? "(无主题)" : m.Subject;
        _readerMeta.Text = "发件人：" + string.Join(", ", m.From.Select(a => a.Name ?? a.Email)) +
                           "   收件人：" + string.Join(", ", m.To.Select(a => a.Email)) +
                           "   时间：" + (m.SentAt?.ToLocalTime().ToString("yyyy-MM-dd HH:mm")
                                          ?? m.ReceivedAt.ToLocalTime().ToString("yyyy-MM-dd HH:mm"));
        _readerAttachments.Text = m.Attachments.Count == 0
            ? "附件：无"
            : "附件：" + string.Join("、", m.Attachments.Select(a => (a.FileName ?? "(未命名)") + " (" + a.Size + " B)"));
        _readerTrust.Text = "发件人信任：" + detail.Trust.Level + " · " + detail.Trust.Message +
                            "   远程图片：" + (detail.Render.RemoteImagesAllowed ? "已允许" : "已拦截 " + detail.Render.BlockedCount + " 个");
        _readerBody.Text = detail.Body ?? detail.Render.Text ?? "(正文为空：本地缓存中没有可显示的文本内容)";
        await MailService.SetReadAsync(emailId, true);
        // 不在此处重建列表/文件夹：那会清掉用户刚做的选择。
        // 角标与列表在「刷新 / 同步 / 就地操作」时统一重建。
        SetStatus("打开邮件 " + emailId + " · 已标记已读");
    }

    private async Task ApplyAsync<T>(Func<string, Task<T>> call)
    {
        string? id = AppState.Current.EmailId;
        if (string.IsNullOrEmpty(id)) { SetStatus("未选中邮件，操作未执行。"); return; }
        await call(id);
        await LoadMessagesAsync();
        SetStatus("操作已执行并刷新列表：" + id);
    }

    private void StartCompose(bool reply)
    {
        _compose.Visibility = Visibility.Visible;
        if (_current is null) return;
        string from = _current.From.Count > 0 ? _current.From[0].Email : "";
        _to.Text = reply ? from : "";
        _subject.Text = (reply ? "Re: " : "Fwd: ") + _current.Subject;
        if (!reply) _body.Text = _readerBody.Text;
    }

    public void ToggleCompose() =>
        _compose.Visibility = _compose.Visibility == Visibility.Visible ? Visibility.Collapsed : Visibility.Visible;

    // Shell 的「撰写」打开独立窗口（对齐网页版可弹出 composer）；
    // 阅读窗格的「回复 / 转发」仍用内联面板，避免丢失上下文。
    public void OpenComposeWindow()
    {
        var w = new ComposeWindow();
        _composeWindows.Add(w);
        w.Activate();
        SetStatus("已打开独立撰写窗口 · 共 " + _composeWindows.Count + " 个");
    }

    private async Task SaveDraftAsync()
    {
        string? acc = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(acc)) { _composeResult.Text = "无账号，未保存草稿。"; return; }
        var draft = new DraftDto
        {
            To = new List<AddressDto> { new() { Email = _to.Text, Name = null } },
            Cc = Array.Empty<AddressDto>(),
            Bcc = Array.Empty<AddressDto>(),
            Subject = _subject.Text,
            TextBody = _body.Text,
            HtmlBody = null,
            InReplyTo = null,
            References = Array.Empty<string>(),
        };
        var saved = await MailService.SaveDraftAsync(acc, draft);
        _draftId = saved.Id;
        _composeResult.Text = "save_draft -> " + saved.Id + " | " + saved.Subject;
    }

    private async Task ListDraftsAsync()
    {
        string? acc = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(acc)) { _composeResult.Text = "无账号，未查询草稿。"; return; }
        var drafts = await MailService.DraftsAsync(acc);
        _composeResult.Text = drafts.Count == 0
            ? "list_drafts.count = 0"
            : "草稿 " + drafts.Count + " 封：" + string.Join(" | ", drafts.Select(d => d.Subject));
    }

    public async Task SearchAsync(string keyword)
    {
        AppState.Current.Query = keyword;
        await LoadMessagesAsync();
    }

    public async Task SyncAsync()
    {
        string? acc = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(acc)) { SetStatus("无账号，sync_account 未执行。"); return; }
        var report = await MailService.SyncAsync(acc);
        SetStatus("sync_account · inserted=" + report.Inserted + " updated=" + report.Updated +
                  " deleted=" + report.Deleted + " unchanged=" + report.Unchanged);
        await LoadMessagesAsync();
    }

    private void ClearWorkspace(string reason)
    {
        _openSeq++; // 让仍在后台回源的旧阅读请求失效
        _folders.Items.Clear();
        _messages.Items.Clear();
        _readerSubject.Text = "暂无内容";
        _readerMeta.Text = "";
        _readerBody.Text = reason;
        _readerAttachments.Text = "";
        _readerTrust.Text = "";
        _listHeader.Text = "邮件";
        SetStatus(reason);
    }

    private void SetStatus(string text) => _hint.Text = text;

    private async Task Safe(string label, Func<Task> action)
    {
        try { await action(); }
        catch (Exception ex)
        {
            try { SetStatus(label + " 失败（真实错误）：" + ex); }
            catch { /* window may already be closing during smoke */ }
        }
    }
}
