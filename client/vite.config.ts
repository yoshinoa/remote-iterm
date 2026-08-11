import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    host: true, // Listen on all addresses, including LAN and public IPs
    port: 7292,
    // Allow any Host header so the app works behind a reverse proxy on a custom
    // domain (Vite otherwise 403s unknown hosts as DNS-rebinding protection).
    allowedHosts: true
  }
})
