/**
 * Minimal SemVer 2.0.0 comparison for the updater downgrade guard.
 *
 * The updater plugin already refuses manifests whose version is not
 * greater than the running version, but the Updates panel keeps a
 * pending `Update` object across re-checks and the MSI path reads the
 * raw manifest, so the panel re-checks here instead of trusting one
 * layer. The Rust twin of this logic lives in
 * `src-tauri/src/semver.rs` and the two test suites mirror each
 * other — keep them in sync.
 *
 * Rules (SemVer 2.0.0 §11): core MAJOR.MINOR.PATCH compared
 * numerically (a missing component counts as 0); a leading `v` is
 * stripped; build metadata (`+...`) is ignored; a version WITH a
 * pre-release is lower than the same version without one
 * (`0.18.14-beta` < `0.18.14`); pre-release identifiers compare per
 * spec (numeric numerically and below alphanumeric ones, alphanumeric
 * lexically, shorter list lower when it is a prefix of the longer).
 */

type Ident = { num: number } | { str: string };

interface Parsed {
  core: number[];
  pre: Ident[] | null;
}

function parse(v: string): Parsed {
  let s = v.trim();
  if (s.startsWith("v") || s.startsWith("V")) s = s.slice(1);
  s = s.split("+")[0] ?? s;
  const dash = s.indexOf("-");
  const corePart = dash === -1 ? s : s.slice(0, dash);
  const prePart = dash === -1 ? null : s.slice(dash + 1);
  const core = corePart.split(".").map((p) => {
    const n = /^\d+$/.test(p) ? Number(p) : 0;
    return Number.isSafeInteger(n) ? n : 0;
  });
  const pre =
    prePart === null
      ? null
      : prePart.split(".").map((id): Ident =>
          id !== "" && /^\d+$/.test(id)
            ? { num: Number(id) }
            : { str: id },
        );
  return { core, pre };
}

/** -1 when `a` is older than `b`, 0 when they denote the same release, 1 when `a` is newer. */
export function compareSemver(a: string, b: string): number {
  const pa = parse(a);
  const pb = parse(b);
  const max = Math.max(pa.core.length, pb.core.length);
  for (let i = 0; i < max; i++) {
    const d = (pa.core[i] ?? 0) - (pb.core[i] ?? 0);
    if (d !== 0) return d > 0 ? 1 : -1;
  }
  if (pa.pre === null && pb.pre === null) return 0;
  if (pa.pre === null) return 1; // A release outranks its own pre-release.
  if (pb.pre === null) return -1;
  const x = pa.pre;
  const y = pb.pre;
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    const xi = x[i] as Ident;
    const yi = y[i] as Ident;
    if ("num" in xi && "num" in yi) {
      if (xi.num !== yi.num) return xi.num > yi.num ? 1 : -1;
    } else if ("num" in xi) {
      return -1; // Numeric identifiers have lower precedence than alphanumeric.
    } else if ("num" in yi) {
      return 1;
    } else if (xi.str !== yi.str) {
      return xi.str > yi.str ? 1 : -1;
    }
  }
  return x.length === y.length ? 0 : x.length > y.length ? 1 : -1;
}

/** True when `candidate` is strictly newer than `current`: equal versions and pre-releases of the running version are NOT updates. */
export function isNewerVersion(candidate: string, current: string): boolean {
  return compareSemver(candidate, current) > 0;
}

/** True when `current` is at least `target` (equal counts). */
export function isAtLeastVersion(current: string, target: string): boolean {
  return compareSemver(current, target) >= 0;
}
