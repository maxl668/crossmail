import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: {
    target: "chrome110",
    // v1.5：emptyOutDir=false——沙箱安全删除 shim 对 dist 清空偶发锁超时导致构建失败；
    // 资源文件名带内容 hash，旧 hash 资源残留无害（index.html 只引用新 hash）
    emptyOutDir: false,
  },
});
