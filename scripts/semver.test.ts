// Unit tests for src/semver.ts — the updater downgrade guard.
// Run with: npm test   (node --test, no extra dependencies)
// Mirrors src-tauri/src/semver.rs tests; keep the two in sync.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  compareSemver,
  isAtLeastVersion,
  isNewerVersion,
} from "../src/semver.ts";

test("equal versions are not newer", () => {
  assert.equal(compareSemver("0.18.13", "0.18.13"), 0);
  assert.equal(isNewerVersion("0.18.13", "0.18.13"), false);
  assert.equal(compareSemver("v0.18.13", "0.18.13"), 0);
  assert.equal(compareSemver("0.18", "0.18.0"), 0);
  assert.equal(isAtLeastVersion("0.18.13", "0.18.13"), true);
});

test("newer and older", () => {
  assert.equal(isNewerVersion("0.18.14", "0.18.13"), true);
  assert.equal(isNewerVersion("0.18.13", "0.18.14"), false);
  assert.equal(isNewerVersion("0.19.0", "0.18.13"), true);
  assert.equal(isNewerVersion("1.0.0", "0.18.13"), true);
  assert.equal(isNewerVersion("0.18.12", "0.18.13"), false);
});

test("downgrade guard never offers equal or older", () => {
  // The guard's whole job: <= current is never an update.
  for (const candidate of ["0.18.13", "0.18.12", "0.18.0", "0.17.99"]) {
    assert.equal(
      isNewerVersion(candidate, "0.18.13"),
      false,
      `${candidate} must not count as newer than 0.18.13`,
    );
  }
});

test("pre-release is older than its release", () => {
  assert.equal(compareSemver("0.18.14-beta", "0.18.14"), -1);
  assert.equal(isNewerVersion("0.18.14-beta", "0.18.14"), false);
  // ...but a pre-release of a NEWER version still is one.
  assert.equal(isNewerVersion("0.18.14-beta", "0.18.13"), true);
  assert.equal(isNewerVersion("0.18.13-beta", "0.18.13"), false);
  // A pre-release running build must not retire a note for the release.
  assert.equal(isAtLeastVersion("0.18.14-beta", "0.18.14"), false);
});

test("pre-release identifier ordering", () => {
  // SemVer 2.0.0 §11 example ordering.
  const ordered = [
    "1.0.0-alpha",
    "1.0.0-alpha.1",
    "1.0.0-alpha.beta",
    "1.0.0-beta",
    "1.0.0-beta.2",
    "1.0.0-beta.11",
    "1.0.0-rc.1",
    "1.0.0",
  ];
  for (let i = 0; i < ordered.length - 1; i++) {
    assert.equal(
      compareSemver(ordered[i] as string, ordered[i + 1] as string),
      -1,
      `${ordered[i]} should be older than ${ordered[i + 1]}`,
    );
  }
  // Numeric identifiers compare numerically, not lexically.
  assert.equal(compareSemver("1.0.0-beta.2", "1.0.0-beta.11"), -1);
});

test("build metadata is ignored", () => {
  assert.equal(compareSemver("0.18.13+build.5", "0.18.13"), 0);
  assert.equal(isNewerVersion("0.18.13+build.5", "0.18.13"), false);
});
