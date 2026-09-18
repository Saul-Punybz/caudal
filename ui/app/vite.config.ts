import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  base: '/',
  build: {
    outDir: '../../crates/caudal-ui/dist',
    emptyOutDir: true,
  },
  server: {
    fs: {
      // ui/theme/tokens.css lives one level above this app's root.
      allow: ['..'],
    },
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/hls': 'http://127.0.0.1:8080',
      '/play': 'http://127.0.0.1:8080',
      '/moq': 'http://127.0.0.1:8080',
    },
  },
});
