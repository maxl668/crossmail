using System;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using CrossMail.App.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;

namespace CrossMail.App;

public static class Program
{
    // --smoke        不进入 XAML/WinUI runtime，只做 P/Invoke 真调用，便于无交互桌面下验证 DLL 链路。
    // --ui-smoke     真启动 WinUI Application，实例化八个页面并逐个执行按钮 handler。
    // --window-smoke 真创建主窗口、加载 XAML、完成导航后自动退出，用于验证未打包分发形态。
    [STAThread]
    public static int Main(string[] args)
    {
        bool smoke = args.Any(a => string.Equals(a, "--smoke", StringComparison.OrdinalIgnoreCase));
        bool uiSmoke = args.Any(a => string.Equals(a, "--ui-smoke", StringComparison.OrdinalIgnoreCase));
        bool windowSmoke = args.Any(a => string.Equals(a, "--window-smoke", StringComparison.OrdinalIgnoreCase));

        // 所有验收冒烟都切独立数据根：它们真调写操作，不能再碰用户真实账号库。
        // 必须在任何 CoreBridge / engine 调用之前设置，且三个 Runner 共用同一目录，
        // 这样 step4 与 step8 的主密钥跨进程指纹断言仍然成立。
        if (smoke || uiSmoke || windowSmoke)
        {
            CoreBridge.EnableIsolation();
            Console.WriteLine("[DATA-ROOT] isolated=" + CoreBridge.UseIsolatedDataRoot +
                              " path=" + CoreBridge.DataRoot);
        }

        if (smoke) return SmokeRunner.Run();

        int exitCode = 0;
        // Await the real button handlers on the UI dispatcher, then exit the message loop.
        async Task RunUiSmokeAsync()
        {
            exitCode = await UiSmokeRunner.RunAsync();
            Application.Current.Exit();
        }
        Application.Start(initParams =>
        {
            var queue = DispatcherQueue.GetForCurrentThread();
            SynchronizationContext.SetSynchronizationContext(
                new DispatcherQueueSynchronizationContext(queue));
            App.UiSmokeMode = uiSmoke;
            App.WindowSmokeMode = windowSmoke;
            _ = new App();
            if (uiSmoke) _ = RunUiSmokeAsync();
        });
        if (windowSmoke) return WindowSmokeRunner.ExitCode;
        return exitCode;
    }
}
