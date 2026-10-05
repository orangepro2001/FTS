import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// 输出目录用 webdist(不用 dist):仓库根已有 dist\(发布产物),避免混淆
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: "webdist", emptyOutDir: true, target: "chrome105" },
});
