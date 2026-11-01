// Writes local/secrets.json for Viceroy if it is missing: a fresh session key,
// and the origin key from ~/.agent-mail/web/origin.key when that exists so a
// real local `fleet-web serve` accepts the edge's signatures.
import { randomBytes } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const target = join(dirname(fileURLToPath(import.meta.url)), "..", "local", "secrets.json");
if (existsSync(target)) {
  console.log(`${target} already exists; leaving it alone`);
  process.exit(0);
}
const originKeyPath = join(homedir(), ".agent-mail", "web", "origin.key");
const originKey = existsSync(originKeyPath) ? readFileSync(originKeyPath, "utf8").trim() : randomBytes(32).toString("hex");
const secrets = {
  github_client_secret: "replace-me",
  session_key: randomBytes(32).toString("hex"),
  origin_key: originKey,
};
writeFileSync(target, JSON.stringify(secrets, null, 2) + "\n", { mode: 0o600 });
console.log(`wrote ${target} (origin_key from ${existsSync(originKeyPath) ? originKeyPath : "random"})`);
