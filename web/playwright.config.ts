import { defineConfig } from "@playwright/test";

// The tests drive a real program with the core and this app
// (examples/dev.rs), on SQLite in a throwaway directory.
const port = Number(process.env.ORTUS_E2E_PORT ?? 7980);
const binary = process.env.ORTUS_E2E_BINARY ?? "../target/debug/examples/dev";
const dnsPort = Number(process.env.ORTUS_E2E_DNS_PORT ?? 5399);

export default defineConfig({
  testDir: "e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [["list"]],
  globalSetup: "./e2e/global-setup.ts",
  use: { baseURL: `http://localhost:${port}`, viewport: { width: 1440, height: 900 }, colorScheme: "dark", storageState: "test-results/owner.json" },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
  webServer: [
    // A test DNS server, answering from e2e/zone.json.
    { command: `rm -f e2e/zone.json && node e2e/dns-server.mjs ${dnsPort}`, wait: { stdout: /listening/ }, reuseExistingServer: false },
    {
      command: `rm -rf test-results/data && ${binary}`,
      env: {
        ORTUS_ROLES: "web,worker",
        ORTUS_WEB_PORT: String(port),
        ORTUS_DATA_DIR: "test-results/data",
        ORTUS_SETUP_TOKEN: "e2e-setup-token",
        ORTUS_PUBLIC_URL: `http://localhost:${port}`,
        ORTUS_DNS_SERVERS: `127.0.0.1:${dnsPort}`,
        ORTUS_MAIL_HOSTNAME: "mail.ministry.example",
      },
      url: `http://127.0.0.1:${port}/api/v1/health`,
      reuseExistingServer: false,
      timeout: 30_000,
    },
  ],
});
