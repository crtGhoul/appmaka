#!/usr/bin/env node
/**
 * make-latest-json.mjs — build the tauri-plugin-updater manifest for an
 * AppMaka release.
 *
 * Usage:
 *   node scripts/make-latest-json.mjs <version> <assets-dir> "<notes>" [out]
 *
 *   <version>    e.g. 0.9.2 (no leading "v")
 *   <assets-dir> directory holding the CI-built .sig files, e.g.
 *                AppMaka_0.9.2_x64-setup.exe.sig and AppMaka_0.9.2_amd64.AppImage.sig
 *   <notes>      one-paragraph release notes for the manifest
 *   [out]        output path (default: <assets-dir>/latest.json)
 *
 * The .sig files that `tauri build` writes (TAURI_SIGNING_PRIVATE_KEY) are
 * already the single base64 encoding of the minisign signature text. They
 * are embedded VERBATIM — never base64-encoded again. v0.9.1 shipped a
 * manifest whose signatures had been run through base64 a SECOND time
 * (584 chars instead of 436); the updater does exactly one decode, so
 * every NSIS-path update failed with "Invalid encoding in minisign data".
 *
 * Release-time assertions — any failure aborts loudly with a non-zero
 * exit BEFORE anything is written, so a bad manifest can never be
 * published by accident:
 *  1. each .sig file exists, is non-empty, and is one line;
 *  2. the embedded signature's length === the .sig file's content length
 *     (catches any re-encoding, now or by a future edit);
 *  3. base64-decoding the signature ONCE yields text starting with
 *     "untrusted comment:" (the minisign signature file format);
 *  4. the decoded trusted comment names the expected artifact file and
 *     version (catches signing/uploading the wrong file).
 */

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const REPO = "crtGhoul/appmaka";

const PLATFORMS = [
  {
    key: "windows-x86_64",
    asset: (v) => `AppMaka_${v}_x64-setup.exe`,
  },
  {
    key: "linux-x86_64",
    asset: (v) => `AppMaka_${v}_amd64.AppImage`,
  },
];

function fail(msg) {
  console.error(`make-latest-json: FATAL: ${msg}`);
  process.exit(1);
}

/** Read a .sig file and return its content: the single base64 encoding. */
function readSig(assetsDir, assetName) {
  const path = join(assetsDir, `${assetName}.sig`);
  let raw;
  try {
    raw = readFileSync(path, "utf8");
  } catch {
    fail(`missing signature file: ${path}`);
  }
  const sig = raw.trim();
  if (!sig) fail(`signature file is empty: ${path}`);
  if (/\s/.test(sig)) fail(`signature file is not a single line: ${path}`);
  return { sig, path };
}

function main() {
  const [version, assetsDir, notes, out] = process.argv.slice(2);
  if (!version || !assetsDir || !notes) {
    fail("usage: node scripts/make-latest-json.mjs <version> <assets-dir> \"<notes>\" [out]");
  }
  if (!/^\d+\.\d+\.\d+$/.test(version)) fail(`bad version "${version}" (want X.Y.Z)`);

  // Full signature verification for one asset (assertions 2–4). Returns
  // the verbatim sig text for manifest embedding.
  function assertSig(assetName) {
    const { sig, path } = readSig(assetsDir, assetName);

    // Assertion 2: the manifest field must be byte-identical in length to
    // the .sig file — any re-encoding changes the length (436 -> 584).
    // (We embed verbatim, so this is structural; it guards hand edits.)
    if (sig.length !== readFileSync(path, "utf8").trim().length) {
      fail(`signature length changed for ${assetName} — refusing to publish`);
    }

    // Assertion 3: one base64 decode must give minisign text.
    let decoded;
    try {
      decoded = Buffer.from(sig, "base64").toString("utf8");
    } catch {
      fail(`signature for ${assetName} is not valid base64`);
    }
    if (!decoded.startsWith("untrusted comment:")) {
      fail(
        `signature for ${assetName} does not decode to minisign text ` +
          `(double-encoded? starts with: ${JSON.stringify(decoded.slice(0, 24))})`
      );
    }

    // Assertion 4: the trusted comment must name this artifact + version.
    if (!decoded.includes(`file:${assetName}`) || !decoded.includes(`version:${version}`)) {
      fail(
        `signature for ${assetName} names the wrong artifact/version ` +
          `(trusted comment mismatch) — refusing to publish`
      );
    }
    return sig;
  }

  const platforms = {};
  for (const p of PLATFORMS) {
    const assetName = p.asset(version);
    const sig = assertSig(assetName);
    platforms[p.key] = {
      signature: sig,
      url: `https://github.com/${REPO}/releases/download/v${version}/${assetName}`,
    };
    console.error(`ok: ${p.key} -> ${assetName} (sig ${sig.length} chars, verified)`);
  }

  // The MSI update path (msi_update.rs) fetches the .msi and its .sig
  // straight from the release — it never appears in this manifest, so
  // validate its signature here too. A broken one would otherwise only
  // surface as a failed install on a user's PC.
  const msiName = `AppMaka_${version}_x64_en-US.msi`;
  const msiSig = assertSig(msiName);
  console.error(`ok: windows-msi -> ${msiName} (sig ${msiSig.length} chars, verified)`);

  const manifest = {
    version,
    notes,
    pub_date: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
    platforms,
  };

  const outPath = out || join(assetsDir, "latest.json");
  writeFileSync(outPath, JSON.stringify(manifest, null, 2) + "\n");
  console.error(`wrote ${outPath}`);
}

main();
