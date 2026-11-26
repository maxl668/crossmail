using System.Threading.Tasks;

namespace CrossMail.App.Services;

// Shell 顶部命令栏只负责发命令，具体行为由当前工作区实现。
// 未实现该接口的模块（如占位页）在 Shell 里自动禁用命令，避免出现假按钮。
public interface IWorkspaceCommands
{
    Task SearchAsync(string keyword);
    void ToggleCompose();
    void OpenComposeWindow();
    Task SyncAsync();
    Task RefreshAsync();
}
