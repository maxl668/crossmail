//! 新邮件托盘气泡提醒（v1.7）
//!
//! 绿色便携版无 package identity，系统 Toast 需 AUMID + COM activator（不可用），
//! 因此用 Win32 Shell_NotifyIconW 在独立 uID 上弹气泡：
//!   - 不干扰 Tauri 现有 tray（独立 uID），气泡生命周期结束后自动 NIM_DELETE。
//!   - 复用主窗口 HWND，不新建隐藏窗口。
//!   - 所有调用集中在 notify_mail，失败静默（提醒不能影响收信主链路）。

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

type Hwnd = *mut c_void;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_INFO, NIF_ICON, NIF_MESSAGE, NIF_TIP,
    NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
};

// 独立于 Tauri tray 的 uID；0xC0FFEE "COFFEE"，不与 tray-icon crate 的内部 COUNTER 冲突
const BUBBLE_UID: u32 = 0xC0FFEE;
const WM_BUBBLE: u32 = 0x0800 + 96;
// 每次气泡带单调递增 token，避免多账号并发推送时互相覆盖
static BUBBLE_SEQ: AtomicU32 = AtomicU32::new(0);

fn to_wide(s: &str, buf: &mut [u16]) {
    let wide: Vec<u16> = OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect();
    let n = wide.len().min(buf.len());
    buf[..n].copy_from_slice(&wide[..n]);
    if n == buf.len() {
        buf[n - 1] = 0;
    }
}

fn build_nid(hwnd: Hwnd) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = BUBBLE_UID;
    nid.uCallbackMessage = WM_BUBBLE;
    nid
}

/// 弹出新邮件托盘气泡。title 建议 "CrossMail · 新邮件"，body 含账号/数量/未读。
/// 调用方可在任何线程；内部通过 Shell_NotifyIconW 与主线程窗口通信，无需额外消息循环。
pub fn notify_mail(hwnd: Hwnd, title: &str, body: &str) {
    if hwnd.is_null() { return; }

    let seq = BUBBLE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut nid = build_nid(hwnd);

    // 1) 注册独立图标（若已存在则 NIM_ADD 失败也无妨，后续 MODIFY 仍能命中）
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    to_wide("CrossMail 新邮件提醒", &mut nid.szTip);
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &nid);
    }

    // 2) 弹气泡（szInfoTitle ≤64 / szInfo ≤256，to_wide 已做截断保护）
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    to_wide(title, &mut nid.szInfoTitle);
    to_wide(body, &mut nid.szInfo);
    nid.Anonymous.uTimeout = 15_000; // Windows 实际按系统最小/最大值裁剪
    let ok = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };

    // 3) 延迟清理独立图标，避免托盘长期残留两个图标
    if ok != 0 {
        let hwnd_bits = hwnd as usize; // 裸指针不 Send，转 usize 跨线程
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(20));
            // 只有仍是最后一条气泡才清理，避免清掉更新的一封
            if BUBBLE_SEQ.load(Ordering::Relaxed) == seq + 1 {
                let cleanup = build_nid(hwnd_bits as Hwnd);
                unsafe { Shell_NotifyIconW(NIM_DELETE, &cleanup); }
            }
        });
    }
}

/// 程序退出时清理气泡图标（可选调用）
pub fn cleanup(hwnd: Hwnd) {
    if hwnd.is_null() { return; }
    let nid = build_nid(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid); }
}
