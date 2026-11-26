using System;
using CrossMail.Bindings;
using Microsoft.UI.Dispatching;

namespace CrossMail.App.Services;

// 把 Rust 推送事件映射到 UI 状态（未读角标）。Rust 回调线程不能直接碰控件，
// 因此 OnEvent 只做线程切换；真正的状态写入集中在 Apply。
// 注意：本类不订阅、也不产生事件。subscribe_push 需要真实账号，且回调 vtable
// 的生命周期尚未在无人值守环境验证，本版只打通「事件 -> 角标」链路。
public sealed class PushBadgeService : IPushObserver
{
    private readonly DispatcherQueue _queue;

    public PushBadgeService(DispatcherQueue queue) => _queue = queue;

    public void OnEvent(PushEventDto evt)
    {
        if (_queue is null) { Apply(evt); return; }
        _queue.TryEnqueue(() => Apply(evt));
    }

    public void Apply(PushEventDto evt)
    {
        AppState.Current.UnreadTotal = evt.UnreadTotal;
        AppState.Current.LastPushSummary = DateTime.Now.ToString("HH:mm:ss") +
            " kind=" + evt.Kind + " reason=" + evt.Reason +
            " inserted=" + evt.Inserted + " unread_total=" + evt.UnreadTotal +
            (string.IsNullOrEmpty(evt.Error) ? "" : " error=" + evt.Error);
        AppState.Current.Notify();
    }
}
