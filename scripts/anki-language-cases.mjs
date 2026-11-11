// SPDX-License-Identifier: GPL-3.0-or-later
// Records the Anki fields the extension's note builder makes in each language
// profile, so the relay's port (src/host/anki/language.rs, glossary.rs,
// values.rs and compact.rs) can be checked against it:
//
//   node scripts/anki-language-cases.mjs ../hachidori
//
// writes src/host/anki/testdata/languages.json: upstream's language scenarios
// (test/theme-languages-fixtures.mjs: Japanese, English, Arabic, Korean and
// Chinese results with dictionaries of both directions) as mining requests,
// rendered by `buildAnkiResourceFields` with the vendored language registry,
// in the scenario's profile and without profiles, as full and as Smaller Anki
// cards. It needs jsdom where Hachidori's tests find it (HACHIDORI_JSDOM, its
// test/tooling, or ~/.cache/hachidori-e2e).

import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import vm from "node:vm";

const source = resolve(process.argv[2] ?? "../hachidori");
const require = createRequire(import.meta.url);
const paths = [process.env.HACHIDORI_JSDOM, join(source, "test/tooling"), join(homedir(), ".cache/hachidori-e2e")].filter(Boolean);
const { JSDOM, VirtualConsole } = require(require.resolve("jsdom", { paths }));

const vendored = {};
vendored.globalThis = vendored;
vm.runInNewContext(readFileSync(join(source, "extension/vendor/languages/registry.js"), "utf8"), vendored);
globalThis.HDLanguages = vendored.HDLanguages;
const { SCENARIOS } = await import(pathToFileURL(join(source, "test/theme-languages-fixtures.mjs")));
const { buildAnkiResourceFields } = await import(pathToFileURL(join(source, "extension/anki-resources.js")));

const field = (value) => ({ value, overwriteMode: "coalesce" });
// Upstream's test/anki-languages.test.mjs Templates, with every glossary
// variant and the single-dictionary markers its cases read.
const TEMPLATES = {
  Front: field("{expression}"), Glossary: field("{glossary}"), Sentence: field("{sentence}"),
  Cloze: field("{cloze-prefix}<b>{cloze-body}</b>{cloze-suffix}"), Frequency: field("{frequencies}"),
  Kind: field("{part-of-speech}"), Furigana: field("{furigana}"), FuriganaPlain: field("{furigana-plain}"),
  Brief: field("{glossary-brief}"), Bare: field("{glossary-no-dictionary}"), Plain: field("{glossary-plain}"),
  First: field("{glossary-first}"), Arabic: field("{single-glossary-kaikki-arabic-english}"),
  English: field("{single-glossary-kaikki-english}"), SentenceFurigana: field("{sentence-furigana}"),
};
const SENTENCES = { ja: "朝ごはんを食べたかった。", en: "I was running late.", ar: "قرأتُ والكتاب أمس.",
  ko: "나는 학교에 갔다.", zh: "我昨天去了学校。" };

// A scenario's first result as the reader sends it once the records have
// profiles: each dictionary's direction and target language beside its name.
function miningRequest(name, languageFields) {
  const scenario = SCENARIOS[name];
  const result = scenario.results[0];
  const sentence = SENTENCES[name].includes(result.matched) ? SENTENCES[name] : `${result.matched}。`;
  const titles = [...new Set(result.term.glossaries.map((row) => row.dictionary))];
  const targets = Object.fromEntries(scenario.dictionaries.filter((item) => item.targetLanguage)
    .map((item) => [item.title, item.targetLanguage]));
  const language = globalThis.HDThemeHost.createLanguage(vendored.HDLanguages.get(name), { marked: true, dictionaries: scenario.dictionaries });
  return { ...result, generation: 1, sentence, matchOffset: sentence.indexOf(result.matched), matched: result.matched,
    searchQuery: result.matched, popupSelectionText: "", documentTitle: "Page", dictionaryAliases: {}, dictionaryIds: {},
    frequencyDictionaries: result.term.frequencies.map((group) => group.dictionary),
    ...(languageFields ? {
      dictionaryDirections: Object.fromEntries(titles.map((title) => [title, language.dictionaryDirection(title, result)])),
      dictionaryTargetLanguages: Object.fromEntries(titles.filter((title) => targets[title]).map((title) => [title, targets[title]])),
    } : {}) };
}

const cases = [];
for (const name of Object.keys(SCENARIOS)) {
  for (const [profile, languageFields] of [[{ language: name, packSettings: {} }, true], [null, true], [null, false]]) {
    for (const compact of [false, true]) {
      const request = miningRequest(name, languageFields);
      const dom = new JSDOM("<!doctype html><html><body></body></html>", { virtualConsole: new VirtualConsole() });
      const { fields } = await buildAnkiResourceFields(request, TEMPLATES, { document: dom.window.document,
        dictionaryPaths: Object.fromEntries(SCENARIOS[name].dictionaries.map((item) => [item.title, `/dicts/${item.title}`])),
        styles: async () => [], compactGlossary: compact, profile });
      dom.window.close();
      cases.push({ name, profile, compact, request, templates: TEMPLATES, fields });
    }
  }
}
const target = new URL("../src/host/anki/testdata/languages.json", import.meta.url);
writeFileSync(target, `${JSON.stringify(cases)}\n`);
console.log(`${cases.length} cases`);
