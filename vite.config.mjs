import { defineConfig } from "vite";

export default defineConfig({
  server: {
    watch: {
      // The Rust build output churns constantly while `tauri dev` compiles, and
      // on Windows watching a file cargo currently holds throws EBUSY. Chokidar
      // raises that as an unhandled error, which takes the dev server — and so
      // `beforeDevCommand` — down with it. None of these feed the frontend.
      ignored: ["**/target/**", "**/src-tauri/**", "**/crates/**", "**/.git/**"],
    },
  },
  build: {
    rollupOptions: {
      input: {
        main: "index.html",
        hostSwitcher: "host-switcher.html",
        switchNotice: "switch-notice.html",
      },
    },
  },
});
