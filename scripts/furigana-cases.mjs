// Writes src/host/lookup/testdata/furigana.json: headwords and what the extension's
// own kanjiFurigana gives them, for the Rust port's test (furigana.rs). Needs a
// Hachidori checkout: node scripts/furigana-cases.mjs <path to hachidori>.
// SPDX-License-Identifier: GPL-3.0-or-later
import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";

const hachidori = resolve(process.argv[2] ?? "../hachidori");
const require = createRequire(import.meta.url);
const { createKanjiReadings, createKanwaReadings, kanjiFurigana } = require(join(hachidori, "extension/render/glossary.js"));
const kanjidic = JSON.parse(readFileSync(join(hachidori, "extension/vendor/kanjidic/kanji-readings.json"), "utf8"));
const kanwa = JSON.parse(readFileSync(join(hachidori, "extension/vendor/kanwa/kanwa-readings.json"), "utf8"));
const kanjiReadings = createKanjiReadings(kanjidic.readings);
const kanwaReadings = createKanwaReadings(kanwa, kanjiReadings);

// A fixed pseudo-random sequence, so the file only changes with the extension.
let seed = 20261104;
const random = (n) => {
  seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
  return seed % n;
};
const pick = (list) => list[random(list.length)];
const katakana = (text) => text.replace(/[ぁ-ゖ]/gu, (c) => String.fromCharCode(c.charCodeAt(0) + 0x60));

const kanji = Object.keys(kanjidic.readings);
const readingsOf = (k) => kanjidic.readings[k].split(" ").map((r) => r.split("."));
const word = () => {
  const k = pick(kanji);
  const [stem, okurigana = ""] = pick(readingsOf(k));
  return [k + okurigana, stem + okurigana];
};
const cases = new Map();
const add = (expression, reading) => cases.set(`${expression}\u0000${reading}`, [expression, reading]);

for (const [compound, readings] of Object.entries(kanwa.compounds)) {
  for (const reading of readings.split(" ")) add(compound, reading.replaceAll("|", ""));
}
for (let i = 0; i < 3000; i += 1) {
  const parts = Array.from({ length: 1 + random(4) }, word);
  const kana = ["", "の", "お", "ご", "する", "っ"];
  const expression = parts.map(([e]) => e + (random(3) === 0 ? pick(kana) : "")).join("");
  let reading = "";
  for (let at = 0, p = 0; p < parts.length; p += 1) {
    const [e, r] = parts[p];
    reading += r;
    at += e.length;
    const tail = expression.slice(at).match(/^[぀-ヿ]*/u)[0];
    reading += tail;
    at += tail.length;
  }
  add(expression, reading);
  if (random(5) === 0) add(expression, katakana(reading));
  if (random(7) === 0) add(expression, reading.slice(0, -1));
  if (random(9) === 0) add(expression, reading.replace(/^./u, (c) => String.fromCharCode(c.charCodeAt(0) + 1)));
}
for (const [expression, reading] of [["々", "どう"], ["人々", "ひとびと"], ["時々", "ときどき"], ["𠮟る", "しかる"], ["ねこ", ""],
  ["食べる", "たべる"], ["ＰＩ", "ぴ"], ["２人", "ふたり"], ["", ""], ["猫", "ネコ"], ["カタカナ", "かたかな"]]) add(expression, reading);

const out = [...cases.values()].map(([expression, reading]) => [expression, reading, kanjiFurigana(expression, reading, kanjiReadings, kanwaReadings)]);
mkdirSync("src/host/lookup/testdata", { recursive: true });
writeFileSync("src/host/lookup/testdata/furigana.json", `${JSON.stringify(out)}\n`);
console.log(out.length, "cases,", out.filter((c) => c[2] !== null).length, "with furigana");

// Tag names in the order the extension's Intl.Collator("en-US") sorts them (tags.rs).
const names = new Set(["n", "v5r", "vt", "P", "news1", "ichi1", "spec2", "uk", "arch", "★", "⭐", "1", "10", "2", "a", "A", "á", "B", "_x", "-x",
  "x-y", "xy", "Xy", "名", "動詞", "かな", "カナ", "ｶﾅ", "ok", "Ok", "OK", "e.g.", "e", "é", "ß", "ss", "", "😀", "z:1", "z:2", "z"]);
for (let i = 0; i < 400; i += 1) {
  const alphabet = "abcAB_-.:0129éÉßあアｱ名★ ";
  names.add(Array.from({ length: 1 + random(5) }, () => alphabet[random(alphabet.length)]).join("").trim() || "q");
}
const collator = new Intl.Collator("en-US");
writeFileSync("src/host/lookup/testdata/tag-order.json", `${JSON.stringify([...names].sort(collator.compare))}\n`);
