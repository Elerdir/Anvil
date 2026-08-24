import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  // Tauri očekává pevný port a nesmí si ho sám přehodit — devUrl
  // v tauri.conf.json je zadrátovaná.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Rust strom je velký; sledovat ho by vite jen zdržovalo.
      ignored: ["**/src-tauri/**", "**/target/**", "**/tools/**"],
    },
  },
});
