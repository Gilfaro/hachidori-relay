// SPDX-License-Identifier: GPL-3.0-or-later
// Not from upstream. An oracle suite skips without a Hachidori checkout, its
// fixtures or jsdom; with HACHIDORI_ORACLES=required (CI) a missing one fails
// instead, so a broken checkout cannot pass as a skip.
export function oracleSkip(missing) {
  if (missing.length === 0) return false;
  const reason = `needs ${missing.join(", ")}`;
  if (process.env.HACHIDORI_ORACLES === "required") throw new Error(`HACHIDORI_ORACLES=required, but the oracle ${reason}`);
  return reason;
}
