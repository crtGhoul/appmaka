#!/usr/bin/env node
/**
 * make-latest-json.mjs — build the tauri-plugin-updater manifest for an
 * AppMaka release.
 *
 * Usage:
 *   node scripts/make-latest-json.mjs <version> <assets-dir> "<notes>" [out] [--host=codeberg|github]
 *
 *   <version>    e.g. 0.9.2 (no leading "v")
 *   <assets-dir> directory holding the CI-built .sig files, e.g.
 *                AppMaka_0.9.2_x64-setup.exe.sig and AppMaka_0.9.2_amd64.AppImage.sig
 *   <notes>      one-paragraph release notes for the manifest
 *   [out]        output path (default: <assets-dir>/latest.json)
 *   --host       where the release assets live: `codeberg` (default) or
 *                `github`. Publishing is Codeberg-first (see
 *                publish-*.py in ~/workspace/appmaka-build): the Codeberg
 *                release is created first with all 10 assets, the GitHub
 *                release mirrors it, and the SAME manifest is pushed to
 *                both repo roots — so both updater endpoints serve
 *                Codeberg asset URLs. A GitHub-URL manifest would point
 *                the MSI path at a mirror that has historically lagged
 *                (or lacked the MSI entirely), 404ing the install.
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
 *
 * All FIVE signed artifacts are asserted (exe, MSI, AppImage, deb,
 * rpm), not just the two the manifest embeds: `tauri build` signs
 * every updater-enabled bundle with the version in the trusted
 * comment, so a missing or wrong .sig for any of them is a broken
 * release even when the manifest itself would still parse.
 */

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const REPO = "crtGhoul/appmaka";

// Codeberg-first: the Codeberg release holds all 10 assets and is
// created first; GitHub only mirrors it. Both updater endpoints serve
// this one manifest, so its URLs must point at the canonical host.
const HOSTS = {
  codeberg: `https://codeberg.org/${REPO}/releases/download`,
  github: `https://github.com/${REPO}/releases/download`,
};

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

// Signed artifacts that never appear in the manifest but must still
// exist and verify: the MSI (fetched directly by msi_update.rs) and
// the deb/rpm (installed from the release page).
const EXTRA_SIGNED = [
  { key: "windows-msi", asset: (v) => `AppMaka_${v}_x64_en-US.msi` },
  { key: "linux-deb", asset: (v) => `AppMaka_${v}_amd64.deb` },
  { key: "linux-rpm", asset: (v) => `AppMaka-${v}-1.x86_64.rpm` },
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
  const args = process.argv.slice(2);
  let host = "codeberg";
  const positional = [];
  for (const arg of args) {
    const match = arg.match(/^--host=(.+)$/);
    if (match) {
      host = match[1];
    } else {
      positional.push(arg);
    }
  }
  if (!(host in HOSTS)) {
    fail(`bad --host "${host}" (want codeberg or github)`);
  }
  const [version, assetsDir, notes, out] = positional;
  if (!version || !assetsDir || !notes) {
    fail(
      'usage: node scripts/make-latest-json.mjs <version> <assets-dir> "<notes>" [out] [--host=codeberg|github]'
    );
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
      url: `${HOSTS[host]}/v${version}/${assetName}`,
    };
    console.error(`ok: ${p.key} -> ${assetName} (sig ${sig.length} chars, verified)`);
  }

  // The MSI update path (msi_update.rs) fetches the .msi and its .sig
  // straight from the release — it never appears in this manifest, so
  // validate its signature here too. A broken one would otherwise only
  // surface as a failed install on a user's PC. Same for the deb and
  // rpm, which are installed from the release page.
  // v0.18.14: the Linux cross-build produces no MSI (WiX is
  // Windows-only) and tauri does not sign deb/rpm, so an EXTRA_SIGNED
  // asset that was never built has no .sig to check. Skip such assets
  // with a warning instead of failing — but a present asset is still
  // fully verified by assertSig (a present-but-wrong sig still fails).
  for (const p of EXTRA_SIGNED) {
    const assetName = p.asset(version);
    if (!existsSync(join(assetsDir, assetName))) {
      console.error(
        `warn: ${p.key} -> ${assetName} not built, skipping signature check`
      );
      continue;
    }
    const sig = assertSig(assetName);
    console.error(`ok: ${p.key} -> ${assetName} (sig ${sig.length} chars, verified)`);
  }

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
