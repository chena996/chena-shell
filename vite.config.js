import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

const projectRoot = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [react()],
  base: './',
  clearScreen: false,
  resolve: {
    alias: {
      // monaco-editor 0.57 的 exports 映射会把深层路径拼成重复段，这里直接指向真实目录。
      'monaco-editor/esm/vs': path.resolve(projectRoot, 'node_modules/monaco-editor/esm/vs'),
    },
  },
  worker: {
    format: 'es',
  },
  server: {
    port: 1420,
    strictPort: true,
    host: '127.0.0.1',
  },
  envPrefix: ['VITE_', 'TAURI_ENV_*'],
  build: {
    target: ['es2021', 'chrome100', 'safari13'],
    minify: !process.env.TAURI_ENV_DEBUG ? 'oxc' : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    rolldownOptions: {
      output: {
        assetFileNames: () => 'assets/[name]-[hash][extname]',
        codeSplitting: {
          groups: [
            { name: 'xterm', test: /node_modules[\\/]xterm/ },
            { name: 'react', test: /node_modules[\\/](?:react|react-dom|scheduler)[\\/]/ },
          ],
        },
      },
    },
  },
  test: {
    environment: 'jsdom',
    setupFiles: './src/test-setup.js',
    restoreMocks: true,
  },
});
