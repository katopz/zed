// Two-stage build: bundle the client island, embed it in the server bundle
// as a virtual module, then compile the server bundle to bin/main.wasm.
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex, utf8ToBytes } from "@noble/hashes/utils.js";
import { build } from "esbuild";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const shared = { bundle: true, jsx: "automatic", jsxImportSource: "preact", target: "es2022", logLevel: "warning", absWorkingDir: root };

const client = await build({
  ...shared,
  entryPoints: ["src/client/app.tsx"],
  platform: "browser",
  format: "iife",
  minify: true,
  write: false,
});
const appJs = client.outputFiles[0].text;
const appHash = bytesToHex(blake3(utf8ToBytes(appJs))).slice(0, 16);

const appAsset = {
  name: "app-asset",
  setup(builder) {
    builder.onResolve({ filter: /^virtual:app-asset$/ }, () => ({ path: "app-asset", namespace: "virtual" }));
    builder.onLoad({ filter: /.*/, namespace: "virtual" }, () => ({
      contents: `export const APP_JS = ${JSON.stringify(appJs)};\nexport const APP_HASH = ${JSON.stringify(appHash)};\n`,
      loader: "js",
    }));
  },
};

mkdirSync(join(root, "build"), { recursive: true });
mkdirSync(join(root, "bin"), { recursive: true });
await build({
  ...shared,
  entryPoints: ["src/index.ts"],
  platform: "neutral",
  mainFields: ["module", "main"],
  format: "esm",
  outfile: "build/bundle.js",
  external: ["fastly:*"],
  plugins: [appAsset],
});

const runtime = join(root, "node_modules", ".bin", "js-compute-runtime");
execFileSync(runtime, [join(root, "build/bundle.js"), join(root, "bin/main.wasm")], { stdio: "inherit", cwd: root });
console.log(`built bin/main.wasm (client island ${appJs.length} bytes, hash ${appHash})`);
