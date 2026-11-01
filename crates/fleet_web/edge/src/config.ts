import { ConfigStore } from "fastly:config-store";
import { env } from "fastly:env";
import { SecretStore } from "fastly:secret-store";
import { parseKeyHex } from "./crypto.ts";

const CONFIG_STORE = "fleet_web_config";
const SECRET_STORE = "fleet_web_secrets";

export class ConfigError extends Error {}

let configStore: ConfigStore | null = null;
let secretStore: SecretStore | null = null;

export function configValue(key: string): string | null {
  configStore ??= new ConfigStore(CONFIG_STORE);
  const value = configStore.get(key);
  return value === null || value.trim() === "" ? null : value.trim();
}

export function requireConfig(key: string): string {
  const value = configValue(key);
  if (value === null) throw new ConfigError(`config store ${CONFIG_STORE} is missing ${key}`);
  return value;
}

export async function requireSecret(key: string): Promise<string> {
  secretStore ??= new SecretStore(SECRET_STORE);
  const entry = await secretStore.get(key);
  if (entry === null) throw new ConfigError(`secret store ${SECRET_STORE} is missing ${key}`);
  return entry.plaintext();
}

export async function secretKey(key: "session_key" | "origin_key"): Promise<Uint8Array> {
  try {
    return parseKeyHex(await requireSecret(key));
  } catch (error) {
    if (error instanceof ConfigError) throw error;
    throw new ConfigError(`${key}: ${(error as Error).message}`);
  }
}

/**
 * True only under Viceroy (`fastly compute serve`), which sets
 * FASTLY_HOSTNAME=localhost; production cache nodes report their own
 * hostname. The request host is checked too so a misreported env alone
 * cannot open the dev bypass on a public domain.
 */
export function isLocalDev(requestUrl: URL): boolean {
  const localHosts = new Set(["localhost", "127.0.0.1", "[::1]"]);
  return env("FASTLY_HOSTNAME") === "localhost" && localHosts.has(requestUrl.hostname);
}

/** The login the dev bypass may mint, or null outside local dev. */
export function devLogin(requestUrl: URL): string | null {
  if (!isLocalDev(requestUrl)) return null;
  return configValue("dev_login");
}
