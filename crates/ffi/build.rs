//! 从 `src/crossmail_ffi.udl` 生成 uniffi scaffolding（S5-01）。
//!
//! 两个工程决策记在这里，避免后人再踩：
//!
//! 1. **走 UDL 单一真源，不用 proc-macro**。本机 uniffi 0.28.3 的 library-mode
//!    （`generate --library`）静默返回成功但不产出任何绑定文件。若继续用
//!    「proc-macro 生成符号 + UDL 生成绑定」的双份定义，两边签名一旦漂移，
//!    绑定侧会去调不存在的符号：编译期不报错，运行期才崩。所以这里让 UDL
//!    成为唯一契约，Rust 侧只写实现。
//!
//! 2. **直接调 `uniffi_bindgen` 而不是 `uniffi_build`**。`uniffi_build` 只是
//!    一层薄封装，但本环境沙箱禁止 `utime`，cargo 解压任何未缓存的新 crate 都会
//!    以非 0 退出而判定失败；`uniffi_bindgen` 已随 bindgen 功能解压可用，故自己
//!    调底层 API，绕开对 `uniffi_build` 的依赖。
fn main() {
    println!("cargo:rerun-if-changed=src/crossmail_ffi.udl");
    // 注意：`out_dir_override` 必须显式传 OUT_DIR。uniffi_bindgen 在 None 时
    // 会退回「UDL 文件所在目录」，产物就落到 src/ 下，`include_scaffolding!`
    // 去 OUT_DIR 找必然落空。这个坑不写下来下次还会踩。
    let out_dir = std::env::var("OUT_DIR").expect("缺少 OUT_DIR");
    // crate 名用带下划线的 lib 名，与 `include_scaffolding!("crossmail_ffi")` 对应，
    // include 时会去找 `$OUT_DIR/crossmail_ffi.uniffi.rs`。
    uniffi_bindgen::generate_component_scaffolding_for_crate(
        camino::Utf8Path::new("src/crossmail_ffi.udl"),
        "crossmail_ffi",
        Some(camino::Utf8Path::new(&out_dir)),
        false,
    )
    .expect("生成 uniffi scaffolding 失败");
}
