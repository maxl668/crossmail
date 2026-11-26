using System;
using System.Collections.Generic;
using System.IO;
using System.Text.Json;
using System.Threading.Tasks;
using CrossMail.App.Services;
using CrossMail.Bindings;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App;

// 独立撰写窗口（对齐网页版 Bulwark 的可弹出 composer）。
//
// 核心限制（实查 Rust service/mail.rs::save_draft）：每次调用都用 Uuid::new_v4()
// 新建一封草稿，且没有 update_draft / delete_draft FFI。若把 debounce 自动保存
// 直接接到 save_draft，每敲几个字就会在草稿箱堆一封重复草稿。
// 因此本窗口的自动保存只写本地 JSON 文件（进程重启可恢复），
// 只有用户显式点「存入核心草稿箱」才调用 save_draft，窗口内文字明确说明这一限制。
public sealed class ComposeWindow : Window
{
    private sealed class DraftFile
    {
        public string To { get; set; } = "";
        public string Subject { get; set; } = "";
        public string Body { get; set; } = "";
        public string? CoreDraftId { get; set; }
        public string SavedAt { get; set; } = "";
    }

    private readonly TextBox _to = new() { Header = "收件人", PlaceholderText = "name@example.com" };
    private readonly TextBox _subject = new() { Header = "主题" };
    private readonly TextBox _body = new()
    {
        Header = "正文", AcceptsReturn = true, Height = 220, TextWrapping = TextWrapping.Wrap,
    };
    private readonly TextBlock _status = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.8 };
    private readonly List<Button> _actionButtons = new();
    private readonly string _autosavePath;
    private Button? _sendButton;
    private DispatcherQueueTimer? _timer;
    private string? _coreDraftId;

    internal string AutosavePath => _autosavePath;
    internal string Status => _status.Text;
    internal string? CoreDraftId => _coreDraftId;
    internal int FieldCount => 3;
    internal int ActionButtons => _actionButtons.Count;
    internal bool HasSendButton => _sendButton is not null;
    internal int AutosaveWrites { get; private set; }
    internal string ToText => _to.Text;
    internal string SubjectText => _subject.Text;
    internal string BodyText => _body.Text;

    public ComposeWindow(string? autosaveFileName = null)
    {
        _autosavePath = Path.Combine(CoreBridge.DataRoot, autosaveFileName ?? "compose-autosave.json");
        Title = "CrossMail · 撰写（独立窗口）";
        BuildUi();
        Restore();

        var queue = DispatcherQueue.GetForCurrentThread();
        if (queue is not null)
        {
            _timer = queue.CreateTimer();
            _timer.IsRepeating = false;
            _timer.Interval = TimeSpan.FromMilliseconds(800);
            _timer.Tick += (_, _) => FlushAutosave();
            _to.TextChanged += (_, _) => Debounce();
            _subject.TextChanged += (_, _) => Debounce();
            _body.TextChanged += (_, _) => Debounce();
        }
    }

    private void BuildUi()
    {
        var root = new Grid { Padding = new Thickness(20, 16, 20, 16) };
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });

        root.Children.Add(new TextBlock { Text = "撰写邮件", FontSize = 22 });

        var fields = new StackPanel { Spacing = 10 };
        Grid.SetRow(fields, 1);
        fields.Children.Add(_to);
        fields.Children.Add(_subject);
        fields.Children.Add(_body);
        fields.Children.Add(new TextBlock
        {
            Text = "自动保存写入本地草稿文件（重启可恢复，不写核心库）。" +
                   "「发送」由核心执行：先落一封草稿 → 走账号 SMTP/JMAP 提交 → 成功后草稿转「已发送」；" +
                   "发送失败时草稿保留在草稿箱，改完可直接重发。",
            Opacity = 0.7,
            TextWrapping = TextWrapping.Wrap,
        });
        root.Children.Add(fields);

        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        Grid.SetRow(row, 2);
        foreach (var (label, handler) in new (string, RoutedEventHandler)[]
                 {
                     ("立即自动保存", (_, _) => FlushAutosave()),
                     ("存入核心草稿箱", (_, _) => _ = SaveToCoreAsync()),
                     ("发送", (_, _) => _ = SendAsync()),
                     ("标记已发送", (_, _) => _ = MarkSentAsync()),
                     ("关闭", (_, _) => CloseSafely()),
                 })
        {
            var b = new Button { Content = label };
            b.Click += handler;
            if (label == "发送") _sendButton = b;
            _actionButtons.Add(b);
            row.Children.Add(b);
        }
        root.Children.Add(row);

        Grid.SetRow(_status, 3);
        root.Children.Add(_status);
        Content = root;
    }

    private void Debounce()
    {
        if (_timer is null) return;
        _timer.Stop();
        _timer.Start();
    }

    private void Restore()
    {
        try
        {
            if (!File.Exists(_autosavePath)) return;
            var d = JsonSerializer.Deserialize<DraftFile>(File.ReadAllText(_autosavePath));
            if (d is null) return;
            _to.Text = d.To;
            _subject.Text = d.Subject;
            _body.Text = d.Body;
            _coreDraftId = d.CoreDraftId;
            _status.Text = "已恢复本地草稿：" + d.SavedAt;
        }
        catch (Exception ex)
        {
            _status.Text = "本地草稿恢复失败（真实错误）：" + ex.Message;
        }
    }

    // 供自动化确定性调用；定时器路径同样落到这个函数。
    internal bool FlushAutosave()
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_autosavePath)!);
            var d = new DraftFile
            {
                To = _to.Text,
                Subject = _subject.Text,
                Body = _body.Text,
                CoreDraftId = _coreDraftId,
                SavedAt = DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss"),
            };
            File.WriteAllText(_autosavePath, JsonSerializer.Serialize(d));
            AutosaveWrites++;
            _status.Text = "本地自动保存 OK · " + _autosavePath;
            return File.Exists(_autosavePath);
        }
        catch (Exception ex)
        {
            _status.Text = "自动保存失败（真实错误）：" + ex;
            return false;
        }
    }

    internal void SetContentForSmoke(string to, string subject, string body)
    {
        _to.Text = to;
        _subject.Text = subject;
        _body.Text = body;
    }

    internal async Task<string> SaveToCoreAsync()
    {
        string? acc = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(acc))
        {
            _status.Text = "save_draft 未执行：本机尚无账号（list_accounts.count = 0）。";
            return _status.Text;
        }
        try
        {
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
            _coreDraftId = saved.Id;
            FlushAutosave();
            _status.Text = "save_draft -> " + saved.Id + "（核心会新建一封草稿，不会覆盖旧草稿）";
        }
        catch (Exception ex)
        {
            _status.Text = "save_draft 失败（真实错误）：" + ex;
        }
        return _status.Text;
    }

    // 收件人解析：支持 "a@b, c@d" / "张三 <a@b>" 两种常见输入，分隔符为逗号或分号。
    // 解析结果原样交给核心校验（核心会拒绝空/非法地址），UI 不重复实现校验规则。
    private static List<AddressDto> ParseAddresses(string raw)
    {
        var list = new List<AddressDto>();
        foreach (string part in raw.Split(new[] { ',', ';', '\n' }, StringSplitOptions.RemoveEmptyEntries))
        {
            string s = part.Trim();
            if (s.Length == 0) continue;
            string? name = null;
            int lt = s.IndexOf('<');
            int gt = s.LastIndexOf('>');
            if (lt >= 0 && gt > lt)
            {
                name = s[..lt].Trim().Trim('"');
                if (name.Length == 0) name = null;
                s = s[(lt + 1)..gt].Trim();
            }
            list.Add(new AddressDto { Email = s, Name = name });
        }
        return list;
    }

    // 真正的发信入口。核心内部顺序：落草稿 → SMTP/JMAP 提交 → 草稿转「已发送」，
    // 所以这里不需要先 SaveToCoreAsync；失败时草稿已落库，用户改完可直接重试。
    internal async Task<string> SendAsync()
    {
        // 先做纯本地校验（不碰核心）：空收件人是最常见的误点，不该先开引擎再报错。
        var to = ParseAddresses(_to.Text);
        if (to.Count == 0)
        {
            _status.Text = "发送未执行：请至少填写一个收件人地址。";
            return _status.Text;
        }
        string? acc = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(acc))
        {
            _status.Text = "发送未执行：本机尚无账号（list_accounts.count = 0）。";
            return _status.Text;
        }
        if (_sendButton is not null) _sendButton.IsEnabled = false;
        _status.Text = "正在发送…（核心：落草稿 → 提交 → 转已发送）";
        try
        {
            var draft = new DraftDto
            {
                To = to,
                Cc = Array.Empty<AddressDto>(),
                Bcc = Array.Empty<AddressDto>(),
                Subject = _subject.Text,
                TextBody = _body.Text,
                HtmlBody = null,
                InReplyTo = null,
                References = Array.Empty<string>(),
            };
            var receipt = await MailService.SendAsync(acc, draft);
            // 草稿已转正，本地自动保存里不再保留旧草稿 id（避免下次恢复后误判）。
            _coreDraftId = null;
            FlushAutosave();
            // 归档到服务端时，本地主键就是服务端身份 `账号/文件夹/UID`；APPEND 失败
            // 或服务端没有已发送文件夹时，主键仍留在草稿箱那一段。这个差异就是
            // 「手机上能不能看到这封信」的真假，必须如实显示给用户。
            bool archived = receipt.EmailId.Contains("/" + receipt.SentMailboxId + "/");
            _status.Text = (archived
                               ? "已发送 · 已归档到 " + receipt.SentMailboxId
                               : "已发送（仅本地记账：服务端已发送未归档，手机端可能看不到）") +
                           " · message_id=" + receipt.MessageId +
                           " · " + receipt.SentAt.ToLocalTime().ToString("yyyy-MM-dd HH:mm:ss") +
                           " · 本地 " + receipt.EmailId;
        }
        catch (Exception ex)
        {
            _status.Text = "发送失败（真实错误）：" + ex.Message + " ｜ 草稿已保留在草稿箱，可改完重试。";
        }
        finally
        {
            if (_sendButton is not null) _sendButton.IsEnabled = true;
        }
        return _status.Text;
    }

    internal async Task<string> MarkSentAsync()
    {
        if (string.IsNullOrEmpty(_coreDraftId))
        {
            _status.Text = "mark_sent 未执行：当前没有核心草稿 ID。";
            return _status.Text;
        }
        try
        {
            var e = await MailService.MarkSentAsync(_coreDraftId);
            _status.Text = "mark_sent -> " + e.Id;
        }
        catch (Exception ex)
        {
            _status.Text = "mark_sent 失败（真实错误）：" + ex;
        }
        return _status.Text;
    }

    internal void CloseSafely()
    {
        try { Close(); }
        catch (Exception ex) { _status.Text = "关闭窗口失败（真实错误）：" + ex.Message; }
    }
}
