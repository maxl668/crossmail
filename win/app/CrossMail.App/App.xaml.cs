using Microsoft.UI.Xaml;

namespace CrossMail.App;

public sealed partial class App : Application
{
    private Window? _window;

    // v1.7：供新邮件提醒判断窗口最小化状态与弹托盘气泡
    internal Window? MainWindow => _window;

    // --ui-smoke 实例化八个页面并真执行按钮 handler，不弹窗口。
    // --window-smoke 真创建主窗口、加载 XAML、完成导航后自动退出，用于验证未打包分发形态。
    public static bool UiSmokeMode { get; set; }
    public static bool WindowSmokeMode { get; set; }

    public App() => InitializeComponent();

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        if (UiSmokeMode) return;
        var window = new MainWindow();
        _window = window;
        window.Activate();
        if (WindowSmokeMode) _ = CrossMail.App.Services.WindowSmokeRunner.RunAsync(window);
    }
}
