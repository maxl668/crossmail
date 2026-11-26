using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using CrossMail.App.Services;
using CrossMail.Bindings;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

public sealed class PushPage : CorePage
{
    private readonly TextBox _account = Field("账号 ID", "");
    private readonly List<string> _events = new();
    private PushSubscription? _subscription;
    private readonly DispatcherQueue _queue;

    public PushPage() : base("通知与后台同步", "S5-09")
    {
        _queue = DispatcherQueue.GetForCurrentThread();
        Root.Children.Add(Row(_account));
        Root.Children.Add(Row(
            Action("订阅推送 (subscribe_push)", Subscribe),
            Action("取消订阅 (cancel)", Cancel),
            Action("是否已取消 (is_cancelled)", IsCancelled),
            Action("同步账号 (sync_account)", Sync)));
        Show("推送事件：尚无。订阅后事件由 Rust 回调 vtable 送回 UI 线程显示。");
    }

    private async Task Subscribe()
    {
        string acc = _account.Text;
        var observer = new UiPushObserver(_queue, Append);
        // 无账号时 Rust 会返回真实错误，页面原样显示，不伪造事件。
        var cancelled = await WithEngine("subscribe_push", e =>
        {
            _subscription = e.SubscribePush(acc, observer);
            return _subscription.IsCancelled();
        });
        Show("subscribe_push -> is_cancelled=" + cancelled + Environment.NewLine + string.Join(Environment.NewLine, _events));
    }

    private async Task Cancel()
    {
        var sub = _subscription;
        if (sub is null) { Show("尚未订阅，无可取消对象。"); return; }
        await Guard("cancel", () => { sub.Cancel(); return true; });
        Show("cancel 已调用。" + Environment.NewLine + string.Join(Environment.NewLine, _events));
    }

    private async Task IsCancelled()
    {
        var sub = _subscription;
        if (sub is null) { Show("尚未订阅。"); return; }
        var b = await Guard("is_cancelled", () => sub.IsCancelled());
        Show("is_cancelled = " + b);
    }

    private async Task Sync()
    {
        string acc = _account.Text;
        var r = await WithEngine("sync_account", e => e.SyncAccount(acc));
        if (r is not null)
            Show($"inserted={r.Inserted} updated={r.Updated} deleted={r.Deleted} unchanged={r.Unchanged}" +
                 $"\nhas_more={r.HasMore} full_resync={r.FullResync}");
    }

    private void Append(PushEventDto evt)
    {
        _events.Add($"{DateTime.Now:HH:mm:ss} kind={evt.Kind} reason={evt.Reason} " +
                    $"inserted={evt.Inserted} unread_total={evt.UnreadTotal} error={evt.Error}");
        Show(string.Join(Environment.NewLine, _events));

        // v1.7：新邮件双态提醒——最小化时托盘气泡，浏览中应用内醒目提示。
        // PushBadgeService 统一维护未读角标；新邮件时进一步触发提醒。
        if (evt.Inserted > 0)
        {
            var window = (App.Current as App)?.MainWindow;
            MailNotifier.NotifyNewMail(window, evt.Inserted, evt.UnreadTotal);
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

    private sealed class UiPushObserver : IPushObserver
    {
        private readonly DispatcherQueue _queue;
        private readonly Action<PushEventDto> _onEvent;

        public UiPushObserver(DispatcherQueue queue, Action<PushEventDto> onEvent)
        {
            _queue = queue;
            _onEvent = onEvent;
        }

        // Rust 侧回调线程，必须切回 UI 线程再更新控件。
        public void OnEvent(PushEventDto evt) => _queue.TryEnqueue(() => _onEvent(evt));
    }
}
